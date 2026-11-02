//! Reads model calls out of a Claude Code session transcript (`<session>.jsonl`,
//! or `<session>/subagents/agent-<id>.jsonl` for a subagent). Claude Code writes
//! each API response as one `assistant` record per content block, all sharing
//! `message.id`; every record becomes an update of that call's `llm` span. The
//! format is undocumented, so records that don't fit are skipped.

use std::ops::Range;

use serde_json::{Map, Value};

use crate::model::{Kind, SpanUpdate};

/// Characters of response text kept as a span's output.
const MAX_OUTPUT: usize = 20_000;
/// Requests and responses are only referred to by the records right after them.
const RECENT: usize = 4;

/// Parse state for one transcript file, fed the bytes appended to it.
pub struct Transcript {
    parent: String,
    /// An incomplete last line, finished by a later feed.
    partial: Vec<u8>,
    /// Assistant records carry no promptId; they belong to the latest user record's turn.
    prompt_id: Option<String>,
    /// When recent `api-request` records were sent, by id.
    requests: Vec<(String, f64)>,
    calls: Vec<Call>,
}

/// What has been seen so far of one model call.
struct Call {
    id: String,
    start: f64,
    end: f64,
    text: String,
    clipped: bool,
    tool_use_ids: Vec<Value>,
}

impl Call {
    fn push_text(&mut self, text: &str) {
        if self.clipped || text.is_empty() {
            return;
        }
        if !self.text.is_empty() {
            self.text.push_str("\n\n");
        }
        self.text.push_str(text);
        if let Some((i, _)) = self.text.char_indices().nth(MAX_OUTPUT) {
            self.text.truncate(i);
            self.text.push('…');
            self.clipped = true;
        }
    }
}

impl Transcript {
    /// `parent` is the span the model calls hang under: `main`, or the subagent's id.
    pub fn new(parent: impl Into<String>) -> Self {
        Transcript { parent: parent.into(), partial: Vec::new(), prompt_id: None, requests: Vec::new(), calls: Vec::new() }
    }

