//! Maps Claude Code hook events (posted by `"type": "http"` hooks) onto spans.
//! Each submitted prompt becomes one trace: prompt -> main agent -> tool calls,
//! with subagents nested under the Agent tool call that spawned them.
//! Fields are read leniently because the hook payload schema grows over time.
//! Model calls come from the session transcript every event names, read up to
//! its end on each event, or from the transcript text a cloud session's hook
//! sends along (integrations/claude-code-web).

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{Map, Value};

use crate::model::{Kind, SpanUpdate, Status};
use crate::transcript::Transcript;

const PROMPT: &str = "prompt";
const MAIN: &str = "main";

#[derive(Default)]
pub struct ClaudeCode {
    sessions: Mutex<HashMap<String, Session>>,
    /// Transcripts are read only from here; None reads none.
    projects: Option<PathBuf>,
}

/// Where Claude Code keeps session transcripts.
pub fn projects_dir() -> Option<PathBuf> {
    std::env::home_dir().map(|home| home.join(".claude").join("projects"))
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
    /// Transcripts being followed, by resolved path.
    tails: HashMap<PathBuf, Tail>,
}

/// A transcript file: how far it has been read, and the parse state there.
struct Tail {
    offset: u64,
    transcript: Transcript,
}

impl Tail {
    fn read_new(&mut self, path: &Path, out: &mut Vec<SpanUpdate>) -> io::Result<()> {
        let mut file = File::open(path)?;
        file.seek(SeekFrom::Start(self.offset))?;
        let mut buf = vec![0; 64 << 10];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                return Ok(());
            }
            self.offset += n as u64;
            out.extend(self.transcript.feed(&buf[..n]));
        }
    }

    /// Reads text a hook sent from byte `from` of the file, skipping what was already read:
    /// hooks run concurrently and retry, so the same text can arrive twice.
    fn feed_sent(&mut self, chunk: &Value) -> Vec<SpanUpdate> {
        let text = chunk["text"].as_str().unwrap_or_default().as_bytes();
        let from = chunk["from"].as_u64().unwrap_or(self.offset);
        let seen = (self.offset.saturating_sub(from) as usize).min(text.len());
        self.offset = self.offset.max(from).saturating_add((text.len() - seen) as u64);
        self.transcript.feed(&text[seen..])
    }
}

/// Model calls appended to the event's transcripts (the session's, and the
/// subagent's on a subagent event) since they were last read. A cloud session's
/// hook sends the appended text itself as `transcript_chunks`, keyed `main` or by
/// agent id, each `{from, text}` with `from` its byte offset in the file; otherwise
/// the files are read, if they are under `projects`.
fn read_transcripts(projects: Option<&Path>, session: &mut Session, ev: &Value) -> Vec<SpanUpdate> {
    let s = |k: &str| ev.get(k).and_then(Value::as_str);
    let Some(main) = s("transcript_path").map(PathBuf::from) else { return Vec::new() };
    let mut files = vec![(MAIN, Some(main_agent(ev)), main.clone())];
    if let Some(agent) = s("agent_id") {
        let derived = main.with_extension("").join("subagents").join(format!("agent-{agent}.jsonl"));
        files.push((agent, s("agent_type"), s("agent_transcript_path").map_or(derived, PathBuf::from)));
    }
    let mut out = Vec::new();
    for (parent, agent, path) in files {
        let sent = ev.get("transcript_chunks").and_then(|c| c.get(parent)).filter(|c| c.is_object());
        let path = match sent {
            Some(_) => path,
            None => match projects.and_then(|p| transcript_file(p, &path)) {
                Some(path) => path,
                None => continue,
            },
        };
        let tail = session.tails.entry(path.clone()).or_insert_with(|| Tail { offset: 0, transcript: Transcript::new(parent, agent) });
        match sent {
            Some(chunk) => out.extend(tail.feed_sent(chunk)),
            // A transcript that can't be read only means no model calls this time.
            None => drop(tail.read_new(&path, &mut out)),
        }
        // A stopped subagent writes nothing more.
        if parent != MAIN && s("hook_event_name") == Some("SubagentStop") {
            session.tails.remove(&path);
        }
    }
    out
}

