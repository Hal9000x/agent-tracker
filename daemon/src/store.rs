use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::model::{Span, SpanUpdate, TraceSummary};

/// What live subscribers receive after each ingested batch or delete.
#[derive(Debug, Serialize)]
pub struct Update {
    pub spans: Vec<Span>,
    pub traces: Vec<TraceSummary>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub deleted: Vec<String>,
}

/// One line of the append-only log: a span update, or a tombstone for a deleted
/// trace. `t` is the receive time, so replay fills missing start times exactly
/// as the live ingest did.
#[derive(Serialize, Deserialize)]
struct LogEntry {
    t: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    span: Option<SpanUpdate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    delete: Option<String>,
}

#[derive(Default)]
struct State {
    traces: HashMap<String, HashMap<String, Span>>,
    seq: u64,
}

impl State {
    fn apply(&mut self, u: SpanUpdate, received: f64) -> (String, String) {
        self.seq += 1;
        let seq = self.seq;
        let key = (u.trace_id.clone(), u.span_id.clone());
        let trace = self.traces.entry(u.trace_id.clone()).or_default();
        match trace.get_mut(&u.span_id) {
            Some(span) => span.apply(u, seq),
            None => {
                trace.insert(u.span_id.clone(), Span::from_update(u, received, seq));
            }
        }
        key
    }
}

pub struct Store {
    state: Mutex<State>,
    log: Option<Mutex<BufWriter<File>>>,
    tx: broadcast::Sender<Arc<Update>>,
}

impl Store {
    #[cfg(test)]
    pub fn in_memory() -> Store {
        Store { state: Mutex::default(), log: None, tx: broadcast::channel(1024).0 }
    }

    /// Replays `dir/spans.jsonl` and appends new updates to it. Unreadable lines
    /// (a torn last line after a crash) are skipped and counted.
    pub fn open(dir: &Path) -> io::Result<Store> {
        fs::create_dir_all(dir)?;
        let path = dir.join("spans.jsonl");
        let mut state = State::default();
        let mut skipped = 0;
        if path.exists() {
            for line in BufReader::new(File::open(&path)?).lines() {
                let line = line?;
                // Ingest already bounded the depth; the log wrapper adds two levels on top.
                let mut de = serde_json::Deserializer::from_str(&line);
                de.disable_recursion_limit();
                match LogEntry::deserialize(&mut de) {
                    Ok(LogEntry { t, span: Some(span), .. }) => {
                        state.apply(span, t);
                    }
                    Ok(LogEntry { delete: Some(trace_id), .. }) => {
                        state.traces.remove(&trace_id);
                    }
                    _ => skipped += 1,
                }
            }
        }
        if skipped > 0 {
            eprintln!("skipped {skipped} unreadable line(s) in {}", path.display());
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Store {
            state: Mutex::new(state),
            log: Some(Mutex::new(BufWriter::new(file))),
            tx: broadcast::channel(1024).0,
        })
    }

    pub fn ingest(&self, updates: Vec<SpanUpdate>, received: f64) -> io::Result<()> {
        if updates.is_empty() {
            return Ok(());
        }
        if let Some(log) = &self.log {
            let mut w = log.lock().unwrap();
            for u in &updates {
                serde_json::to_writer(&mut *w, &LogEntry { t: received, span: Some(u.clone()), delete: None })?;
                w.write_all(b"\n")?;
            }
            w.flush()?;
        }
        let mut state = self.state.lock().unwrap();
        let mut touched: Vec<(String, String)> = Vec::new();
        for u in updates {
            let key = state.apply(u, received);
            if !touched.contains(&key) {
                touched.push(key);
            }
        }
        let spans = touched.iter().map(|(t, s)| state.traces[t][s].clone()).collect();
        let mut trace_ids: Vec<&String> = Vec::new();
        for (t, _) in &touched {
            if !trace_ids.contains(&t) {
                trace_ids.push(t);
            }
        }
        let traces = trace_ids
            .into_iter()
            .filter_map(|t| TraceSummary::of(t, state.traces[t].values()))
            .collect();
        // No subscribers is fine.
        let _ = self.tx.send(Arc::new(Update { spans, traces, deleted: Vec::new() }));
        Ok(())
    }

    /// Removes a trace; a tombstone in the log keeps it removed after a restart.
    /// Returns whether it existed.
    pub fn delete(&self, trace_id: &str, received: f64) -> io::Result<bool> {
        let mut state = self.state.lock().unwrap();
        if !state.traces.contains_key(trace_id) {
            return Ok(false);
        }
        if let Some(log) = &self.log {
            let mut w = log.lock().unwrap();
            serde_json::to_writer(&mut *w, &LogEntry { t: received, span: None, delete: Some(trace_id.to_string()) })?;
            w.write_all(b"\n")?;
            w.flush()?;
        }
        state.traces.remove(trace_id);
        let _ = self.tx.send(Arc::new(Update { spans: Vec::new(), traces: Vec::new(), deleted: vec![trace_id.to_string()] }));
        Ok(true)
    }