    /// Takes bytes appended to the file; an incomplete last line waits for the next feed.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<SpanUpdate> {
        self.partial.extend_from_slice(bytes);
        let Some(last) = self.partial.iter().rposition(|&b| b == b'\n') else { return Vec::new() };
        let lines: Vec<u8> = self.partial.drain(..=last).collect();
        let mut out: Vec<SpanUpdate> = Vec::new();
        for line in lines.split(|&b| b == b'\n') {
            let Some(u) = self.record(line) else { continue };
            // An update carries everything seen of its call, so it replaces the one before.
            if out.last().is_some_and(|prev| prev.trace_id == u.trace_id && prev.span_id == u.span_id) {
                out.pop();
            }
            out.push(u);
        }
        out
    }

    fn record(&mut self, line: &[u8]) -> Option<SpanUpdate> {
        let r: Value = serde_json::from_slice(line).ok()?;
        match r.get("type")?.as_str()? {
            "user" => {
                if let Some(p) = r.get("promptId").and_then(Value::as_str) {
                    self.prompt_id = Some(p.to_string());
                }
                None
            }
            "api-request" => {
                let sent = epoch_ms(r.get("timestamp")?.as_str()?)?;
                remember(&mut self.requests, (r.get("id")?.as_str()?.to_string(), sent));
                None
            }
            "assistant" => self.response_chunk(&r),
            _ => None,
        }
    }

    fn response_chunk(&mut self, r: &Value) -> Option<SpanUpdate> {
        let trace_id = self.prompt_id.clone()?;
        let time = epoch_ms(r.get("timestamp")?.as_str()?)?;
        let message = r.get("message")?;
        let id = message.get("id")?.as_str()?;
        let model = message.get("model").and_then(Value::as_str);
        // Claude Code makes these up locally (interruptions, API errors); no model ran.
        if model == Some("<synthetic>") {
            return None;
        }
        let request = r.get("requestRef").and_then(Value::as_str);
        let sent = request.and_then(|rr| self.requests.iter().find(|(id, _)| id == rr)).map(|&(_, t)| t);

        if !self.calls.iter().any(|c| c.id == id) {
            let call = Call { id: id.to_string(), start: time, end: time, text: String::new(), clipped: false, tool_use_ids: Vec::new() };
            remember(&mut self.calls, call);
        }
        let call = self.calls.iter_mut().find(|c| c.id == id)?;
        call.start = call.start.min(sent.unwrap_or(time));
        call.end = call.end.max(time);
        for block in message.get("content").and_then(Value::as_array).into_iter().flatten() {
            // Thinking blocks are left out on purpose.
            match block.get("type").and_then(Value::as_str) {
                Some("text") => call.push_text(block.get("text").and_then(Value::as_str).unwrap_or_default()),
                Some("tool_use") => {
                    if let Some(tool) = block.get("id").filter(|v| v.is_string() && !call.tool_use_ids.contains(v)) {
                        call.tool_use_ids.push(tool.clone());
                    }
                }
                _ => {}
            }
        }

        let usage = |k: &str| message.get("usage").and_then(|u| u.get(k)).and_then(Value::as_u64);
        let input = ["input_tokens", "cache_read_input_tokens", "cache_creation_input_tokens"].map(|k| (k, usage(k)));
        let mut attrs: Map<String, Value> = input.iter().filter_map(|&(k, n)| Some((k.to_string(), n?.into()))).collect();
        if let Some(v) = r.get("requestId").filter(|v| v.is_string()) {
            attrs.insert("request_id".into(), v.clone());
        }
        if let Some(v) = message.get("stop_reason").filter(|v| v.is_string()) {
            attrs.insert("stop_reason".into(), v.clone());
        }
        if !call.tool_use_ids.is_empty() {
            attrs.insert("tool_use_ids".into(), Value::Array(call.tool_use_ids.clone()));
        }
        Some(SpanUpdate {
            parent_id: Some(self.parent.clone()),
            kind: Some(Kind::Llm),
            name: model.map(str::to_string),
            model: model.map(str::to_string),
            start: Some(call.start),
            end: Some(call.end),
            output: (!call.text.is_empty()).then(|| Value::from(call.text.clone())),
            tokens_in: input.into_iter().filter_map(|(_, n)| n).reduce(u64::saturating_add),
            tokens_out: usage("output_tokens"),
            attrs,
            ..SpanUpdate::new(trace_id, id)
        })
    }
}

fn remember<T>(recent: &mut Vec<T>, item: T) {
    if recent.len() == RECENT {
        recent.remove(0);
    }
    recent.push(item);
}

/// Epoch milliseconds of an RFC 3339 time such as `2026-01-15T10:00:00.123Z`.
fn epoch_ms(s: &str) -> Option<f64> {
    let (date, time) = s.split_once('T')?;
    let (clock, offset_minutes) = match time.strip_suffix(['Z', 'z']) {
        Some(clock) => (clock, 0),
        None => {
            let sign = time.rfind(['+', '-'])?;
            let (h, m) = time[sign + 1..].split_once(':')?;
            let minutes = number(h, 0..24)? * 60 + number(m, 0..60)?;
            (&time[..sign], if time[sign..].starts_with('-') { -minutes } else { minutes })
        }
    };
    let ([y, mo, d], [h, mi, sec]) = (parts(date, '-')?, parts(clock, ':')?);
    let (sec, fraction) = sec.split_once('.').unwrap_or((sec, ""));
    if !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let millis = number(&format!("{fraction:0<3}")[..3], 0..1000)?;
    let days = days_from_civil(number(y, 0..10_000)?, number(mo, 1..13)?, number(d, 1..32)?);
    let minutes = (days * 24 + number(h, 0..24)?) * 60 + number(mi, 0..60)? - offset_minutes;
    Some(((minutes * 60 + number(sec, 0..61)?) * 1000 + millis) as f64)
}

fn parts(s: &str, sep: char) -> Option<[&str; 3]> {
    let mut it = s.split(sep);
    let three = [it.next()?, it.next()?, it.next()?];
    it.next().is_none().then_some(three)
}

/// Plain digits only; `str::parse` would also take a sign.
fn number(s: &str, range: Range<i64>) -> Option<i64> {
    let n = s.bytes().all(|b| b.is_ascii_digit()).then(|| s.parse().ok())??;
    range.contains(&n).then_some(n)
}