/// `path` resolved, if it is a `.jsonl` file under `projects`. Paths come from
/// the request, so nothing else is ever opened.
fn transcript_file(projects: &Path, path: &Path) -> Option<PathBuf> {
    let path = fs::canonicalize(path).ok()?;
    let inside = path.starts_with(fs::canonicalize(projects).ok()?);
    (inside && path.extension().is_some_and(|e| e == "jsonl") && path.is_file()).then_some(path)
}

impl Session {
    /// Ends every open span as failed (the session is over).
    fn close_open(&mut self, now: f64, reason: &str) -> Vec<SpanUpdate> {
        std::mem::take(&mut self.open)
            .into_iter()
            .map(|o| SpanUpdate {
                end: Some(now),
                status: Some(Status::Error),
                error: Some(reason.to_string()),
                ..SpanUpdate::new(o.trace_id, o.span_id)
            })
            .collect()
    }

    /// Ends the previous turn's agent normally. Claude Code takes messages mid-turn
    /// and keeps working, so a new prompt is not an interruption; tools still
    /// running finish later in the trace they started in.
    fn hand_over(&mut self, now: f64) -> Option<SpanUpdate> {
        let i = self.open.iter().position(|o| o.span_id == MAIN)?;
        let main = self.open.remove(i);
        let mut attrs = Map::new();
        attrs.insert("ended_by".into(), "next prompt".into());
        Some(SpanUpdate { end: Some(now), status: Some(Status::Ok), attrs, ..SpanUpdate::new(main.trace_id, MAIN) })
    }

