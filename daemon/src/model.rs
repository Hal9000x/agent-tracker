use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Message,
    Agent,
    Llm,
    Tool,
    Split,
    Fork,
    Join,
    #[serde(other)]
    Step,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Message => "message",
            Kind::Agent => "agent",
            Kind::Llm => "llm",
            Kind::Tool => "tool",
            Kind::Split => "split",
            Kind::Fork => "fork",
            Kind::Join => "join",
            Kind::Step => "step",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Running,
    Ok,
    Error,
}

/// A partial span as sent by clients. Updates with the same (trace_id, span_id)
/// merge, so a span can be reported once at start and again at end, in either order.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SpanUpdate {
    pub trace_id: String,
    pub span_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<Kind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Epoch milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_in: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_out: Option<u64>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub attrs: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<String>,
}

impl SpanUpdate {
    pub fn new(trace_id: impl Into<String>, span_id: impl Into<String>) -> Self {
        SpanUpdate { trace_id: trace_id.into(), span_id: span_id.into(), ..Default::default() }
    }
}

/// A span after all of its updates have been merged.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Span {
    pub trace_id: String,
    pub span_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    pub kind: Kind,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    pub start: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end: Option<f64>,
    pub status: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_in: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_out: Option<u64>,
    #[serde(skip_serializing_if = "Map::is_empty")]
    pub attrs: Map<String, Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<String>,
    /// Server-assigned, increases on every change; clients keep the highest.
    pub seq: u64,
}

impl Span {
    /// `received` stands in for a missing start time.
    pub fn from_update(u: SpanUpdate, received: f64, seq: u64) -> Span {
        let kind = u.kind.unwrap_or(Kind::Step);
        let mut span = Span {
            trace_id: u.trace_id.clone(),
            span_id: u.span_id.clone(),
            parent_id: None,
            kind,
            name: kind.label().to_string(),
            agent: None,
            start: u.start.or(u.end).unwrap_or(received),
            end: None,
            status: Status::Running,
            error: None,
            input: None,
            output: None,
            model: None,
            tokens_in: None,
            tokens_out: None,
            attrs: Map::new(),
            links: Vec::new(),
            seq,
        };
        span.apply(u, seq);
        span
    }

