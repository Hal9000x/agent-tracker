//! A recorded run of the TypeScript demo (a single agent, a research swarm and
//! a support handoff), replayed in real time so the app can be tried without
//! any client. Regenerate the recording with `scripts/record-demo.mjs`.

use serde::Deserialize;

use crate::model::SpanUpdate;

const RECORDING: &str = include_str!("../demo/recording.jsonl");

#[derive(Deserialize)]
struct Step {
    /// Milliseconds after the start of the recording.
    t: f64,
    span: SpanUpdate,
}

/// The recording as (send at, update) pairs in time order, with times moved to
/// start at `now` and trace ids made unique to this `run`.
pub fn plan(now: f64, run: u64) -> Vec<(f64, SpanUpdate)> {
    let mut steps: Vec<(f64, SpanUpdate)> = RECORDING
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let Step { t, mut span } = serde_json::from_str(l).expect("demo recording is valid JSON");
            span.trace_id = format!("demo-{run}-{}", span.trace_id);
            span.start = span.start.map(|s| s + now);
            span.end = span.end.map(|e| e + now);
            (t + now, span)
        })
        .collect();
    steps.sort_by(|a, b| a.0.total_cmp(&b.0));
    steps
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Status;
    use crate::store::Store;

    #[test]
    fn replays_the_three_demo_runs_from_now() {
        let now = 1_000_000.0;
        let steps = plan(now, 7);
        assert!(steps.windows(2).all(|w| w[0].0 <= w[1].0), "in time order");
        assert!(steps.iter().all(|(at, s)| *at >= now && s.start.is_none_or(|st| st >= now)));

        let store = Store::in_memory();
        for (at, span) in steps {
            store.ingest(vec![span], at).unwrap();
        }
        let mut traces: Vec<(String, usize, Status)> = store.traces().into_iter().map(|t| (t.name, t.spans, t.status)).collect();
        traces.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            traces,
            vec![
                ("Double charge on March invoice".into(), 7, Status::Error),
                ("Market brief: solid-state batteries".into(), 28, Status::Error),
                ("Weather in Lisbon tomorrow?".into(), 5, Status::Ok),
            ]
        );
        assert!(store.traces().iter().all(|t| t.trace_id.starts_with("demo-7-")));
    }

    #[test]
    fn each_run_gets_its_own_trace_ids() {
        let ids = |run| plan(0.0, run).into_iter().map(|(_, s)| s.trace_id).collect::<std::collections::HashSet<_>>();
        assert!(ids(1).is_disjoint(&ids(2)));
    }
}
