//! Maps Claude Code hook events (posted by `"type": "http"` hooks) onto spans.
//! Each submitted prompt becomes one trace: prompt -> main agent -> tool calls,
//! with subagents nested under the Agent tool call that spawned them.
//! Fields are read leniently because the hook payload schema grows over time.

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::{Map, Value};

use crate::model::{Kind, SpanUpdate, Status};

const PROMPT: &str = "prompt";
const MAIN: &str = "main";

#[derive(Default)]
pub struct ClaudeCode {
    sessions: Mutex<HashMap<String, Session>>,
}

struct PendingAgent {
    trace_id: String,
    tool_use_id: String,
    subagent_type: Option<String>,
    description: Option<String>,
    prompt: Option<Value>,
}

struct OpenSpan {
    trace_id: String,
    span_id: String,
    /// Subagents can keep working in the background after the user sends another prompt.
    in_subagent: bool,
}

#[derive(Default)]
struct Session {
    trace_id: Option<String>,
    /// Agent tool calls whose subagent has not started yet, oldest first.
    pending_agents: Vec<PendingAgent>,
    /// The trace each running subagent started in. Its later events go there,
    /// even after a new prompt has opened another trace.
    agent_traces: HashMap<String, String>,
    /// Spans that started and have not ended.
    open: Vec<OpenSpan>,
}

impl Session {
    /// Ends open spans as failed. Subagent work is left running unless `all`.
    fn close_open(&mut self, now: f64, reason: &str, all: bool) -> Vec<SpanUpdate> {
        let (closing, keep): (Vec<OpenSpan>, Vec<OpenSpan>) = std::mem::take(&mut self.open).into_iter().partition(|o| all || !o.in_subagent);
        self.open = keep;
        closing
            .into_iter()
            .map(|o| SpanUpdate {
                end: Some(now),
                status: Some(Status::Error),
                error: Some(reason.to_string()),
                ..SpanUpdate::new(o.trace_id, o.span_id)
            })
            .collect()
    }

    fn opened(&mut self, trace_id: &str, span_id: &str, in_subagent: bool) {
        self.open.push(OpenSpan { trace_id: trace_id.to_string(), span_id: span_id.to_string(), in_subagent });
    }

    fn closed(&mut self, trace_id: &str, span_id: &str) {
        self.open.retain(|o| !(o.trace_id == trace_id && o.span_id == span_id));
    }
}

/// A background subagent's final message is empty (it reports another way); treat that as no output.
fn text_output(v: Option<&Value>) -> Option<Value> {
    v.filter(|v| v.as_str() != Some("")).cloned()
}