/// Days since 1970-01-01 (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let year_of_era = y - era * 400;
    let day_of_year = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    era * 146_097 + year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year - 719_468
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::model::{Kind, Span};
    use crate::store::Store;

    const MAIN: &str = include_str!("../fixtures/claude-code-transcript/sess-t.jsonl");
    const SUBAGENT: &str = include_str!("../fixtures/claude-code-transcript/sess-t/subagents/agent-a1.jsonl");

    /// Milliseconds after 2026-01-15T10:00:00Z, where the fixtures start.
    fn at(ms: f64) -> f64 {
        1_768_471_200_000.0 + ms
    }

    /// Feeds `text` in pieces of `step` bytes and returns the merged spans, sequence numbers cleared.
    fn spans(parent: &str, text: &str, step: usize) -> Vec<Span> {
        let store = Store::in_memory();
        let mut transcript = Transcript::new(parent);
        for piece in text.as_bytes().chunks(step) {
            store.ingest(transcript.feed(piece), 0.0).unwrap();
        }
        let spans = ["p1", "p2"].into_iter().filter_map(|p| store.trace(p)).flatten();
        spans.map(|s| Span { seq: 0, ..s }).collect()
    }

    #[test]
    fn one_llm_span_per_model_call() {
        let updates = Transcript::new("main").feed(MAIN.as_bytes());
        let ids: Vec<(&str, &str)> = updates.iter().map(|u| (u.trace_id.as_str(), u.span_id.as_str())).collect();
        // A response's chunks collapse into one update per read.
        assert_eq!(ids, vec![("p1", "msg_01"), ("p1", "msg_02"), ("p2", "msg_03")]);

        let first = &updates[0];
        assert_eq!(first.kind, Some(Kind::Llm));
        assert_eq!((first.parent_id.as_deref(), first.name.as_deref(), first.model.as_deref()), (Some("main"), Some("claude-opus-5-5"), Some("claude-opus-5-5")));
        assert_eq!((first.tokens_in, first.tokens_out), (Some(12 + 4000 + 300), Some(85)));
        assert_eq!((first.start, first.end), (Some(at(200.0)), Some(at(3000.0))), "from the request to the last chunk");
        assert_eq!(first.output, Some(Value::from("I'll ask a subagent to look — it knows the files.")));
        assert_eq!(first.attrs["request_id"], "req_011");
        assert_eq!(first.attrs["stop_reason"], "tool_use");
        assert_eq!(first.attrs["tool_use_ids"], json!(["toolu_agent"]));
        let cache = ["input_tokens", "cache_read_input_tokens", "cache_creation_input_tokens"].map(|k| first.attrs[k].as_u64());
        assert_eq!(cache, [Some(12), Some(4000), Some(300)]);

        let second = &updates[1];
        assert_eq!((second.start, second.end, second.tokens_in), (Some(at(9100.0)), Some(at(10_000.0)), Some(5 + 4300 + 120)));
        assert_eq!(second.attrs["stop_reason"], "end_turn");

        // No request record: it starts at its first chunk. Only thinking and a tool call: no output.
        let third = &updates[2];
        assert_eq!((third.start, third.end, third.output.as_ref()), (Some(at(61_500.0)), Some(at(62_000.0)), None));
        assert_eq!(third.attrs["tool_use_ids"], json!(["toolu_bash"]));

        let all = serde_json::to_string(&updates).unwrap();
        assert!(!all.contains("PRIVATE-THOUGHT"), "thinking stays out of the spans");
    }

    #[test]
    fn subagent_calls_hang_under_the_agent_in_the_parent_turn() {
        let spans = spans("a1", SUBAGENT, SUBAGENT.len());
        let ids: Vec<&str> = spans.iter().map(|s| s.span_id.as_str()).collect();
        assert_eq!(ids, ["msg_s1", "msg_s2"]);
        assert!(spans.iter().all(|s| s.trace_id == "p1" && s.parent_id.as_deref() == Some("a1") && s.kind == Kind::Llm));
        assert_eq!((spans[1].start, spans[1].output.as_ref()), (at(5200.0), Some(&Value::from("The launch code is 42."))));
    }

    #[test]
    fn feeding_in_any_pieces_gives_the_same_spans() {
        let whole = spans("main", MAIN, MAIN.len());
        assert_eq!(whole.len(), 3);
        // One byte at a time also splits the multi-byte dash in msg_01's text.
        for step in [1, 7, 333] {
            assert_eq!(spans("main", MAIN, step), whole, "{step}-byte pieces");
        }
        let mut t = Transcript::new("main");
        let (newline, rest) = MAIN.as_bytes().split_last().unwrap();
        assert_eq!(t.feed(rest).last().unwrap().end, Some(at(61_500.0)), "a last line without its newline waits");
        let finished = t.feed(&[*newline]);
        assert_eq!((finished.len(), finished[0].end), (1, Some(at(62_000.0))));
    }

    #[test]
    fn skips_records_it_cannot_use() {
        let lines = [
            r#"{"type":"assistant","timestamp":"2026-01-15T10:00:00Z","message":{"id":"before-any-prompt","model":"m","content":[]}}"#,
            "not json",
            "[1,2]",
            r#"{"type":"user","promptId":"p1","timestamp":"2026-01-15T10:00:00Z"}"#,
            r#"{"type":"assistant","message":{"id":"no-time","model":"m","content":[]}}"#,
            r#"{"type":"assistant","timestamp":"yesterday","message":{"id":"bad-time","model":"m","content":[]}}"#,
            r#"{"type":"assistant","timestamp":"2026-01-15T10:00:00Z","message":{"model":"m","content":[]}}"#,
            r#"{"type":"assistant","timestamp":"2026-01-15T10:00:00Z","message":{"id":"local","model":"<synthetic>","content":[{"type":"text","text":"No response requested."}]}}"#,
            r#"{"type":"assistant","timestamp":"2026-01-15T10:00:01Z","message":{"id":"odd","content":"text","usage":{"output_tokens":"many"}}}"#,
        ];
        let updates = Transcript::new("main").feed(format!("{}\n", lines.join("\n")).as_bytes());
        let ids: Vec<&str> = updates.iter().map(|u| u.span_id.as_str()).collect();
        assert_eq!(ids, ["odd"], "an odd shape still marks the call");
        assert_eq!((updates[0].tokens_out, updates[0].model.as_ref(), updates[0].end), (None, None, Some(at(1000.0))));
    }

    #[test]
    fn long_output_is_clipped() {
        let chunk = |text: &str| json!({"type": "assistant", "timestamp": "2026-01-15T10:00:00Z", "message": {"id": "m", "content": [{"type": "text", "text": text}]}});
        let lines = [json!({"type": "user", "promptId": "p"}), chunk(&"é".repeat(15_000)), chunk(&"z".repeat(15_000)), chunk("after")];
        let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
        let output = Transcript::new("main").feed(text.as_bytes()).pop().unwrap().output.unwrap();
        let output = output.as_str().unwrap();
        assert_eq!(output.chars().count(), MAX_OUTPUT + 1);
        assert!(output.starts_with('é') && output.ends_with("z…") && !output.contains("after"));
    }

    #[test]
    fn reads_rfc3339_times() {
        assert_eq!(epoch_ms("2026-01-15T10:00:00.000Z"), Some(at(0.0)));
        assert_eq!(epoch_ms("2000-03-01T00:00:00Z"), Some(951_868_800_000.0));
        assert_eq!(epoch_ms("1969-12-31T23:59:59Z"), Some(-1000.0));
        assert_eq!(epoch_ms("2026-01-15T05:00:00.25-05:00"), Some(at(250.0)));
        assert_eq!(epoch_ms("2026-01-15T10:00:00.123456789+00:00"), Some(at(123.0)));
        for bad in ["", "2026-01-15", "2026-01-15T10:00:00", "2026-13-01T00:00:00Z", "99999999999-01-01T00:00:00Z", "2026-01-15T10:00:xxZ", "2026-01-15T10:00:00.1é3Z"] {
            assert_eq!(epoch_ms(bad), None, "{bad}");
        }
    }
}