    /// Newest first.
    pub fn traces(&self) -> Vec<TraceSummary> {
        let state = self.state.lock().unwrap();
        let mut out: Vec<TraceSummary> =
            state.traces.iter().filter_map(|(id, spans)| TraceSummary::of(id, spans.values())).collect();
        out.sort_by(|a, b| b.start.total_cmp(&a.start));
        out
    }

    pub fn has_trace(&self, trace_id: &str) -> bool {
        self.state.lock().unwrap().traces.contains_key(trace_id)
    }

    /// Spans ordered by start time.
    pub fn trace(&self, trace_id: &str) -> Option<Vec<Span>> {
        let state = self.state.lock().unwrap();
        let mut spans: Vec<Span> = state.traces.get(trace_id)?.values().cloned().collect();
        spans.sort_by(|a, b| a.start.total_cmp(&b.start).then(a.seq.cmp(&b.seq)));
        Some(spans)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Update>> {
        self.tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Kind, Status};

    fn scratch_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("agent-tracker-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn batch() -> Vec<Vec<SpanUpdate>> {
        vec![
            vec![SpanUpdate { kind: Some(Kind::Agent), name: Some("planner".into()), ..SpanUpdate::new("t1", "a") }],
            vec![SpanUpdate { parent_id: Some("a".into()), kind: Some(Kind::Tool), start: Some(5.0), ..SpanUpdate::new("t1", "b") }],
            vec![
                SpanUpdate { end: Some(9.0), ..SpanUpdate::new("t1", "b") },
                SpanUpdate { end: Some(10.0), ..SpanUpdate::new("t1", "a") },
            ],
        ]
    }

    #[test]
    fn replay_reproduces_live_state() {
        let dir = scratch_dir("replay");
        let live = Store::open(&dir).unwrap();
        for (i, b) in batch().into_iter().enumerate() {
            live.ingest(b, i as f64).unwrap();
        }
        let replayed = Store::open(&dir).unwrap();
        assert_eq!(live.trace("t1"), replayed.trace("t1"));
        assert_eq!(live.traces(), replayed.traces());
        // The agent had no start; it took the receive time of its first update.
        assert_eq!(replayed.trace("t1").unwrap()[0].start, 0.0);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn torn_last_line_is_skipped() {
        let dir = scratch_dir("torn");
        let s = Store::open(&dir).unwrap();
        s.ingest(batch().remove(0), 1.0).unwrap();
        drop(s);
        let mut f = OpenOptions::new().append(true).open(dir.join("spans.jsonl")).unwrap();
        f.write_all(b"{\"t\":2,\"span\":{\"trace_id\":\"t1\",\"sp").unwrap();
        let s = Store::open(&dir).unwrap();
        assert_eq!(s.trace("t1").unwrap().len(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn deleting_a_trace_survives_replay_and_is_broadcast() {
        let dir = scratch_dir("delete");
        let s = Store::open(&dir).unwrap();
        for b in batch() {
            s.ingest(b, 0.0).unwrap();
        }
        s.ingest(vec![SpanUpdate::new("keep", "x")], 1.0).unwrap();
        let mut rx = s.subscribe();
        assert!(s.delete("t1", 2.0).unwrap());
        assert!(!s.delete("t1", 2.0).unwrap(), "already gone");
        assert_eq!(rx.try_recv().unwrap().deleted, vec!["t1".to_string()]);
        assert!(s.trace("t1").is_none());
        drop(s);

        let replayed = Store::open(&dir).unwrap();
        assert!(replayed.trace("t1").is_none());
        assert!(replayed.trace("keep").is_some());
        // The id can be reused after a delete.
        replayed.ingest(vec![SpanUpdate::new("t1", "fresh")], 3.0).unwrap();
        assert_eq!(replayed.trace("t1").unwrap().len(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn replay_keeps_deep_payloads_that_ingest_accepted() {
        let dir = scratch_dir("deep");
        let deep: serde_json::Value = serde_json::from_str(&format!("{}1{}", "[".repeat(126), "]".repeat(126))).unwrap();
        let s = Store::open(&dir).unwrap();
        s.ingest(vec![SpanUpdate { input: Some(deep), ..SpanUpdate::new("deep", "a") }], 1.0).unwrap();
        drop(s);
        assert!(Store::open(&dir).unwrap().trace("deep").is_some());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn ingest_broadcasts_merged_spans_and_summaries() {
        let s = Store::in_memory();
        let mut rx = s.subscribe();
        for b in batch() {
            s.ingest(b, 0.0).unwrap();
        }
        let last = (0..3).map(|_| rx.try_recv().unwrap()).last().unwrap();
        assert_eq!(last.spans.len(), 2);
        assert_eq!(last.traces.len(), 1);
        assert_eq!(last.traces[0].status, Status::Ok);
        assert_eq!(last.traces[0].end, Some(10.0));
    }
}