impl ClaudeCode {
    /// `known` says whether a trace is already stored, so a tracker restarted
    /// mid-turn resumes that turn's trace instead of re-creating its prompt.
    pub fn handle(&self, ev: &Value, now: f64, known: impl Fn(&str) -> bool) -> Vec<SpanUpdate> {
        let s = |k: &str| ev.get(k).and_then(Value::as_str);
        let Some(session_id) = s("session_id") else { return vec![] };
        let event = s("hook_event_name").unwrap_or_default();
        let mut sessions = self.sessions.lock().unwrap();
        let session = sessions.entry(session_id.to_string()).or_default();
        let mut out = Vec::new();

        match event {
            "UserPromptSubmit" => {
                out.extend(session.close_open(now, "interrupted", false));
                let trace_id = s("prompt_id").map_or_else(|| format!("{session_id}-{now}"), str::to_string);
                out.extend(begin(session, trace_id, s("prompt"), ev, now));
                return out;
            }
            "SessionEnd" => {
                out.extend(session.close_open(now, "session ended", true));
                sessions.remove(session_id);
                return out;
            }
            _ => {}
        }

        // Events before any prompt (the tracker started mid-turn) open a trace of their own.
        if session.trace_id.is_none() {
            let trace_id = s("prompt_id").map_or_else(|| format!("{session_id}-{now}"), str::to_string);
            if known(&trace_id) {
                session.opened(&trace_id, MAIN, false);
                session.trace_id = Some(trace_id);
            } else {
                out.extend(begin(session, trace_id, None, ev, now));
            }
        }
        let agent_id = s("agent_id");
        let trace_id = match agent_id.and_then(|a| session.agent_traces.get(a)) {
            Some(t) => t.clone(),
            None => session.trace_id.clone().unwrap(),
        };
        let span = |id: &str| SpanUpdate::new(trace_id.clone(), id);
        let tool_use_id = s("tool_use_id").unwrap_or_default();

        match event {
            "PreToolUse" if !tool_use_id.is_empty() => {
                let tool_input = ev.get("tool_input").cloned();
                if matches!(s("tool_name"), Some("Agent" | "Task")) {
                    let field = |k: &str| tool_input.as_ref().and_then(|i| i.get(k)).cloned();
                    session.pending_agents.push(PendingAgent {
                        trace_id: trace_id.clone(),
                        tool_use_id: tool_use_id.to_string(),
                        subagent_type: field("subagent_type").and_then(|v| v.as_str().map(str::to_string)),
                        description: field("description").and_then(|v| v.as_str().map(str::to_string)),
                        prompt: field("prompt"),
                    });
                }
                let mut attrs = Map::new();
                if let Some(mcp) = ev.get("mcp_server") {
                    attrs.insert("mcp_server".into(), mcp.clone());
                }
                session.opened(&trace_id, tool_use_id, agent_id.is_some());
                out.push(SpanUpdate {
                    parent_id: Some(agent_id.unwrap_or(MAIN).to_string()),
                    kind: Some(Kind::Tool),
                    name: s("tool_name").map(str::to_string),
                    agent: s("agent_type").map(str::to_string),
                    start: Some(now),
                    input: tool_input,
                    attrs,
                    ..span(tool_use_id)
                });
            }
            "PostToolUse" | "PostToolUseFailure" if !tool_use_id.is_empty() => {
                session.closed(&trace_id, tool_use_id);
                let failed = event == "PostToolUseFailure";
                let duration = ev.get("duration_ms").and_then(Value::as_f64);
                out.push(SpanUpdate {
                    parent_id: Some(agent_id.unwrap_or(MAIN).to_string()),
                    kind: Some(Kind::Tool),
                    name: s("tool_name").map(str::to_string),
                    agent: s("agent_type").map(str::to_string),
                    start: duration.map(|d| now - d),
                    end: Some(now),
                    status: Some(if failed { Status::Error } else { Status::Ok }),
                    error: if failed { s("error").map(str::to_string) } else { None },
                    output: ev.get("tool_response").cloned(),
                    ..span(tool_use_id)
                });
            }
            "SubagentStart" => {
                let Some(agent_id) = agent_id else { return out };
                let agent_type = s("agent_type");
                let pending = &mut session.pending_agents;
                let idx = pending
                    .iter()
                    .position(|p| p.subagent_type.as_deref().unwrap_or("general-purpose") == agent_type.unwrap_or_default())
                    .or((!pending.is_empty()).then_some(0));
                let spawned_by = idx.map(|i| pending.remove(i));
                let trace_id = spawned_by.as_ref().map_or(trace_id, |p| p.trace_id.clone());
                session.agent_traces.insert(agent_id.to_string(), trace_id.clone());
                session.opened(&trace_id, agent_id, true);
                out.push(SpanUpdate {
                    parent_id: Some(spawned_by.as_ref().map_or(MAIN, |p| p.tool_use_id.as_str()).to_string()),
                    kind: Some(Kind::Agent),
                    name: spawned_by.as_ref().and_then(|p| p.description.clone()).or(agent_type.map(str::to_string)),
                    agent: agent_type.map(str::to_string),
                    start: Some(now),
                    input: spawned_by.and_then(|p| p.prompt),
                    ..SpanUpdate::new(trace_id, agent_id)
                });
            }
            "SubagentStop" => {
                let Some(agent_id) = agent_id else { return out };
                session.closed(&trace_id, agent_id);
                session.agent_traces.remove(agent_id);
                let mut attrs = Map::new();
                if let Some(p) = ev.get("agent_transcript_path") {
                    attrs.insert("agent_transcript_path".into(), p.clone());
                }
                out.push(SpanUpdate {
                    end: Some(now),
                    status: Some(Status::Ok),
                    output: text_output(ev.get("last_assistant_message")),
                    attrs,
                    ..span(agent_id)
                });
            }
            "Stop" | "StopFailure" => {
                session.closed(&trace_id, MAIN);
                let failed = event == "StopFailure";
                let error = [s("error"), s("error_details")].into_iter().flatten().collect::<Vec<_>>().join(": ");
                out.push(SpanUpdate {
                    end: Some(now),
                    status: Some(if failed { Status::Error } else { Status::Ok }),
                    error: failed.then_some(error),
                    output: text_output(ev.get("last_assistant_message")),
                    ..span(MAIN)
                });
            }
            _ => {}
        }
        out
    }
}

