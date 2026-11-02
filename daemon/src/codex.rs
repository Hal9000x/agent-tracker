//! Maps OpenAI Codex CLI hook events (posted by command hooks that pipe their
//! stdin to curl) onto spans. Codex's payloads are shaped like Claude Code's, so
//! each one is translated, handed to that adapter, and its spans relabelled.
//! Hooks don't report model calls; those come from the session's rollout file
//! under `~/.codex/sessions`, read a little further on every event: one `llm`
//! span per response, with its reasoning and assistant messages as children.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{Map, Value};

use crate::claude_code::ClaudeCode;
use crate::model::{Kind, SpanUpdate, Status};

// The span ids the Claude Code adapter gives a turn's prompt and main agent.
const PROMPT: &str = "prompt";
const MAIN: &str = "main";
/// Longest message or reasoning text kept, in characters.
const MAX_TEXT: usize = 20_000;

pub struct Codex {
    hooks: ClaudeCode,
    /// Rollouts are only read from inside this folder; None turns reading off.
    sessions_dir: Option<PathBuf>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    rollouts: HashMap<PathBuf, Rollout>,
    sessions: HashMap<String, Session>,
}

#[derive(Default)]
struct Session {
    /// Root turns whose prompt arrived with UserPromptSubmit.
    prompted: HashSet<String>,
    /// For each tool call id: the trace of its hook span, and the response that made the call.
    calls: HashMap<String, (Option<String>, Option<String>)>,
}

impl Session {
    /// Links a tool span to the response that called it, once both halves are
    /// known. Hooks and the rollout report them in either order.
    fn join(&mut self, call: &str, trace: Option<&str>, response: Option<&str>) -> Option<SpanUpdate> {
        let (t, r) = self.calls.entry(call.to_string()).or_default();
        let linked = t.is_some() && r.is_some();
        if t.is_none() {
            *t = trace.map(str::to_string);
        }
        if r.is_none() {
            *r = response.map(str::to_string);
        }
        match (t, r) {
            (Some(t), Some(r)) if !linked => Some(SpanUpdate { links: vec![r.clone()], ..SpanUpdate::new(t.clone(), call) }),
            _ => None,
        }
    }
}

impl Default for Codex {
    fn default() -> Self {
        let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
        Codex::new(home.map(|h| PathBuf::from(h).join(".codex").join("sessions")))
    }
}

impl Codex {
    pub fn new(sessions_dir: Option<PathBuf>) -> Codex {
        Codex { hooks: ClaudeCode::default(), sessions_dir, state: Mutex::default() }
    }

    /// `known` is the store lookup the Claude Code adapter takes. Rollout spans
    /// only go into traces that exist, so a resumed session's old turns don't
    /// come back as traces of their own.
    pub fn handle(&self, ev: &Value, now: f64, known: impl Fn(&str) -> bool) -> Vec<SpanUpdate> {
        let s = |k: &str| ev.get(k).and_then(Value::as_str);
        let Some(session_id) = s("session_id") else { return vec![] };
        let event = s("hook_event_name").unwrap_or_default();
        let mut out = match to_claude_code(ev) {
            Some(translated) => self.hooks.handle(&translated, now, &known),
            None => Vec::new(),
        };
        relabel(&mut out, ev);

        let mut state = self.state.lock().unwrap();
        let State { rollouts, sessions } = &mut *state;
        let session = sessions.entry(session_id.to_string()).or_default();
        if event == "UserPromptSubmit" && ev.get("agent_id").is_none() && let Some(turn) = s("turn_id") {
            session.prompted.insert(turn.to_string());
        }
        let mut extra = Vec::new();
        for u in out.iter().filter(|u| u.kind == Some(Kind::Tool)) {
            extra.extend(session.join(&u.span_id, Some(&u.trace_id), None));
        }

        let fresh: HashSet<&str> = out.iter().map(|u| u.trace_id.as_str()).collect();
        let live = |trace: &str| fresh.contains(trace) || known(trace);
        let parsed = read_rollouts(rollouts, self.sessions_dir.as_deref(), ev, session_id);
        extra.extend(parsed.spans.into_iter().filter(|u| live(&u.trace_id)));
        for (call, response) in parsed.calls {
            extra.extend(session.join(&call, None, Some(&response)));
        }
        for (turn, text) in parsed.prompts {
            if live(&turn) && !session.prompted.contains(&turn) {
                extra.push(SpanUpdate { name: Some(title(&text)), input: Some(text.into()), ..SpanUpdate::new(turn, PROMPT) });
            }
        }
        if event == "SessionEnd" {
            sessions.remove(session_id);
        }
        out.extend(extra);
        out
    }
}

