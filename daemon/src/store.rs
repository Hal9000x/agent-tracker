use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::model::{Span, SpanUpdate, TraceSummary};

/// What live subscribers receive after each ingested batch.
#[derive(Debug, Serialize)]
pub struct Update {
    pub spans: Vec<Span>,
    pub traces: Vec<TraceSummary>,
}

/// One line of the append-only log. `t` is the receive time, so replay fills
/// missing start times exactly as the live ingest did.
#[derive(Serialize, Deserialize)]
struct LogEntry {
    t: f64,
    span: SpanUpdate,
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

    /// Replays `dir/spans.jsonl` and appends new updates to it. A torn last line
    /// (crash mid-write) is skipped.
    pub fn open(dir: &Path) -> io::Result<Store> {
        fs::create_dir_all(dir)?;
        let path = dir.join("spans.jsonl");
        let mut state = State::default();
        if path.exists() {
            for line in BufReader::new(File::open(&path)?).lines() {
                if let Ok(e) = serde_json::from_str::<LogEntry>(&line?) {
                    state.apply(e.span, e.t);
                }
            }
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
                serde_json::to_writer(&mut *w, &LogEntry { t: received, span: u.clone() })?;
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
        let _ = self.tx.send(Arc::new(Update { spans, traces }));
        Ok(())
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