fn begin(session: &mut Session, trace_id: String, prompt: Option<&str>, ev: &Value, now: f64) -> Vec<SpanUpdate> {
    session.trace_id = Some(trace_id.clone());
    session.pending_agents.clear();
    session.opened(&trace_id, MAIN, false);
    let mut attrs = Map::new();
    attrs.insert("source".into(), "claude-code".into());
    for k in ["session_id", "cwd", "permission_mode", "transcript_path"] {
        if let Some(v) = ev.get(k) {
            attrs.insert(k.into(), v.clone());
        }
    }
    let name = match prompt {
        Some(p) => truncate(p, 80),
        None => "(prompt not captured)".to_string(),
    };
    vec![
        SpanUpdate {
            kind: Some(Kind::Message),
            name: Some(name),
            start: Some(now),
            end: Some(now),
            status: Some(Status::Ok),
            input: prompt.map(Value::from),
            attrs,
            ..SpanUpdate::new(trace_id.clone(), PROMPT)
        },
        SpanUpdate {
            parent_id: Some(PROMPT.into()),
            kind: Some(Kind::Agent),
            name: Some("Claude Code".into()),
            agent: Some(ev.get("agent_type").and_then(Value::as_str).unwrap_or("claude-code").to_string()),
            start: Some(now),
            ..SpanUpdate::new(trace_id, MAIN)
        },
    ]
}