/// What the rollouts this event names gained since their last read. SessionEnd
/// sweeps every rollout of the session, because Stop hooks run before Codex
/// flushes the turn's last response to disk.
fn read_rollouts(rollouts: &mut HashMap<PathBuf, Rollout>, dir: Option<&Path>, ev: &Value, session: &str) -> Parsed {
    let mut out = Parsed::default();
    let Some(dir) = dir else { return out };
    let s = |k: &str| ev.get(k).and_then(Value::as_str);
    let mut paths: Vec<PathBuf> =
        [s("transcript_path"), s("agent_transcript_path")].into_iter().flatten().filter_map(|p| rollout_path(dir, p)).collect();
    let ending = s("hook_event_name") == Some("SessionEnd");
    if ending {
        paths.extend(rollouts.iter().filter(|(_, r)| r.session == session).map(|(p, _)| p.clone()));
    }
    paths.sort();
    paths.dedup();
    for path in paths {
        let rollout = rollouts.entry(path.clone()).or_insert_with(|| Rollout { session: session.to_string(), ..Default::default() });
        match rollout.read(&path) {
            Ok(parsed) => out.extend(parsed),
            Err(e) => eprintln!("cannot read Codex rollout {}: {e}", path.display()),
        }
    }
    if ending {
        rollouts.retain(|_, r| r.session != session);
    }
    out
}

/// Translates a Codex hook payload into the Claude Code shape, or None for an
/// event that adapter must not see.
fn to_claude_code(ev: &Value) -> Option<Value> {
    let mut ev = ev.as_object()?.clone();
    let in_subagent = ev.contains_key("agent_id");
    match ev.get("hook_event_name").and_then(Value::as_str).unwrap_or_default() {
        // It fires before any prompt, and the adapter would open an empty trace for it.
        "SessionStart" => return None,
        // A subagent's task arrives as a prompt of its own, but it belongs to
        // the parent's trace and the spawn_agent call already carries it.
        "UserPromptSubmit" if in_subagent => return None,
        "Interrupt" => {
            ev.insert("hook_event_name".into(), "StopFailure".into());
            ev.insert("error".into(), "interrupted".into());
        }
        _ => {}
    }
    // One trace per root turn. A subagent's turn ids are its own and never name one.
    if !in_subagent && let Some(turn) = ev.get("turn_id").cloned() {
        ev.insert("prompt_id".into(), turn);
    }
    // Codex's subagent tool (`spawn_agent`, namespaced in newer multi-agent
    // modes) plays the part of Claude Code's `Agent` tool.
    if ev.get("tool_name").and_then(Value::as_str).is_some_and(|t| t.ends_with("spawn_agent")) {
        ev.insert("tool_name".into(), "Agent".into());
        if let Some(Value::Object(input)) = ev.get_mut("tool_input") {
            for (from, to) in [("message", "prompt"), ("agent_type", "subagent_type"), ("task_name", "description")] {
                if let Some(v) = input.get(from).cloned() {
                    input.entry(to).or_insert(v);
                }
            }
        }
    }
    Some(Value::Object(ev))
}

/// Makes the Claude Code adapter's spans say Codex, and gives the tool span
/// back the name and input that `to_claude_code` rewrote.
fn relabel(updates: &mut [SpanUpdate], ev: &Value) {
    let tool_use_id = ev.get("tool_use_id").and_then(Value::as_str);
    for u in updates {
        if u.span_id == PROMPT && u.attrs.contains_key("source") {
            u.attrs.insert("source".into(), "codex".into());
        }
        if u.span_id == MAIN && u.name.as_deref() == Some("Claude Code") {
            u.name = Some("Codex".into());
        }
        if u.agent.as_deref() == Some("claude-code") {
            u.agent = Some("codex".into());
        }
        if Some(u.span_id.as_str()) == tool_use_id {
            if u.name.is_some() {
                u.name = ev.get("tool_name").and_then(Value::as_str).map(str::to_string);
            }
            if u.input.is_some() {
                u.input = ev.get("tool_input").cloned();
            }
        }
    }
}

/// Hook payloads are untrusted, so only `.jsonl` files inside the sessions
/// folder are read. Older, compressed `.jsonl.zst` rollouts are skipped.
fn rollout_path(dir: &Path, path: &str) -> Option<PathBuf> {
    let path = fs::canonicalize(path).ok()?;
    let inside = path.starts_with(fs::canonicalize(dir).ok()?);
    (inside && path.extension().is_some_and(|e| e == "jsonl")).then_some(path)
}

#[derive(Default)]
struct Rollout {
    /// The hook session that named this file, for the SessionEnd sweep.
    session: String,
    /// Bytes parsed so far, always the end of a complete line.
    offset: u64,
    parser: Parser,
}