    fn opened(&mut self, trace_id: &str, span_id: &str) {
        self.open.push(OpenSpan { trace_id: trace_id.to_string(), span_id: span_id.to_string() });
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
    /// Also reads model calls from session transcripts under `projects` (see [`projects_dir`]).
    pub fn new(projects: Option<PathBuf>) -> Self {
        ClaudeCode { projects, ..Default::default() }
    }

    /// `known` says whether a trace is already stored, so a tracker restarted
    /// mid-turn resumes that turn's trace instead of re-creating its prompt.
    pub fn handle(&self, ev: &Value, now: f64, known: impl Fn(&str) -> bool) -> Vec<SpanUpdate> {
        let s = |k: &str| ev.get(k).and_then(Value::as_str);
        let Some(session_id) = s("session_id") else { return vec![] };
        let event = s("hook_event_name").unwrap_or_default();
        let mut sessions = self.sessions.lock().unwrap();
        let session = sessions.entry(session_id.to_string()).or_default();
        // Turns the tracker never saw (it started mid-session) or that were
        // deleted stay out, rather than coming back as traces of bare model calls.
        let current: Vec<String> = [s("prompt_id"), session.trace_id.as_deref()].into_iter().flatten().map(str::to_string).collect();
        let mut out = read_transcripts(self.projects.as_deref(), session, ev);
        out.retain(|u| known(&u.trace_id) || current.contains(&u.trace_id));

        match event {
            // A cloud session hands a subagent's result back as a prompt of the turn already running.
            "UserPromptSubmit" if s("prompt_id").is_some() && s("prompt_id") == session.trace_id.as_deref() => return out,
            "UserPromptSubmit" => {
                out.extend(session.hand_over(now));
                let trace_id = s("prompt_id").map_or_else(|| format!("{session_id}-{now}"), str::to_string);
                out.extend(begin(session, trace_id, s("prompt"), ev, now));
                return out;
            }
            "SessionEnd" => {
                out.extend(session.close_open(now, "session ended"));
                sessions.remove(session_id);
                return out;
            }
            _ => {}
        }

        // Events before any prompt (the tracker started mid-turn) open a trace of their own.
        if session.trace_id.is_none() {
            let trace_id = s("prompt_id").map_or_else(|| format!("{session_id}-{now}"), str::to_string);
            if known(&trace_id) {
                session.opened(&trace_id, MAIN);
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

        // A subagent already running when the tracker started never sent SubagentStart;
        // adopt it into the current turn so its steps hang off a real agent span.
        if let Some(agent) = agent_id
            && !matches!(event, "SubagentStart" | "SubagentStop")
            && !session.agent_traces.contains_key(agent)
        {
            session.agent_traces.insert(agent.to_string(), trace_id.clone());
            session.opened(&trace_id, agent);
            out.push(SpanUpdate {
                parent_id: Some(MAIN.into()),
                kind: Some(Kind::Agent),
                name: s("agent_type").map(str::to_string),
                agent: s("agent_type").map(str::to_string),
                start: Some(now),
                ..span(agent)
            });
        }

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
                session.opened(&trace_id, tool_use_id);
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
                // A tool started before a mid-turn prompt finishes in its own trace.
                let started_in = session.open.iter().find(|o| o.span_id == tool_use_id).map(|o| o.trace_id.clone());
                let trace_id = started_in.unwrap_or_else(|| trace_id.clone());
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
                    ..SpanUpdate::new(trace_id, tool_use_id)
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
                session.opened(&trace_id, agent_id);
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
                // An agent seen only stopping is one Claude Code ran for itself, with nothing to show.
                let Some(agent_id) = agent_id.filter(|a| session.agent_traces.contains_key(*a)) else { return out };
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
    session.opened(&trace_id, MAIN);
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
            agent: Some(main_agent(ev).to_string()),
            start: Some(now),
            ..SpanUpdate::new(trace_id, MAIN)
        },
    ]
}

/// The session's own agent. On a subagent's event, agent_type names the subagent instead.
fn main_agent(ev: &Value) -> &str {
    let own = if ev.get("agent_id").is_some() { None } else { ev.get("agent_type").and_then(Value::as_str) };
    own.unwrap_or("claude-code")
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
    use std::fs::OpenOptions;
    use std::io::Write;

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
    fn a_subagent_event_opening_the_trace_does_not_rename_the_main_agent() {
        let main_agent = |ev: Value| {
            let updates = ClaudeCode::default().handle(&ev, 1.0, |_| false);
            updates.into_iter().find(|u| u.span_id == MAIN).and_then(|u| u.agent)
        };
        let ev = serde_json::json!({"session_id": "s", "prompt_id": "p", "hook_event_name": "PreToolUse", "agent_id": "a1", "agent_type": "Explore", "tool_name": "Grep", "tool_use_id": "t1"});
        assert_eq!(main_agent(ev).as_deref(), Some("claude-code"));
        // A session started with --agent is still named after that agent.
        let ev = serde_json::json!({"session_id": "s", "prompt_id": "p", "hook_event_name": "UserPromptSubmit", "agent_type": "reviewer", "prompt": "hi"});
        assert_eq!(main_agent(ev).as_deref(), Some("reviewer"));
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

    const TRANSCRIPT: &str = include_str!("../fixtures/claude-code-transcript/sess-t.jsonl");
    const SUBAGENT_TRANSCRIPT: &str = include_str!("../fixtures/claude-code-transcript/sess-t/subagents/agent-a1.jsonl");

    /// A fresh `<base>/projects/proj/sess-t/subagents`, laid out like `~/.claude/projects`. Returns `<base>`.
    fn scratch(name: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("agent-tracker-test-cc-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("projects/proj/sess-t/subagents")).unwrap();
        base
    }

    fn event(transcript: &Path, name: &str, extra: Value) -> Value {
        let mut e = serde_json::json!({"session_id": "sess-t", "prompt_id": "p1", "transcript_path": transcript, "hook_event_name": name});
        e.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        e
    }

    /// (span id, parent, end) of each llm span in the trace.
    fn llm_spans(store: &Store, trace_id: &str) -> Vec<(String, String, Option<f64>)> {
        let spans = store.trace(trace_id).unwrap_or_default().into_iter().filter(|s| s.kind == Kind::Llm);
        spans.map(|s| (s.span_id, s.parent_id.unwrap_or_default(), s.end)).collect()
    }

    #[test]
    fn model_calls_are_read_as_the_transcript_grows() {
        let base = scratch("tail");
        let path = base.join("projects/proj/sess-t.jsonl");
        // The first write stops halfway through msg_02's line.
        let cut = TRANSCRIPT.find(r#""msg_02""#).unwrap();
        fs::write(&path, &TRANSCRIPT[..cut]).unwrap();
        let store = Store::in_memory();
        let cc = ClaudeCode::new(Some(base.join("projects")));
        let send = |ev: Value, t: f64| store.ingest(cc.handle(&ev, t, |id| store.has_trace(id)), t).unwrap();
        let t0 = 1_768_471_200_000.0; // 2026-01-15T10:00:00Z, where the fixture starts

        send(event(&path, "UserPromptSubmit", serde_json::json!({"prompt": "Find the launch code"})), t0);
        assert_eq!(llm_spans(&store, "p1"), [("msg_01".into(), MAIN.into(), Some(t0 + 3000.0))]);

        let append = |from: usize, to: usize| OpenOptions::new().append(true).open(&path).unwrap().write_all(&TRANSCRIPT.as_bytes()[from..to]).unwrap();
        // The second turn starts at its user record; until its prompt event arrives it isn't written.
        let turn2 = TRANSCRIPT[..TRANSCRIPT.find(r#""p2""#).unwrap()].rfind('\n').unwrap() + 1;
        append(cut, turn2);
        send(event(&path, "Stop", serde_json::json!({"last_assistant_message": "The launch code is 42."})), t0 + 10_100.0);
        let spans = llm_spans(&store, "p1");
        assert_eq!(spans.iter().map(|s| s.0.as_str()).collect::<Vec<_>>(), ["msg_01", "msg_02"]);
        assert_eq!(spans[1], ("msg_02".into(), MAIN.into(), Some(t0 + 10_000.0)));
        // The next turn's call lands in that turn's trace.
        append(turn2, TRANSCRIPT.len());
        send(event(&path, "UserPromptSubmit", serde_json::json!({"prompt_id": "p2", "prompt": "And the files?"})), t0 + 60_000.0);
        assert_eq!(llm_spans(&store, "p2"), [("msg_03".into(), MAIN.into(), Some(t0 + 62_000.0))]);
        let p1 = store.trace("p1").unwrap();
        let by = |id: &str| p1.iter().find(|s| s.span_id == id).unwrap_or_else(|| panic!("no span {id}"));
        assert_eq!(by("msg_02").output, Some(Value::from("The launch code is 42.")));
        let reply = by("msg_02:m0");
        assert_eq!((reply.kind, reply.parent_id.as_deref(), reply.agent.as_deref()), (Kind::Message, Some("msg_02"), Some("claude-code")));
        assert_eq!(by("msg_01:r0").kind, Kind::Reasoning);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_subagent_already_running_when_the_tracker_starts_becomes_an_agent_span() {
        let store = Store::in_memory();
        let cc = ClaudeCode::default();
        let send = |ev: Value, t: f64| store.ingest(cc.handle(&ev, t, |id| store.has_trace(id)), t).unwrap();
        let ev = |name: &str, extra: Value| {
            let mut e = serde_json::json!({"session_id": "s", "prompt_id": "p1", "hook_event_name": name});
            e.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            e
        };
        send(ev("UserPromptSubmit", serde_json::json!({"prompt": "go"})), 1.0);
        // This subagent started before the tracker did, so its SubagentStart was never seen.
        let sub = serde_json::json!({"agent_id": "a7", "agent_type": "general-purpose"});
        let mut tool = sub.clone();
        tool.as_object_mut().unwrap().extend(serde_json::json!({"tool_name": "Bash", "tool_use_id": "t1"}).as_object().unwrap().clone());
        send(ev("PreToolUse", tool.clone()), 2.0);
        send(ev("PostToolUse", tool), 3.0);
        send(ev("SubagentStop", sub), 4.0);

        let spans = store.trace("p1").unwrap();
        let by = |id: &str| spans.iter().find(|s| s.span_id == id).unwrap_or_else(|| panic!("no span {id}"));
        let agent = by("a7");
        assert_eq!((agent.kind, agent.name.as_str(), agent.agent.as_deref()), (Kind::Agent, "general-purpose", Some("general-purpose")));
        assert_eq!((agent.parent_id.as_deref(), agent.start, agent.end, agent.status), (Some(MAIN), 2.0, Some(4.0), Status::Ok));
        assert_eq!(by("t1").parent_id.as_deref(), Some("a7"));
    }

    #[test]
    fn a_prompt_sent_mid_turn_does_not_fail_the_running_turn() {
        let store = Store::in_memory();
        let cc = ClaudeCode::default();
        let send = |ev: Value, t: f64| store.ingest(cc.handle(&ev, t, |id| store.has_trace(id)), t).unwrap();
        let ev = |p: &str, name: &str, extra: Value| {
            let mut e = serde_json::json!({"session_id": "s", "prompt_id": p, "hook_event_name": name});
            e.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            e
        };
        send(ev("p1", "UserPromptSubmit", serde_json::json!({"prompt": "build it"})), 1.0);
        send(ev("p1", "PreToolUse", serde_json::json!({"tool_name": "Bash", "tool_use_id": "t1"})), 2.0);
        // The user types while the Bash call runs; Claude Code takes the message and keeps going.
        send(ev("p2", "UserPromptSubmit", serde_json::json!({"prompt": "also run the tests"})), 3.0);
        send(ev("p2", "PostToolUse", serde_json::json!({"tool_name": "Bash", "tool_use_id": "t1"})), 4.0);
        send(ev("p2", "Stop", serde_json::json!({"last_assistant_message": "done"})), 5.0);

        let p1 = store.trace("p1").unwrap();
        let by = |id: &str| p1.iter().find(|s| s.span_id == id).unwrap_or_else(|| panic!("no span {id}"));
        let main = by(MAIN);
        assert_eq!((main.status, main.error.as_deref(), main.end), (Status::Ok, None, Some(3.0)));
        assert_eq!(main.attrs.get("ended_by"), Some(&Value::from("next prompt")));
        let bash = by("t1");
        assert_eq!((bash.status, bash.end), (Status::Ok, Some(4.0)), "the tool finishes in the turn it started in");
        let p2 = store.trace("p2").unwrap();
        assert!(p2.iter().all(|s| s.span_id != "t1"));
        assert_eq!(p2.iter().find(|s| s.span_id == MAIN).unwrap().status, Status::Ok);
    }

    #[test]
    fn a_message_handed_into_the_running_turn_stays_part_of_it() {
        let store = Store::in_memory();
        let cc = ClaudeCode::default();
        let send = |ev: Value, t: f64| store.ingest(cc.handle(&ev, t, |id| store.has_trace(id)), t).unwrap();
        let ev = |name: &str, extra: Value| {
            let mut e = serde_json::json!({"session_id": "s", "prompt_id": "p1", "hook_event_name": name});
            e.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            e
        };
        send(ev("UserPromptSubmit", serde_json::json!({"prompt": "read the notes"})), 1.0);
        // A cloud session hands a subagent's result back as a prompt of the same turn.
        send(ev("UserPromptSubmit", serde_json::json!({"prompt": "<agent-message from=\"a1\">\nthe notes say hi"})), 2.0);
        send(ev("Stop", serde_json::json!({"last_assistant_message": "done"})), 3.0);

        let spans = store.trace("p1").unwrap();
        let by = |id: &str| spans.iter().find(|s| s.span_id == id).unwrap_or_else(|| panic!("no span {id}"));
        assert_eq!(by(PROMPT).name, "read the notes");
        let main = by(MAIN);
        assert_eq!((main.start, main.end, main.attrs.get("ended_by")), (1.0, Some(3.0), None));
    }

    #[test]
    fn an_agent_seen_only_stopping_is_left_out() {
        let store = Store::in_memory();
        let cc = ClaudeCode::default();
        let send = |ev: Value, t: f64| store.ingest(cc.handle(&ev, t, |id| store.has_trace(id)), t).unwrap();
        let ev = |name: &str, extra: Value| {
            let mut e = serde_json::json!({"session_id": "s", "prompt_id": "p1", "hook_event_name": name});
            e.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            e
        };
        send(ev("UserPromptSubmit", serde_json::json!({"prompt": "hi"})), 1.0);
        send(ev("Stop", serde_json::json!({"last_assistant_message": "hello"})), 2.0);
        // Claude Code runs agents of its own after a reply, with no type and no SubagentStart.
        send(ev("SubagentStop", serde_json::json!({"agent_id": "a9", "agent_type": ""})), 3.0);

        let ids: Vec<String> = store.trace("p1").unwrap().into_iter().map(|s| s.span_id).collect();
        assert_eq!(ids, vec![PROMPT, MAIN]);
    }

    #[test]
    fn starting_mid_session_skips_turns_the_tracker_never_saw() {
        let base = scratch("midsession");
        let path = base.join("projects/proj/sess-t.jsonl");
        fs::write(&path, TRANSCRIPT).unwrap();
        let store = Store::in_memory();
        let cc = ClaudeCode::new(Some(base.join("projects")));
        // The first event the tracker sees is from the session's second turn.
        let ev = event(&path, "PreToolUse", serde_json::json!({"prompt_id": "p2", "tool_name": "Bash", "tool_use_id": "t9"}));
        store.ingest(cc.handle(&ev, 1.0, |id| store.has_trace(id)), 1.0).unwrap();
        assert!(store.trace("p1").is_none(), "an earlier turn stays out instead of becoming a trace of bare model calls");
        assert_eq!(llm_spans(&store, "p2").len(), 1, "the current turn's calls are kept");
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn subagent_events_read_the_subagent_transcript() {
        let base = scratch("subagent");
        let path = base.join("projects/proj/sess-t.jsonl");
        fs::write(&path, "").unwrap();
        fs::write(base.join("projects/proj/sess-t/subagents/agent-a1.jsonl"), SUBAGENT_TRANSCRIPT).unwrap();
        // SubagentStop names its transcript, wherever it is.
        let moved = base.join("projects/proj/moved-a2.jsonl");
        fs::write(&moved, SUBAGENT_TRANSCRIPT.replace("msg_s", "msg_t")).unwrap();
        let store = Store::in_memory();
        let cc = ClaudeCode::new(Some(base.join("projects")));
        let send = |ev: Value| store.ingest(cc.handle(&ev, 1.0, |id| store.has_trace(id)), 1.0).unwrap();

        send(event(&path, "UserPromptSubmit", serde_json::json!({"prompt": "Find the launch code"})));
        send(event(&path, "SubagentStart", serde_json::json!({"agent_id": "a1", "agent_type": "Explore"})));
        send(event(&path, "SubagentStop", serde_json::json!({"agent_id": "a2", "agent_type": "Explore", "agent_transcript_path": moved})));
        let mut parents: Vec<(String, String)> = llm_spans(&store, "p1").into_iter().map(|(id, parent, _)| (id, parent)).collect();
        parents.sort();
        let expected = [("msg_s1", "a1"), ("msg_s2", "a1"), ("msg_t1", "a2"), ("msg_t2", "a2")];
        assert_eq!(parents, expected.map(|(id, parent)| (id.to_string(), parent.to_string())));
        let reply = store.trace("p1").unwrap().into_iter().find(|s| s.span_id == "msg_s2:m0").expect("the subagent's reply");
        assert_eq!((reply.parent_id.as_deref(), reply.agent.as_deref()), (Some("msg_s2"), Some("Explore")));
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn transcript_text_sent_with_the_events_is_read_like_a_local_transcript() {
        // A cloud session: the tracker can't open the path, so the hook sends what was appended.
        let path = Path::new("/home/user/.claude/projects/-home-user-repo/sess-t.jsonl");
        let cut = TRANSCRIPT.find(r#""msg_02""#).unwrap();
        let turn2 = TRANSCRIPT[..TRANSCRIPT.find(r#""p2""#).unwrap()].rfind('\n').unwrap() + 1;
        let chunk = |from: usize, to: usize| serde_json::json!({"from": from, "text": &TRANSCRIPT[from..to]});
        let t0 = 1_768_471_200_000.0;
        // `overlap` resends the first turn's text, as two hooks running at once (or a retry) would.
        let run = |overlap: usize| {
            let store = Store::in_memory();
            let cc = ClaudeCode::new(None);
            let send = |ev: Value, t: f64| store.ingest(cc.handle(&ev, t, |id| store.has_trace(id)), t).unwrap();
            send(event(path, "UserPromptSubmit", serde_json::json!({"prompt": "Find the launch code", "transcript_chunks": {"main": chunk(0, cut)}})), t0);
            assert_eq!(llm_spans(&store, "p1"), [("msg_01".into(), MAIN.into(), Some(t0 + 3000.0))]);
            let main = chunk(cut - overlap, turn2);
            let sub = serde_json::json!({"from": 0, "text": SUBAGENT_TRANSCRIPT});
            send(event(path, "SubagentStop", serde_json::json!({"agent_id": "a1", "agent_type": "Explore", "transcript_chunks": {"main": main, "a1": sub}})), t0 + 10_100.0);
            send(event(path, "UserPromptSubmit", serde_json::json!({"prompt_id": "p2", "prompt": "And the files?", "transcript_chunks": {"main": chunk(turn2, TRANSCRIPT.len())}})), t0 + 60_000.0);
            store
        };

        let store = run(0);
        let mut p1: Vec<(String, String)> = llm_spans(&store, "p1").into_iter().map(|(id, parent, _)| (id, parent)).collect();
        p1.sort();
        let expected = [("msg_01", MAIN), ("msg_02", MAIN), ("msg_s1", "a1"), ("msg_s2", "a1")];
        assert_eq!(p1, expected.map(|(id, parent)| (id.to_string(), parent.to_string())));
        assert_eq!(llm_spans(&store, "p2"), [("msg_03".into(), MAIN.into(), Some(t0 + 62_000.0))]);
        assert_eq!(store.trace("p1").unwrap().iter().find(|s| s.span_id == "msg_01:r0").map(|s| s.kind), Some(Kind::Reasoning));

        let outputs = |store: &Store| store.trace("p1").unwrap().into_iter().map(|s| (s.span_id, s.output)).collect::<Vec<_>>();
        assert_eq!(outputs(&run(cut)), outputs(&store), "text sent twice is read once");

        // Offsets come from the request; an absurd one must not take hook handling down.
        let cc = ClaudeCode::new(None);
        for from in [u64::MAX, u64::MAX] {
            let ev = event(path, "Stop", serde_json::json!({"transcript_chunks": {"main": {"from": from, "text": "{}\n"}}}));
            cc.handle(&ev, t0, |_| true);
        }
    }

    #[test]
    fn only_jsonl_files_under_the_projects_dir_are_read() {
        let base = scratch("outside");
        let projects = base.join("projects");
        fs::create_dir_all(base.join("elsewhere")).unwrap();
        for p in ["elsewhere/sess-t.jsonl", "projects/proj/sess-t.txt", "projects/proj/sess-t.jsonl"] {
            fs::write(base.join(p), TRANSCRIPT).unwrap();
        }
        let cc = ClaudeCode::new(Some(projects.clone()));
        let span_ids = |i: usize, path: PathBuf| {
            let ev = event(&path, "UserPromptSubmit", serde_json::json!({"session_id": format!("s{i}"), "prompt": "hi"}));
            cc.handle(&ev, 1.0, |_| false).into_iter().map(|u| u.span_id).collect::<Vec<_>>()
        };
        let refused = [
            base.join("elsewhere/sess-t.jsonl"),
            projects.join("proj/../../elsewhere/sess-t.jsonl"),
            projects.join("proj/sess-t.txt"),
            projects.join("proj/missing.jsonl"),
            projects.join("proj"),
        ];
        for (i, path) in refused.into_iter().enumerate() {
            assert_eq!(span_ids(i, path.clone()), [PROMPT, MAIN], "{}", path.display());
        }
        let read = span_ids(99, projects.join("proj/sess-t.jsonl"));
        // The event's turn is p1, so only p1's two calls; p2 is a turn the tracker hasn't seen.
        assert_eq!(read.iter().filter(|id| id.starts_with("msg_") && !id.contains(':')).count(), 2, "{read:?}");
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn truncate_keeps_first_line() {
        assert_eq!(truncate("short", 80), "short");
        assert_eq!(truncate("line one\nline two", 80), "line one…");
        assert_eq!(truncate("abcdef", 3), "abc…");
    }
}
