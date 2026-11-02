//! Claude Code on the web. A cloud session can't reach this machine, but its hook
//! (integrations/claude-code-web) commits each event to the branch
//! `agent-tracker/<session>` of the session's repository. This fetches those branches
//! into the local clones `install-web` registered and returns the events not yet handled.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

/// The registered clones, in the data folder.
const REPOS: &str = "web-repos.json";
/// How much of each session's events.jsonl has been handled, so a restart doesn't repeat it.
const DONE: &str = "web-sync.json";

fn read_json<T: serde::de::DeserializeOwned + Default>(path: &Path) -> T {
    fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn write_json(path: &Path, value: &impl serde::Serialize) -> io::Result<()> {
    fs::create_dir_all(path.parent().unwrap_or(Path::new(".")))?;
    fs::write(path, serde_json::to_vec_pretty(value)?)
}

fn registered(data: &Path) -> Vec<PathBuf> {
    read_json(&data.join(REPOS))
}

/// Adds `repo` to the clones the tracker fetches from.
pub fn register(data: &Path, repo: &Path) -> io::Result<()> {
    let repo = std::path::absolute(repo)?;
    let mut repos = registered(data);
    if !repos.contains(&repo) {
        repos.push(repo);
    }
    write_json(&data.join(REPOS), &repos)
}

/// Stops fetching from `repo`.
pub fn unregister(data: &Path, repo: &Path) -> io::Result<()> {
    let repo = std::path::absolute(repo)?;
    let mut repos = registered(data);
    repos.retain(|r| *r != repo);
    write_json(&data.join(REPOS), &repos)
}

/// Runs git in `repo` without ever waiting for a password or a terminal: a fetch that needs
/// one fails, and the next round tries again.
fn git(repo: &Path, args: &[&str]) -> io::Result<Vec<u8>> {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        // Give up on a fetch that stalls below 1 kB/s for 30 s.
        .env("GIT_HTTP_LOW_SPEED_LIMIT", "1000")
        .env("GIT_HTTP_LOW_SPEED_TIME", "30")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()?;
    if out.status.success() { Ok(out.stdout) } else { Err(io::Error::other(format!("git {} failed", args[0]))) }
}

/// The complete lines after byte `done` of a session's events.jsonl, and where they end.
/// A file shorter than `done` was replaced, so it is read from the start again.
fn new_events(content: &[u8], done: u64) -> (Vec<Value>, u64) {
    let done = if (content.len() as u64) < done { 0 } else { done as usize };
    let end = content[done..].iter().rposition(|&b| b == b'\n').map_or(done, |i| done + i + 1);
    let events = content[done..end].split(|&b| b == b'\n').filter_map(|line| serde_json::from_slice(line).ok()).collect();
    (events, end as u64)
}

pub struct RepoSync {
    data: PathBuf,
    /// Bytes of each `<clone>|<branch>`'s events.jsonl already handled.
    done: HashMap<String, u64>,
}

impl RepoSync {
    pub fn new(data: &Path) -> RepoSync {
        RepoSync { data: data.to_path_buf(), done: read_json(&data.join(DONE)) }
    }

    /// Fetches every registered clone and returns the events added since the last call, in order within each session.
    pub fn poll(&mut self) -> Vec<Value> {
        let mut out = Vec::new();
        let before = self.done.clone();
        for repo in registered(&self.data) {
            // Into a namespace of their own, so the clone's branches and remote-tracking refs stay as they are.
            if git(&repo, &["fetch", "--quiet", "--prune", "origin", "+refs/heads/agent-tracker/*:refs/agent-tracker/*"]).is_err() {
                continue;
            }
            let Ok(refs) = git(&repo, &["for-each-ref", "--format=%(refname)", "refs/agent-tracker/"]) else { continue };
            for branch in String::from_utf8_lossy(&refs).lines() {
                let Ok(content) = git(&repo, &["cat-file", "blob", &format!("{branch}:events.jsonl")]) else { continue };
                let key = format!("{}|{branch}", repo.display());
                let (events, end) = new_events(&content, self.done.get(&key).copied().unwrap_or(0));
                out.extend(events);
                self.done.insert(key, end);
            }
        }
        if self.done != before
            && let Err(e) = write_json(&self.data.join(DONE), &self.done)
        {
            eprintln!("cannot save web session progress: {e}");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git").args(args).current_dir(dir).stdin(Stdio::null()).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }

    /// A data folder, a repository on "GitHub" (a bare repo), the tester's clone of it, and a
    /// cloud session's clone that pushes events the way the hook does.
    struct World {
        data: PathBuf,
        clone: PathBuf,
        cloud: PathBuf,
    }

    impl World {
        fn new(name: &str) -> World {
            let base = std::env::temp_dir().join(format!("agent-tracker-sync-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&base);
            let (data, origin, clone, cloud) = (base.join("data"), base.join("origin.git"), base.join("clone"), base.join("cloud"));
            fs::create_dir_all(&base).unwrap();
            git(&base, &["init", "-q", "--bare", origin.to_str().unwrap()]);
            for c in [&clone, &cloud] {
                git(&base, &["clone", "-q", origin.to_str().unwrap(), c.to_str().unwrap()]);
            }
            World { data, clone, cloud }
        }

        /// Pushes `lines` as the whole of session `s`'s events.jsonl.
        fn push(&self, s: &str, lines: &str) {
            fs::write(self.cloud.join("events.jsonl"), lines).unwrap();
            git(&self.cloud, &["checkout", "-q", "-B", &format!("agent-tracker/{s}")]);
            git(&self.cloud, &["add", "events.jsonl"]);
            git(&self.cloud, &["-c", "user.name=t", "-c", "user.email=t@example.com", "commit", "-q", "--allow-empty", "-m", "event"]);
            git(&self.cloud, &["push", "-q", "-f", "origin", &format!("HEAD:refs/heads/agent-tracker/{s}")]);
        }
    }

    fn names(events: &[Value]) -> Vec<String> {
        events.iter().map(|e| format!("{}:{}", e["session_id"].as_str().unwrap(), e["hook_event_name"].as_str().unwrap())).collect()
    }

    const E1: &str = "{\"session_id\":\"s1\",\"hook_event_name\":\"UserPromptSubmit\"}\n";
    const E2: &str = "{\"session_id\":\"s1\",\"hook_event_name\":\"PreToolUse\"}\n";
    const E3: &str = "{\"session_id\":\"s1\",\"hook_event_name\":\"Stop\"}\n";
    const F1: &str = "{\"session_id\":\"s2\",\"hook_event_name\":\"UserPromptSubmit\"}\n";

    #[test]
    fn picks_up_each_sessions_new_events_once() {
        let w = World::new("new");
        register(&w.data, &w.clone).unwrap();
        let mut sync = RepoSync::new(&w.data);

        w.push("s1", &format!("{E1}{E2}{{\"session_id\":\"s1\",\"hook_ev"));
        assert_eq!(names(&sync.poll()), ["s1:UserPromptSubmit", "s1:PreToolUse"], "an unfinished line waits");
        assert!(sync.poll().is_empty());

        w.push("s1", &format!("{E1}{E2}{E3}"));
        w.push("s2", F1);
        let mut got = names(&sync.poll());
        got.sort();
        assert_eq!(got, ["s1:Stop", "s2:UserPromptSubmit"]);

        assert!(RepoSync::new(&w.data).poll().is_empty(), "a restarted tracker doesn't repeat them");
    }

    #[test]
    fn only_registered_clones_are_fetched() {
        let w = World::new("registered");
        w.push("s1", E1);
        assert!(RepoSync::new(&w.data).poll().is_empty());

        register(&w.data, &w.clone).unwrap();
        register(&w.data, &w.clone).unwrap();
        register(&w.data, &w.data.join("not-a-repo")).unwrap();
        assert_eq!(names(&RepoSync::new(&w.data).poll()), ["s1:UserPromptSubmit"], "a clone counts once; a broken one is skipped");

        unregister(&w.data, &w.clone).unwrap();
        w.push("s1", &format!("{E1}{E2}"));
        assert!(RepoSync::new(&w.data).poll().is_empty());
    }
}