impl Rollout {
    fn read(&mut self, path: &Path) -> io::Result<Parsed> {
        let mut file = File::open(path)?;
        if file.metadata()?.len() < self.offset {
            // Rewritten rather than appended to: start over.
            *self = Rollout { session: std::mem::take(&mut self.session), ..Default::default() };
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;
        // A line still being written is left for the next read.
        let complete = buf.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        self.offset += complete as u64;
        Ok(self.parser.parse(&String::from_utf8_lossy(&buf[..complete])))
    }
}

#[derive(Debug, Default, PartialEq)]
struct Parsed {
    /// Each response's `llm` span followed by its reasoning and message spans.
    spans: Vec<SpanUpdate>,
    /// (tool call id, id of the response that made the call).
    calls: Vec<(String, String)>,
    /// (root turn id, what the user typed).
    prompts: Vec<(String, String)>,
}

impl Parsed {
    fn extend(&mut self, other: Parsed) {
        self.spans.extend(other.spans);
        self.calls.extend(other.calls);
        self.prompts.extend(other.prompts);
    }
}

/// Turns rollout lines into spans. A `token_usage_record` closes the response
/// whose output items precede it. That response was requested once its inputs
/// were ready: the prompt, or the last tool output.
#[derive(Default)]
struct Parser {
    /// The file's own thread; records inherited from a parent's history are skipped.
    thread: Option<String>,
    /// Whether this is the session's root thread, whose prompts are the user's.
    root: bool,
    model: Option<String>,
    ready_at: Option<f64>,
    response: Response,
    /// What item_completed just reported: (item id, start, end). Its response_item follows.
    completed: Option<(String, f64, f64)>,
    prompted: Option<String>,
}

/// The response being streamed.
#[derive(Default)]
struct Response {
    /// Set by its first output item.
    started_at: Option<f64>,
    /// Where the next item starts when Codex didn't time it.
    last_end: Option<f64>,
    text: Vec<String>,
    /// Reasoning and message spans, given their ids when the response's record arrives.
    items: Vec<SpanUpdate>,
    calls: Vec<String>,
}

impl Parser {
    fn parse(&mut self, lines: &str) -> Parsed {
        let mut out = Parsed::default();
        for line in lines.lines() {
            let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
            let at = v.get("timestamp").and_then(Value::as_str).and_then(epoch_ms);
            let p = &v["payload"];
            let s = |k: &str| p.get(k).and_then(Value::as_str);
            match v["type"].as_str().unwrap_or_default() {
                "session_meta" => {
                    self.thread = s("id").map(str::to_string);
                    self.root = s("session_id").is_none_or(|id| Some(id) == s("id"));
                }
                "turn_context" => self.model = s("model").map(str::to_string),
                "event_msg" if s("type") == Some("task_started") => self.next_response(at),
                "event_msg" if s("type") == Some("item_completed") => self.item_completed(p, &mut out),
                "response_item" => self.response_item(p, at),
                "token_usage_record" => {
                    self.record(p, at, &mut out);
                    self.next_response(at);
                }
                _ => {}
            }
        }
        out
    }

    fn next_response(&mut self, at: Option<f64>) {
        self.ready_at = at;
        self.response = Response::default();
        self.completed = None;
    }

    fn item_completed(&mut self, p: &Value, out: &mut Parsed) {
        let item = &p["item"];
        let ms = |k: &str| p.get(k).and_then(Value::as_f64);
        if item["type"] == "UserMessage" {
            let Some(turn) = p["turn_id"].as_str().filter(|t| self.root && self.prompted.as_deref() != Some(*t)) else { return };
            // Only a turn's first message is its prompt; later ones steer it.
            self.prompted = Some(turn.to_string());
            out.prompts.push((turn.to_string(), texts(&item["content"], "text").join("\n")));
        } else if let (Some(id), Some(start), Some(end)) = (item["id"].as_str(), ms("started_at_ms"), ms("completed_at_ms")) {
            self.completed = Some((id.to_string(), start, end));
        }
    }

    fn response_item(&mut self, p: &Value, at: Option<f64>) {
        let s = |k: &str| p.get(k).and_then(Value::as_str);
        let kind = s("type").unwrap_or_default();
        let assistant = s("role") == Some("assistant");
        if kind.ends_with("_output") || kind == "additional_tools" || (kind == "message" && !assistant) {
            if self.response.started_at.is_none() {
                self.ready_at = at.or(self.ready_at);
            }
            return;
        }
        let r = &mut self.response;
        r.started_at = r.started_at.or(self.ready_at).or(at);
        let (start, end) = match self.completed.take().filter(|(id, ..)| Some(id.as_str()) == s("id")) {
            Some((_, start, end)) => (Some(start), Some(end)),
            None => (r.last_end.or(r.started_at), at),
        };
        r.last_end = end.or(r.last_end);
        let item = |kind, name: &str, output: &str| SpanUpdate {
            kind: Some(kind),
            name: Some(name.into()),
            start,
            end,
            status: Some(Status::Ok),
            output: Some(clip(output, MAX_TEXT).into()),
            ..Default::default()
        };
        match kind {
            "reasoning" => {
                let summary = texts(&p["summary"], "summary_text").join("\n\n");
                // The encrypted content is opaque; only the summary is readable.
                r.items.push(item(Kind::Reasoning, "reasoning", if summary.is_empty() { "(reasoning not shown)" } else { &summary }));
            }
            "message" => {
                let text = texts(&p["content"], "output_text").concat();
                if !text.is_empty() {
                    let mut span = item(Kind::Message, "assistant", &text);
                    if let Some(phase) = p.get("phase") {
                        span.attrs.insert("phase".into(), phase.clone());
                    }
                    r.items.push(span);
                    r.text.push(text);
                }
            }
            _ => r.calls.extend(s("call_id").map(str::to_string)),
        }
    }