fn truncate(s: &str, max: usize) -> String {
    let line = s.lines().next().unwrap_or_default();
    match line.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &line[..i]),
        None if line.len() < s.trim_end().len() => format!("{line}…"),
        None => line.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    fn replay(fixture: &str) -> Store {
        let store = Store::in_memory();
        let cc = ClaudeCode::default();
        for (i, line) in fixture.lines().filter(|l| !l.trim().is_empty()).enumerate() {
            let ev: Value = serde_json::from_str(line).unwrap();
            let t = 1000.0 * (i as f64 + 1.0);
            store.ingest(cc.handle(&ev, t, |id| store.has_trace(id)), t).unwrap();
        }
        store
    }

    #[test]
    fn session_with_parallel_subagents_builds_a_tree() {
        let store = replay(include_str!("../fixtures/claude-code-session.jsonl"));
        let spans = store.trace("p1").unwrap();
        let by = |id: &str| spans.iter().find(|s| s.span_id == id).unwrap_or_else(|| panic!("no span {id}"));
        let parent = |id: &str| by(id).parent_id.clone().unwrap_or_default();

        assert_eq!(spans.len(), 9);
        assert_eq!(by(PROMPT).kind, Kind::Message);
        assert_eq!(by(PROMPT).input, Some(Value::from("Find the launch code and list the files")));
        assert_eq!(parent(MAIN), PROMPT);
        assert_eq!((parent("toolu_A"), parent("toolu_B")), (MAIN.into(), MAIN.into()));
        assert_eq!((parent("agent-a1"), parent("agent-a2")), ("toolu_A".into(), "toolu_B".into()));
        assert_eq!((parent("toolu_r1"), parent("toolu_g1"), parent("toolu_r2")), ("agent-a1".into(), "agent-a2".into(), MAIN.into()));

        let a1 = by("agent-a1");
        assert_eq!((a1.kind, a1.agent.as_deref(), a1.name.as_str()), (Kind::Agent, Some("Explore"), "Find launch code"));
        assert_eq!(a1.output, Some(Value::from("The launch code is 42.")));
        assert_eq!((by("toolu_r1").start, by("toolu_r1").end), (6000.0, Some(8000.0)));

        let failed = by("toolu_r2");
        assert_eq!((failed.status, failed.error.as_deref()), (Status::Error, Some("File does not exist.")));
        assert_eq!(by(MAIN).status, Status::Ok);
        assert!(spans.iter().all(|s| s.end.is_some()), "every span closed");

        let summary = store.traces().into_iter().find(|t| t.trace_id == "p1").unwrap();
        assert_eq!(summary.source.as_deref(), Some("claude-code"));
        assert_eq!(summary.status, Status::Error);
    }

    #[test]
    fn interrupted_turn_is_closed_by_session_end() {
        let store = replay(include_str!("../fixtures/claude-code-session.jsonl"));
        let spans = store.trace("p2").unwrap();
        let bash = spans.iter().find(|s| s.span_id == "toolu_bash").unwrap();
        assert_eq!((bash.status, bash.error.as_deref()), (Status::Error, Some("session ended")));
        assert!(spans.iter().all(|s| s.end.is_some()));
    }

    #[test]
    fn background_subagent_keeps_its_trace_across_a_new_prompt() {
        let ev = |p: &str, name: &str, extra: Value| {
            let mut e = serde_json::json!({"session_id": "s", "prompt_id": p, "hook_event_name": name});
            e.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            e.to_string()
        };
        let events = [
            ev("p1", "UserPromptSubmit", serde_json::json!({"prompt": "look something up"})),
            ev("p1", "PreToolUse", serde_json::json!({"tool_name": "Agent", "tool_use_id": "tu_A", "tool_input": {"subagent_type": "Explore", "run_in_background": true}})),
            ev("p1", "PostToolUse", serde_json::json!({"tool_name": "Agent", "tool_use_id": "tu_A", "tool_response": {"isAsync": true}})),
            ev("p1", "SubagentStart", serde_json::json!({"agent_id": "a1", "agent_type": "Explore"})),
            ev("p1", "Stop", serde_json::json!({"last_assistant_message": "started it"})),
            // The user sends another prompt while the subagent is still working.
            ev("p2", "UserPromptSubmit", serde_json::json!({"prompt": "next question"})),
            ev("p2", "PreToolUse", serde_json::json!({"agent_id": "a1", "agent_type": "Explore", "tool_name": "Grep", "tool_use_id": "tu_g"})),
            ev("p2", "PostToolUse", serde_json::json!({"agent_id": "a1", "agent_type": "Explore", "tool_name": "Grep", "tool_use_id": "tu_g"})),
            ev("p2", "SubagentStop", serde_json::json!({"agent_id": "a1", "agent_type": "Explore", "last_assistant_message": ""})),
            ev("p2", "Stop", serde_json::json!({"last_assistant_message": "answered"})),
        ];
        let store = replay(&events.join("\n"));

        let p1 = store.trace("p1").unwrap();
        let a1 = p1.iter().find(|s| s.span_id == "a1").unwrap();
        assert_eq!((a1.status, a1.error.as_deref(), a1.output.as_ref()), (Status::Ok, None, None));
        let grep = p1.iter().find(|s| s.span_id == "tu_g").expect("subagent tool stays in the subagent's trace");
        assert_eq!(grep.parent_id.as_deref(), Some("a1"));

        let p2 = store.trace("p2").unwrap();
        assert_eq!(p2.iter().map(|s| s.span_id.as_str()).collect::<Vec<_>>(), vec![PROMPT, MAIN]);
    }

    #[test]
    fn tracker_restart_mid_turn_resumes_the_existing_trace() {
        let store = replay(include_str!("../fixtures/claude-code-session.jsonl"));
        let restarted = ClaudeCode::default();
        let ev = serde_json::json!({"session_id": "sess-1", "prompt_id": "p1", "hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_use_id": "t_late"});
        let updates = restarted.handle(&ev, 99_000.0, |t| store.has_trace(t));
        assert_eq!(updates.iter().map(|u| u.span_id.as_str()).collect::<Vec<_>>(), vec!["t_late"]);
        store.ingest(updates, 99_000.0).unwrap();
        let prompt = store.trace("p1").unwrap().into_iter().find(|s| s.span_id == PROMPT).unwrap();
        assert_eq!(prompt.name, "Find the launch code and list the files");
    }

    #[test]
    fn events_without_a_prompt_open_a_trace() {
        let cc = ClaudeCode::default();
        let ev = serde_json::json!({"session_id": "s", "prompt_id": "px", "hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_use_id": "t1"});
        let ids: Vec<String> = cc.handle(&ev, 1.0, |_| false).into_iter().map(|u| u.span_id).collect();
        assert_eq!(ids, vec![PROMPT, MAIN, "t1"]);
    }

    #[test]
    fn post_without_pre_is_backdated_and_parented() {
        let cc = ClaudeCode::default();
        let ev = serde_json::json!({"session_id": "s", "hook_event_name": "PostToolUse", "tool_name": "Read", "tool_use_id": "t1", "duration_ms": 250});
        let tool = cc.handle(&ev, 1000.0, |_| false).pop().unwrap();
        assert_eq!((tool.start, tool.end, tool.parent_id.as_deref()), (Some(750.0), Some(1000.0), Some(MAIN)));
        let ev = serde_json::json!({"session_id": "s", "hook_event_name": "PostToolUse", "agent_id": "a9", "agent_type": "Explore", "tool_name": "Grep", "tool_use_id": "t2"});
        let tool = cc.handle(&ev, 1000.0, |_| false).pop().unwrap();
        assert_eq!((tool.parent_id.as_deref(), tool.agent.as_deref()), (Some("a9"), Some("Explore")));
    }

    #[test]
    fn truncate_keeps_first_line() {
        assert_eq!(truncate("short", 80), "short");
        assert_eq!(truncate("line one\nline two", 80), "line one…");
        assert_eq!(truncate("abcdef", 3), "abc…");
    }
}