    pub fn apply(&mut self, u: SpanUpdate, seq: u64) {
        fn set<T>(slot: &mut Option<T>, v: Option<T>) {
            if v.is_some() {
                *slot = v;
            }
        }
        set(&mut self.parent_id, u.parent_id);
        if let Some(k) = u.kind {
            self.kind = k;
        }
        if let Some(n) = u.name {
            self.name = n;
        }
        set(&mut self.agent, u.agent);
        if let Some(s) = u.start {
            self.start = self.start.min(s);
        }
        set(&mut self.end, u.end);
        set(&mut self.error, u.error);
        set(&mut self.input, u.input);
        set(&mut self.output, u.output);
        set(&mut self.model, u.model);
        set(&mut self.tokens_in, u.tokens_in);
        set(&mut self.tokens_out, u.tokens_out);
        self.attrs.extend(u.attrs);
        for l in u.links {
            if !self.links.contains(&l) {
                self.links.push(l);
            }
        }
        // A late "running" must not reopen a span whose end already arrived.
        match u.status {
            Some(Status::Running) if self.status != Status::Running => {}
            Some(s) => self.status = s,
            None if self.end.is_some() && self.status == Status::Running => self.status = Status::Ok,
            None => {}
        }
        self.seq = seq;
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TraceSummary {
    pub trace_id: String,
    pub name: String,
    pub start: f64,
    /// None while any span is still running.
    pub end: Option<f64>,
    pub status: Status,
    pub spans: usize,
    pub errors: usize,
    pub agents: Vec<String>,
    pub tokens: u64,
    pub source: Option<String>,
    pub seq: u64,
}

impl TraceSummary {
    pub fn of<'a>(trace_id: &str, spans: impl IntoIterator<Item = &'a Span>) -> Option<TraceSummary> {
        let spans: Vec<&Span> = spans.into_iter().collect();
        let ids: HashSet<&str> = spans.iter().map(|s| s.span_id.as_str()).collect();
        let is_root = |s: &Span| s.parent_id.as_deref().is_none_or(|p| !ids.contains(p));
        let earliest_root = |message_only: bool| {
            spans
                .iter()
                .copied()
                .filter(|s| is_root(s) && (!message_only || s.kind == Kind::Message))
                .min_by(|a, b| a.start.total_cmp(&b.start))
        };
        // Orphans (a subagent tool whose agent is in another trace) can start before the message.
        let root = earliest_root(true).or_else(|| earliest_root(false)).or_else(|| spans.first().copied())?;
        let running = spans.iter().any(|s| s.status == Status::Running);
        let errors = spans.iter().filter(|s| s.status == Status::Error).count();
        let mut agents: Vec<String> = Vec::new();
        for a in spans.iter().filter_map(|s| s.agent.as_ref()) {
            if !agents.contains(a) {
                agents.push(a.clone());
            }
        }
        let attr = |k: &str| root.attrs.get(k).and_then(Value::as_str).map(str::to_string);
        Some(TraceSummary {
            trace_id: trace_id.to_string(),
            name: root.name.clone(),
            start: spans.iter().map(|s| s.start).fold(f64::INFINITY, f64::min),
            end: if running {
                None
            } else {
                spans.iter().filter_map(|s| s.end).fold(None, |m: Option<f64>, e| Some(m.map_or(e, |m| m.max(e))))
            },
            status: if running {
                Status::Running
            } else if errors > 0 {
                Status::Error
            } else {
                Status::Ok
            },
            spans: spans.len(),
            errors,
            agents,
            tokens: spans.iter().fold(0u64, |n, s| n.saturating_add(s.tokens_in.unwrap_or(0)).saturating_add(s.tokens_out.unwrap_or(0))),
            source: attr("source").or_else(|| attr("service.name")),
            seq: spans.iter().map(|s| s.seq).max().unwrap_or(0),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start(t: f64) -> SpanUpdate {
        SpanUpdate { kind: Some(Kind::Tool), name: Some("search".into()), start: Some(t), ..SpanUpdate::new("t", "s") }
    }
    fn end(t: f64) -> SpanUpdate {
        SpanUpdate { end: Some(t), output: Some("done".into()), ..SpanUpdate::new("t", "s") }
    }

    #[test]
    fn start_then_end_merges() {
        let mut s = Span::from_update(start(10.0), 999.0, 1);
        assert_eq!(s.status, Status::Running);
        s.apply(end(25.0), 2);
        assert_eq!((s.start, s.end, s.status), (10.0, Some(25.0), Status::Ok));
        assert_eq!(s.name, "search");
        assert_eq!(s.output, Some(Value::from("done")));
    }

    #[test]
    fn end_before_start_gives_same_span() {
        let mut a = Span::from_update(start(10.0), 999.0, 1);
        a.apply(end(25.0), 2);
        let mut b = Span::from_update(end(25.0), 999.0, 1);
        b.apply(SpanUpdate { status: Some(Status::Running), ..start(10.0) }, 2);
        assert_eq!(a, b);
    }

    #[test]
    fn unknown_kind_becomes_step() {
        let u: SpanUpdate = serde_json::from_str(r#"{"trace_id":"t","span_id":"s","kind":"retrieval"}"#).unwrap();
        assert_eq!(u.kind, Some(Kind::Step));
    }

    #[test]
    fn huge_token_counts_saturate_instead_of_panicking() {
        let a = Span::from_update(SpanUpdate { tokens_in: Some(u64::MAX), tokens_out: Some(1), ..SpanUpdate::new("t", "a") }, 0.0, 1);
        let b = Span::from_update(SpanUpdate { tokens_in: Some(5), ..SpanUpdate::new("t", "b") }, 0.0, 2);
        assert_eq!(TraceSummary::of("t", [&a, &b]).unwrap().tokens, u64::MAX);
    }

    #[test]
    fn trace_is_named_after_its_message_not_an_earlier_orphan() {
        // A subagent tool whose agent span sits in another trace has no parent here.
        let orphan = Span::from_update(
            SpanUpdate { parent_id: Some("elsewhere".into()), kind: Some(Kind::Tool), name: Some("WebFetch".into()), start: Some(1.0), ..SpanUpdate::new("t", "w") },
            0.0,
            1,
        );
        let mut attrs = Map::new();
        attrs.insert("source".into(), "claude-code".into());
        let message = Span::from_update(
            SpanUpdate { kind: Some(Kind::Message), name: Some("hi".into()), start: Some(5.0), attrs, ..SpanUpdate::new("t", "m") },
            0.0,
            2,
        );
        let s = TraceSummary::of("t", [&orphan, &message]).unwrap();
        assert_eq!((s.name.as_str(), s.source.as_deref()), ("hi", Some("claude-code")));
        assert_eq!(TraceSummary::of("t", [&orphan]).unwrap().name, "WebFetch", "without a message the earliest root names it");
    }

    #[test]
    fn summary_reports_running_until_all_spans_end() {
        let root = Span::from_update(
            SpanUpdate { kind: Some(Kind::Message), name: Some("hi".into()), start: Some(0.0), end: Some(0.0), ..SpanUpdate::new("t", "m") },
            0.0,
            1,
        );
        let mut child = Span::from_update(
            SpanUpdate { parent_id: Some("m".into()), agent: Some("a".into()), start: Some(1.0), tokens_in: Some(5), ..SpanUpdate::new("t", "c") },
            0.0,
            2,
        );
        let s = TraceSummary::of("t", [&root, &child]).unwrap();
        assert_eq!((s.name.as_str(), s.status, s.end), ("hi", Status::Running, None));
        child.apply(SpanUpdate { end: Some(9.0), status: Some(Status::Error), ..SpanUpdate::new("t", "c") }, 3);
        let s = TraceSummary::of("t", [&root, &child]).unwrap();
        assert_eq!((s.status, s.end, s.errors, s.tokens, s.seq), (Status::Error, Some(9.0), 1, 5, 3));
        assert_eq!(s.agents, vec!["a".to_string()]);
    }
}