    fn record(&mut self, p: &Value, at: Option<f64>, out: &mut Parsed) {
        let r = std::mem::take(&mut self.response);
        let s = |k: &str| p.get(k).and_then(Value::as_str);
        let (Some(thread), Some(response), Some(trace)) = (s("thread_id"), s("response_id"), s("root_turn_id").or(s("turn_id"))) else {
            return;
        };
        if self.thread.as_deref().is_some_and(|t| t != thread) {
            return;
        }
        // The root thread's id is the session id; a subagent's is its agent_id.
        let parent = if s("session_id") == Some(thread) { MAIN } else { thread };
        let usage = &p["usage"];
        let tokens = |k: &str| usage.get(k).and_then(Value::as_u64);
        let mut attrs = Map::new();
        for k in ["cached_input_tokens", "reasoning_output_tokens"] {
            if let Some(n) = tokens(k) {
                attrs.insert(k.into(), n.into());
            }
        }
        out.spans.push(SpanUpdate {
            parent_id: Some(parent.to_string()),
            kind: Some(Kind::Llm),
            name: Some(self.model.clone().unwrap_or_else(|| "model call".into())),
            start: r.started_at.or(self.ready_at).or(at),
            end: at,
            status: Some(Status::Ok),
            output: (!r.text.is_empty()).then(|| clip(&r.text.join("\n\n"), MAX_TEXT).into()),
            model: self.model.clone(),
            tokens_in: tokens("input_tokens"),
            tokens_out: tokens("output_tokens"),
            attrs,
            ..SpanUpdate::new(trace, response)
        });
        for (n, item) in r.items.into_iter().enumerate() {
            let tag = if item.kind == Some(Kind::Reasoning) { 'r' } else { 'm' };
            out.spans.push(SpanUpdate {
                trace_id: trace.to_string(),
                span_id: format!("{response}:{tag}{}", n + 1),
                parent_id: Some(response.to_string()),
                ..item
            });
        }
        out.calls.extend(r.calls.into_iter().map(|c| (c, response.to_string())));
    }
}

/// The `text` of each part of `parts` whose type is `kind`.
fn texts<'a>(parts: &'a Value, kind: &str) -> Vec<&'a str> {
    parts.as_array().into_iter().flatten().filter(|c| c["type"] == kind).filter_map(|c| c["text"].as_str()).collect()
}

/// The first `max` characters of `s`, marked when cut.
fn clip(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

/// A prompt's first line, short enough to name its trace.
fn title(prompt: &str) -> String {
    let line = prompt.lines().next().unwrap_or_default();
    let name = clip(line, 80);
    if name == line && line.len() < prompt.trim_end().len() { format!("{line}…") } else { name }
}

/// Epoch milliseconds of the UTC timestamps Codex writes, like "2026-10-27T01:05:47.756Z".
fn epoch_ms(ts: &str) -> Option<f64> {
    let (date, time) = ts.strip_suffix('Z')?.split_once('T')?;
    let mut ymd = date.splitn(3, '-').map(|n| n.parse::<i64>().ok());
    let (y, m, d) = (ymd.next()??, ymd.next()??, ymd.next()??);
    let mut hms = time.splitn(3, ':');
    let (h, min, sec): (i64, i64, f64) = (hms.next()?.parse().ok()?, hms.next()?.parse().ok()?, hms.next()?.parse().ok()?);
    // Days since 1970-01-01 in the proleptic Gregorian calendar (Howard Hinnant's days_from_civil).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    Some(((days * 86_400 + h * 3600 + min * 60) as f64 * 1000.0 + sec * 1000.0).round())
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::io::Write;

    use super::*;
    use crate::model::Span;
    use crate::store::Store;

    const HOOKS: &str = include_str!("../fixtures/codex-hooks.jsonl");
    const ROLLOUT: &str = include_str!("../fixtures/codex-rollout.jsonl");
    const AGENT_ROLLOUT: &str = include_str!("../fixtures/codex-rollout-agent.jsonl");
    /// The file names the hook fixture's transcript paths end in.
    const ROLLOUT_FILE: &str = "rollout-2026-11-01T09-00-00-019a-main.jsonl";
    const AGENT_ROLLOUT_FILE: &str = "rollout-2026-11-01T10-00-05-019a-c1.jsonl";
    /// 2026-11-01T10:00:00Z. Hook event i of the fixture is replayed i seconds later.
    const BASE: f64 = 1_793_527_200_000.0;
    const CHILD: &str = "019a-c1";

    fn at(seconds: f64) -> Option<f64> {
        Some(BASE + seconds * 1000.0)
    }

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("agent-tracker-codex-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn line_time(line: &str) -> f64 {
        let v: Value = serde_json::from_str(line).unwrap();
        epoch_ms(v["timestamp"].as_str().unwrap()).unwrap()
    }

    /// Replays the hook fixture. With `dir`, Codex's rollouts are written there
    /// the way Codex writes them: each line once its time has come.
    fn replay(dir: Option<&Path>) -> Store {
        let codex = Codex::new(dir.map(Path::to_path_buf));
        let store = Store::in_memory();
        let rollouts = [(ROLLOUT_FILE, ROLLOUT), (AGENT_ROLLOUT_FILE, AGENT_ROLLOUT)];
        let mut written = [0; 2];
        for (i, line) in HOOKS.lines().enumerate() {
            let now = BASE + 1000.0 * i as f64;
            let mut ev: Value = serde_json::from_str(line).unwrap();
            if let Some(dir) = dir {
                for (n, (name, text)) in rollouts.iter().enumerate() {
                    let due: Vec<&str> = text.lines().skip(written[n]).take_while(|l| line_time(l) <= now).collect();
                    let mut f = OpenOptions::new().create(true).append(true).open(dir.join(name)).unwrap();
                    f.write_all(due.iter().map(|l| format!("{l}\n")).collect::<String>().as_bytes()).unwrap();
                    written[n] += due.len();
                }
                for key in ["transcript_path", "agent_transcript_path"] {
                    if let Some(p) = ev[key].as_str() {
                        let name = p.rsplit(['\\', '/']).next().unwrap();
                        ev[key] = dir.join(name).to_string_lossy().into_owned().into();
                    }
                }
            }
            store.ingest(codex.handle(&ev, now, |t| store.has_trace(t)), now).unwrap();
        }
        store
    }

    fn find<'a>(spans: &'a [Span], id: &str) -> &'a Span {
        spans.iter().find(|s| s.span_id == id).unwrap_or_else(|| panic!("no span {id}"))
    }

    #[test]
    fn turn_with_tools_and_a_subagent_builds_a_codex_tree() {
        let store = replay(None);
        let spans = store.trace("turn-1").unwrap();
        let parent = |id: &str| find(&spans, id).parent_id.clone().unwrap_or_default();

        assert_eq!(spans.len(), 7);
        let prompt = find(&spans, PROMPT);
        assert_eq!((prompt.kind, prompt.attrs["source"].as_str()), (Kind::Message, Some("codex")));
        assert_eq!(prompt.input, Some(Value::from("List the files and have a helper read README.txt")));
        let main = find(&spans, MAIN);
        assert_eq!((main.name.as_str(), main.agent.as_deref(), main.status), ("Codex", Some("codex"), Status::Ok));
        assert_eq!(parent(MAIN), PROMPT);
        assert_eq!((parent("call_ls"), parent("call_spawn"), parent("call_wait")), (MAIN.into(), MAIN.into(), MAIN.into()));

        let spawn = find(&spans, "call_spawn");
        assert_eq!(spawn.name, "spawn_agent");
        assert_eq!(spawn.input, Some(serde_json::json!({"message": "Read README.txt and report its first line", "agent_type": "explorer"})));
        let agent = find(&spans, CHILD);
        assert_eq!((agent.kind, agent.agent.as_deref(), parent(CHILD)), (Kind::Agent, Some("explorer"), "call_spawn".into()));
        assert_eq!(agent.input, Some(Value::from("Read README.txt and report its first line")));
        assert_eq!(agent.output, Some(Value::from("First line: Hello from the README")));
        let cat = find(&spans, "call_cat");
        assert_eq!((parent("call_cat"), cat.agent.as_deref(), cat.name.as_str()), (CHILD.into(), Some("explorer"), "Bash"));
        assert!(spans.iter().all(|s| s.end.is_some() && s.status == Status::Ok), "every span closed cleanly");

        // The subagent's own prompt and turn id open nothing.
        assert!(!store.has_trace("turn-c1"));
        let summary = store.traces().into_iter().find(|t| t.trace_id == "turn-1").unwrap();
        assert_eq!((summary.source.as_deref(), summary.status), (Some("codex"), Status::Ok));
    }

    #[test]
    fn interrupt_fails_the_turn() {
        let store = replay(None);
        let spans = store.trace("turn-2").unwrap();
        let main = find(&spans, MAIN);
        assert_eq!((main.status, main.error.as_deref()), (Status::Error, Some("interrupted")));
        let test = find(&spans, "call_test");
        assert_eq!((test.status, test.error.as_deref()), (Status::Error, Some("session ended")));
    }

    #[test]
    fn translation_feeds_claude_code_its_own_names() {
        let ev = serde_json::json!({"session_id": "s", "turn_id": "t", "hook_event_name": "PreToolUse", "tool_name": "collaboration.spawn_agent",
            "tool_use_id": "c", "tool_input": {"message": "go", "agent_type": "worker", "task_name": "scout"}});
        let cc = to_claude_code(&ev).unwrap();
        assert_eq!((cc["prompt_id"].as_str(), cc["tool_name"].as_str()), (Some("t"), Some("Agent")));
        assert_eq!(
            cc["tool_input"],
            serde_json::json!({"message": "go", "agent_type": "worker", "task_name": "scout", "prompt": "go", "subagent_type": "worker", "description": "scout"})
        );

        let sub = serde_json::json!({"session_id": "s", "turn_id": "child-turn", "agent_id": "a", "hook_event_name": "PreToolUse", "tool_name": "Bash"});
        assert!(to_claude_code(&sub).unwrap().get("prompt_id").is_none(), "a subagent's turn never names a trace");
        let interrupt = to_claude_code(&serde_json::json!({"session_id": "s", "hook_event_name": "Interrupt"})).unwrap();
        assert_eq!((interrupt["hook_event_name"].as_str(), interrupt["error"].as_str()), (Some("StopFailure"), Some("interrupted")));
        assert_eq!(to_claude_code(&serde_json::json!({"session_id": "s", "hook_event_name": "SessionStart"})), None);
    }

    #[test]
    fn relabel_says_codex_and_restores_the_tool() {
        let ev = serde_json::json!({"tool_name": "spawn_agent", "tool_use_id": "c", "tool_input": {"message": "go"}});
        let mut attrs = Map::new();
        attrs.insert("source".into(), "claude-code".into());
        let mut updates = vec![
            SpanUpdate { attrs, ..SpanUpdate::new("t", PROMPT) },
            SpanUpdate { name: Some("Claude Code".into()), agent: Some("claude-code".into()), ..SpanUpdate::new("t", MAIN) },
            SpanUpdate { name: Some("Agent".into()), input: Some(serde_json::json!({"message": "go", "prompt": "go"})), ..SpanUpdate::new("t", "c") },
            SpanUpdate { name: Some("explorer".into()), agent: Some("explorer".into()), ..SpanUpdate::new("t", "a1") },
        ];
        relabel(&mut updates, &ev);
        assert_eq!(updates[0].attrs["source"], "codex");
        assert_eq!((updates[1].name.as_deref(), updates[1].agent.as_deref()), (Some("Codex"), Some("codex")));
        assert_eq!((updates[2].name.as_deref(), updates[2].input.as_ref()), (Some("spawn_agent"), Some(&serde_json::json!({"message": "go"}))));
        assert_eq!((updates[3].name.as_deref(), updates[3].agent.as_deref()), (Some("explorer"), Some("explorer")));
    }

    #[test]
    fn rollout_records_become_llm_spans() {
        let parsed = Parser::default().parse(ROLLOUT);
        let llm: Vec<&SpanUpdate> = parsed.spans.iter().filter(|s| s.kind == Some(Kind::Llm)).collect();
        let ids: Vec<&str> = llm.iter().map(|s| s.span_id.as_str()).collect();
        assert_eq!(ids, ["resp_0", "resp_1", "resp_2", "resp_3", "resp_4", "resp_5"]);

        let first = llm[1];
        assert_eq!((first.trace_id.as_str(), first.parent_id.as_deref()), ("turn-1", Some(MAIN)));
        assert_eq!((first.model.as_deref(), first.name.as_deref()), (Some("gpt-5.6-sol"), Some("gpt-5.6-sol")));
        // Requested once the prompt was recorded, done when its usage was.
        assert_eq!((first.start, first.end), (at(1.2), at(1.9)));
        assert_eq!((first.tokens_in, first.tokens_out), (Some(1200), Some(40)));
        assert_eq!((first.attrs["cached_input_tokens"].as_u64(), first.attrs["reasoning_output_tokens"].as_u64()), (Some(1000), Some(12)));
        assert_eq!(first.output, Some(Value::from("Listing the files.")));

        // Requested once the tool output came back; a tool call alone has no text.
        assert_eq!((llm[2].start, llm[2].end, llm[2].output.as_ref()), (at(3.1), at(3.8), None));
        assert_eq!(llm[4].output, Some(Value::from("Files: README.txt, notes.md.\n\nREADME starts with: Hello from the README")));

        let pairs = |v: &[(String, String)]| v.iter().map(|(a, b)| format!("{a}<-{b}")).collect::<Vec<_>>();
        assert_eq!(pairs(&parsed.calls), ["call_ls<-resp_1", "call_spawn<-resp_2", "call_wait<-resp_3", "call_test<-resp_5"]);
        assert_eq!(pairs(&parsed.prompts), ["turn-1<-List the files and have a helper read README.txt"]);

        let agent = Parser::default().parse(AGENT_ROLLOUT);
        let llm: Vec<&SpanUpdate> = agent.spans.iter().filter(|s| s.kind == Some(Kind::Llm)).collect();
        assert_eq!(llm.iter().map(|s| (s.trace_id.as_str(), s.parent_id.as_deref())).collect::<Vec<_>>(), [("turn-1", Some(CHILD)); 2]);
        assert_eq!((llm[0].model.as_deref(), llm[0].start, llm[0].end), (Some("gpt-5.6-sol-mini"), at(5.3), at(8.5)));
        assert!(agent.prompts.is_empty(), "a subagent's task is not the user's prompt");
    }

    #[test]
    fn reasoning_and_messages_hang_under_their_response() {
        let parsed = Parser::default().parse(ROLLOUT);
        let children = |id: &str| parsed.spans.iter().filter(|s| s.parent_id.as_deref() == Some(id)).cloned().collect::<Vec<_>>();

        let first = children("resp_1");
        assert_eq!(first.iter().map(|s| (s.span_id.as_str(), s.kind)).collect::<Vec<_>>(), [("resp_1:r1", Some(Kind::Reasoning)), ("resp_1:m2", Some(Kind::Message))]);
        let (thinking, said) = (&first[0], &first[1]);
        // Timed by item_completed when Codex reported the item.
        assert_eq!((thinking.start, thinking.end), (at(1.3), at(1.6)));
        assert_eq!(thinking.output, Some(Value::from("**Planning**\n\nList the folder first.")));
        assert_eq!((said.name.as_deref(), said.start, said.end), (Some("assistant"), at(1.62), at(1.7)));
        assert_eq!((said.output.as_ref(), said.attrs["phase"].as_str()), (Some(&Value::from("Listing the files.")), Some("commentary")));

        // No summary and no item_completed: placeholder text, timed from the response start to its line.
        let hidden = children("resp_2");
        assert_eq!((hidden[0].output.as_ref(), hidden[0].start, hidden[0].end), (Some(&Value::from("(reasoning not shown)")), at(3.1), at(3.5)));
        assert!(children("resp_3").is_empty());
        assert_eq!(children("resp_4").iter().map(|s| s.span_id.as_str()).collect::<Vec<_>>(), ["resp_4:m1", "resp_4:m2"]);
        assert_eq!(clip(&"é".repeat(MAX_TEXT + 5), MAX_TEXT).chars().count(), MAX_TEXT + 1);
    }

    #[test]
    fn records_inherited_from_another_thread_are_skipped() {
        let meta = r#"{"timestamp":"2026-11-01T10:00:00.000Z","type":"session_meta","payload":{"id":"child","session_id":"root"}}"#;
        let inherited = r#"{"timestamp":"2026-11-01T10:00:01.000Z","type":"token_usage_record","payload":{"thread_id":"root","session_id":"root","turn_id":"t","root_turn_id":"t","response_id":"r","usage":{}}}"#;
        assert_eq!(Parser::default().parse(&format!("{meta}\n{inherited}\n")), Parsed::default());
    }

    #[test]
    fn rollout_is_read_in_pieces_without_losing_a_line() {
        let dir = scratch_dir("pieces");
        let path = dir.join("rollout.jsonl");
        let whole = Parser::default().parse(ROLLOUT);
        // Cut mid-line: the partial line must wait for the rest.
        let (bytes, cut) = (ROLLOUT.as_bytes(), ROLLOUT.len() / 2 + 7);
        fs::write(&path, &bytes[..cut]).unwrap();
        let mut rollout = Rollout::default();
        let mut pieces = rollout.read(&path).unwrap();
        OpenOptions::new().append(true).open(&path).unwrap().write_all(&bytes[cut..]).unwrap();
        pieces.extend(rollout.read(&path).unwrap());
        assert_eq!(rollout.read(&path).unwrap(), Parsed::default(), "nothing is read twice");
        assert_eq!(pieces, whole);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_jsonl_rollouts_inside_the_sessions_folder_are_read() {
        let dir = scratch_dir("paths");
        let sessions = dir.join("sessions");
        fs::create_dir_all(sessions.join("2026")).unwrap();
        for f in ["sessions/2026/a.jsonl", "sessions/2026/b.jsonl.zst", "outside.jsonl"] {
            fs::write(dir.join(f), "").unwrap();
        }
        let path = |p: &str| rollout_path(&sessions, &dir.join(p).to_string_lossy());
        assert!(path("sessions/2026/a.jsonl").is_some());
        assert_eq!(path("sessions/2026/b.jsonl.zst"), None);
        assert_eq!(path("outside.jsonl"), None);
        assert_eq!(path("sessions/../outside.jsonl"), None);
        assert_eq!(path("sessions/2026/missing.jsonl"), None);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rollout_spans_join_the_hook_trace() {
        let dir = scratch_dir("e2e");
        let store = replay(Some(&dir));
        let spans = store.trace("turn-1").unwrap();
        let llm: Vec<(&str, &str)> =
            spans.iter().filter(|s| s.kind == Kind::Llm).map(|s| (s.span_id.as_str(), s.parent_id.as_deref().unwrap())).collect();
        assert_eq!(llm, [("resp_1", MAIN), ("resp_2", MAIN), ("resp_3", MAIN), ("resp_c1", CHILD), ("resp_c2", CHILD), ("resp_4", MAIN)]);
        // resp_c2 reached the disk after SubagentStop; the SessionEnd sweep found it.
        assert_eq!(find(&spans, "resp_c2:m1").output, Some(Value::from("First line: Hello from the README")));
        assert_eq!(spans.len(), 7 + 6 + 7, "hook spans, responses, and their reasoning and messages");

        // Each tool span points back at the response that called it.
        for (call, response) in [("call_ls", "resp_1"), ("call_spawn", "resp_2"), ("call_wait", "resp_3"), ("call_cat", "resp_c1")] {
            assert_eq!(find(&spans, call).links, [response], "{call}");
        }
        // The hook already gave the prompt; the rollout's copy doesn't replace it.
        assert_eq!(find(&spans, PROMPT).name, "List the files and have a helper read README.txt");

        let turn2 = store.trace("turn-2").unwrap();
        assert_eq!(find(&turn2, "resp_5").parent_id.as_deref(), Some(MAIN));
        assert_eq!(find(&turn2, "call_test").links, ["resp_5"]);
        // The earlier turn already in the rollout when the session started gets no trace.
        assert!(!store.has_trace("turn-0"));
        let summary = store.traces().into_iter().find(|t| t.trace_id == "turn-1").unwrap();
        assert_eq!(summary.tokens, [1240, 1360, 1420, 830, 915, 1530].iter().sum::<u64>());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rollout_fills_in_a_prompt_the_hooks_missed() {
        let dir = scratch_dir("prompt");
        let path = dir.join("rollout-x.jsonl");
        let line = |ts: &str, kind: &str, payload: Value| serde_json::json!({"timestamp": ts, "type": kind, "payload": payload}).to_string() + "\n";
        let user = |turn: &str, text: &str| {
            line("2026-11-01T10:00:01.000Z", "event_msg", serde_json::json!({"type": "item_completed", "thread_id": "s", "turn_id": turn,
                "started_at_ms": 0, "completed_at_ms": 0, "item": {"type": "UserMessage", "id": "u", "content": [{"type": "text", "text": text}]}}))
        };
        fs::write(&path, line("2026-11-01T10:00:00.000Z", "session_meta", serde_json::json!({"id": "s", "session_id": "s"})) + &user("t1", "what is in here?")).unwrap();
        let codex = Codex::new(Some(dir.clone()));
        let tool = |turn: &str| serde_json::json!({"session_id": "s", "turn_id": turn, "transcript_path": path, "hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_use_id": format!("c-{turn}")});

        // The tracker missed t1's UserPromptSubmit, so its first tool call opens the trace.
        let updates = codex.handle(&tool("t1"), 2.0, |_| false);
        let prompt: Vec<&SpanUpdate> = updates.iter().filter(|u| u.span_id == PROMPT).collect();
        assert_eq!(prompt[0].name.as_deref(), Some("(prompt not captured)"));
        assert_eq!((prompt[1].name.as_deref(), prompt[1].input.as_ref()), (Some("what is in here?"), Some(&Value::from("what is in here?"))));

        let submit = serde_json::json!({"session_id": "s", "turn_id": "t2", "transcript_path": path, "hook_event_name": "UserPromptSubmit", "prompt": "next"});
        codex.handle(&submit, 3.0, |_| true);
        OpenOptions::new().append(true).open(&path).unwrap().write_all(user("t2", "next").as_bytes()).unwrap();
        assert!(codex.handle(&tool("t2"), 4.0, |_| true).iter().all(|u| u.span_id != PROMPT));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn epoch_ms_matches_codex_item_times() {
        // The same instant appears as a line timestamp and as started_at_ms in a real rollout.
        assert_eq!(epoch_ms("2026-10-27T01:05:17.619Z"), Some(1_793_063_117_619.0));
        assert_eq!(epoch_ms("2000-02-29T23:59:59.999Z"), Some(951_868_799_999.0));
        assert_eq!(epoch_ms("1970-01-01T00:00:00Z"), Some(0.0));
        assert_eq!(epoch_ms("2026-10-27 01:05:17"), None);
    }

    #[test]
    fn title_is_the_first_line() {
        assert_eq!(title("short"), "short");
        assert_eq!(title("line one\nline two"), "line one…");
        assert_eq!(title(&"x".repeat(90)), format!("{}…", "x".repeat(80)));
    }
}
