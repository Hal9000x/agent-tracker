//! `agent-tracker install` and `uninstall`: add the tracker's hooks to the Claude Code and
//! Codex settings in a home folder, or take them out again, leaving everything else as it was.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::Value;

struct Harness {
    name: &'static str,
    /// The harness's folder under the home folder; it is skipped when that folder is missing.
    dir: &'static str,
    file: &'static str,
    template: &'static str,
    /// Text that only the tracker's own hook handlers contain.
    marker: &'static str,
    next_step: Option<&'static str>,
}

const HARNESSES: [Harness; 2] = [
    Harness {
        name: "Claude Code",
        dir: ".claude",
        file: "settings.json",
        template: include_str!("../../integrations/claude-code/settings.json"),
        marker: "/api/hooks/claude-code",
        next_step: None,
    },
    Harness {
        name: "Codex",
        dir: ".codex",
        file: "hooks.json",
        template: include_str!("../../integrations/codex/hooks.json"),
        marker: "/api/hooks/codex",
        next_step: Some("run codex, type /hooks and approve the new hooks; Codex skips hooks nobody approved"),
    },
];

const WEB_SETTINGS: &str = include_str!("../../integrations/claude-code-web/settings.json");
const WEB_HOOK: &str = include_str!("../../integrations/claude-code-web/agent-tracker-hook.mjs");
const WEB_MARKER: &str = "agent-tracker-hook.mjs";

/// Sets up a repository so its Claude Code on the web sessions report to the tracker at `url`:
/// adds the hooks to its committed `.claude/settings.json` and writes the hook script beside it.
pub fn install_web(repo: &Path, url: &str) -> io::Result<Vec<String>> {
    // The address ends up in a shell command, so only plain web addresses get in; and the
    // hooks carry prompts and the ingest token across the internet, so only over https.
    let plain = url.chars().all(|c| c.is_ascii_alphanumeric() || "-._~:/".contains(c));
    let host = url.strip_prefix("https://").filter(|_| plain).and_then(|rest| rest.split(['/', ':']).next());
    let Some(host) = host.filter(|h| !h.is_empty()) else {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("{url} is not a plain https address, like https://tracker.example.com")));
    };
    let dir = repo.join(".claude");
    fs::create_dir_all(&dir)?;
    let ours: Value = serde_json::from_str(&WEB_SETTINGS.replace("https://tracker.example.com", url)).expect("bundled hooks are JSON");
    let settings = dir.join("settings.json");
    edit(&settings, false, |s| {
        remove_ours(s, WEB_MARKER);
        add(s, &ours)
    })?;
    fs::write(dir.join(WEB_MARKER), WEB_HOOK)?;
    Ok(vec![
        format!("Claude Code on the web: hooks added to {}, script written to {}", settings.display(), dir.join(WEB_MARKER).display()),
        "Next: commit both files and push.".into(),
        format!("Then in your claude.ai/code cloud environment: allow the domain {host}, and add a network secret for {host} (Bearer, header Authorization) holding the tracker's ingest token."),
    ])
}

/// Takes the cloud-session hooks and script out of a repository.
pub fn uninstall_web(repo: &Path) -> io::Result<Vec<String>> {
    let dir = repo.join(".claude");
    let settings = dir.join("settings.json");
    let removed = settings.exists()
        && edit(&settings, false, |s| {
            remove_ours(s, WEB_MARKER);
            Ok(())
        })?;
    let script = fs::remove_file(dir.join(WEB_MARKER)).is_ok();
    Ok(vec![if removed || script { format!("Claude Code on the web: hooks removed from {}", dir.display()) } else { "Claude Code on the web: no tracker hooks found".into() }])
}

/// Adds the tracker's hooks for every harness found in `home`, replacing any earlier copy. Returns one line per harness.
pub fn install(home: &Path, port: u16) -> io::Result<Vec<String>> {
    let mut report = Vec::new();
    for h in &HARNESSES {
        if !home.join(h.dir).is_dir() {
            report.push(format!("{}: not found (no {} folder), skipped", h.name, h.dir));
            continue;
        }
        let path = home.join(h.dir).join(h.file);
        let ours: Value = serde_json::from_str(&h.template.replace(":4318/", &format!(":{port}/"))).expect("bundled hooks are JSON");
        edit(&path, true, |settings| {
            remove_ours(settings, h.marker);
            add(settings, &ours)
        })?;
        let next = h.next_step.map(|s| format!("; next, {s}")).unwrap_or_default();
        report.push(format!("{}: hooks added to {}{next}", h.name, path.display()));
    }
    Ok(report)
}

/// Takes the tracker's hooks out of every harness's settings in `home`.
pub fn uninstall(home: &Path) -> io::Result<Vec<String>> {
    let mut report = Vec::new();
    for h in &HARNESSES {
        let path = home.join(h.dir).join(h.file);
        let removed = path.exists()
            && edit(&path, true, |settings| {
                remove_ours(settings, h.marker);
                Ok(())
            })?;
        report.push(if removed { format!("{}: hooks removed from {}", h.name, path.display()) } else { format!("{}: no tracker hooks found", h.name) });
    }
    Ok(report)
}

/// Loads a settings file (a missing one is empty), applies `change`, and saves it if anything changed,
/// with `backup` keeping the previous version as `<file>.bak-agent-tracker`. Returns whether it changed.
fn edit(path: &Path, backup: bool, change: impl FnOnce(&mut Value) -> io::Result<()>) -> io::Result<bool> {
    let invalid = |e: String| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {e}; left it unchanged", path.display()));
    let before = match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(text.trim_start_matches('\u{feff}')).map_err(|e| invalid(e.to_string()))?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Value::Object(Default::default()),
        Err(e) => return Err(e),
    };
    let mut after = before.clone();
    change(&mut after).map_err(|e| invalid(e.to_string()))?;
    if after == before {
        return Ok(false);
    }
    if backup && path.exists() {
        fs::copy(path, backup_path(path))?;
    }
    fs::write(path, serde_json::to_string_pretty(&after)? + "\n")?;
    Ok(true)
}

fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".bak-agent-tracker");
    path.with_file_name(name)
}

fn is_ours(handler: &Value, marker: &str) -> bool {
    handler.as_object().is_some_and(|o| o.values().any(|v| v.as_str().is_some_and(|s| s.contains(marker))))
}

/// Drops the tracker's handlers, then any group and event they leave empty.
fn remove_ours(settings: &mut Value, marker: &str) {
    let Some(events) = settings.get_mut("hooks").and_then(Value::as_object_mut) else { return };
    for groups in events.values_mut().filter_map(Value::as_array_mut) {
        for handlers in groups.iter_mut().filter_map(|g| g.get_mut("hooks").and_then(Value::as_array_mut)) {
            handlers.retain(|h| !is_ours(h, marker));
        }
        groups.retain(|g| g["hooks"].as_array().is_none_or(|h| !h.is_empty()));
    }
    events.retain(|_, groups| groups.as_array().is_none_or(|g| !g.is_empty()));
    if events.is_empty() {
        settings.as_object_mut().map(|o| o.remove("hooks"));
    }
}

/// Appends the template's hook groups to each event's list.
fn add(settings: &mut Value, template: &Value) -> io::Result<()> {
    let shape = |what: &str| io::Error::other(format!("{what} is not a JSON object"));
    let hooks = settings.as_object_mut().ok_or_else(|| shape("the file"))?.entry("hooks").or_insert_with(|| Value::Object(Default::default()));
    let hooks = hooks.as_object_mut().ok_or_else(|| shape("\"hooks\""))?;
    for (event, groups) in template["hooks"].as_object().into_iter().flatten() {
        let list = hooks.entry(event.clone()).or_insert_with(|| Value::Array(Vec::new()));
        list.as_array_mut().ok_or_else(|| io::Error::other(format!("\"hooks.{event}\" is not a list")))?.extend(groups.as_array().into_iter().flatten().cloned());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn scratch_home(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("agent-tracker-setup-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn read(path: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    fn handlers(settings: &Value, event: &str) -> Vec<Value> {
        settings["hooks"][event].as_array().into_iter().flatten().flat_map(|g| g["hooks"].as_array().unwrap().clone()).collect()
    }

    const USERS: &str = r#"{
  "model": "opus",
  "permissions": {"allow": ["Bash(npm test)"]},
  "hooks": {
    "Stop": [{"hooks": [{"type": "command", "command": "notify-send done"}]}]
  }
}"#;

    #[test]
    fn install_adds_the_hooks_and_keeps_the_users_own_settings() {
        let home = scratch_home("install");
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::write(home.join(".claude/settings.json"), USERS).unwrap();

        let report = install(&home, 4318).unwrap();

        let settings = read(&home.join(".claude/settings.json"));
        assert_eq!((settings["model"].clone(), settings["permissions"].clone()), (json!("opus"), json!({"allow": ["Bash(npm test)"]})));
        let stop = handlers(&settings, "Stop");
        assert_eq!(stop.len(), 2, "the user's Stop hook stays beside the tracker's: {stop:?}");
        assert_eq!(stop[0]["command"], "notify-send done");
        assert_eq!(stop[1]["url"], "http://127.0.0.1:4318/api/hooks/claude-code");
        assert_eq!(handlers(&settings, "PreToolUse").len(), 1);
        assert_eq!(fs::read_to_string(home.join(".claude/settings.json.bak-agent-tracker")).unwrap(), USERS, "the old file is backed up");
        assert!(!home.join(".codex").exists(), "a harness that isn't there is left alone");
        assert!(report.iter().any(|l| l.contains("Claude Code")) && report.iter().any(|l| l.contains("Codex")), "{report:?}");
    }

    #[test]
    fn installing_again_replaces_the_hooks_instead_of_adding_more() {
        let home = scratch_home("again");
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::write(home.join(".claude/settings.json"), USERS).unwrap();
        install(&home, 4318).unwrap();
        install(&home, 9000).unwrap();

        let stop = handlers(&read(&home.join(".claude/settings.json")), "Stop");
        assert_eq!(stop.len(), 2, "{stop:?}");
        assert_eq!(stop[1]["url"], "http://127.0.0.1:9000/api/hooks/claude-code");
    }

    #[test]
    fn uninstall_leaves_only_the_users_own_settings() {
        let home = scratch_home("uninstall");
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::create_dir_all(home.join(".codex")).unwrap();
        fs::write(home.join(".claude/settings.json"), USERS).unwrap();
        install(&home, 4318).unwrap();
        assert_eq!(handlers(&read(&home.join(".codex/hooks.json")), "Stop").len(), 1, "Codex hooks.json is created");

        uninstall(&home).unwrap();

        assert_eq!(read(&home.join(".claude/settings.json")), serde_json::from_str::<Value>(USERS).unwrap());
        assert_eq!(read(&home.join(".codex/hooks.json")), json!({}), "events left empty are dropped");
    }

    #[test]
    fn install_web_sets_up_a_repository_for_cloud_sessions() {
        let repo = scratch_home("web");
        fs::create_dir_all(repo.join(".claude")).unwrap();
        fs::write(repo.join(".claude/settings.json"), USERS).unwrap();

        install_web(&repo, "https://tracker.example.org").unwrap();

        let settings = read(&repo.join(".claude/settings.json"));
        assert_eq!(settings["model"], "opus");
        let stop = handlers(&settings, "Stop");
        assert_eq!(stop.len(), 2, "{stop:?}");
        assert_eq!(stop[1]["command"], r#"node "$CLAUDE_PROJECT_DIR/.claude/agent-tracker-hook.mjs" https://tracker.example.org"#);
        assert_eq!(fs::read_to_string(repo.join(".claude/agent-tracker-hook.mjs")).unwrap(), WEB_HOOK);
        assert!(!repo.join(".claude/settings.json.bak-agent-tracker").exists(), "no backup to commit by mistake; git has the old file");

        uninstall_web(&repo).unwrap();
        assert_eq!(read(&repo.join(".claude/settings.json")), serde_json::from_str::<Value>(USERS).unwrap());
        assert!(!repo.join(".claude/agent-tracker-hook.mjs").exists());
    }

    #[test]
    fn install_web_takes_only_plain_web_addresses() {
        let repo = scratch_home("web-url");
        // Plain http would carry prompts and the ingest token across the internet readable.
        for url in ["tracker.example.org", "https://x.org/$(id)", "https://x.org\" && rm -rf ~ \"", "https://x.org;id", "ftp://x.org", "http://x.org", "https://"] {
            assert!(install_web(&repo, url).is_err(), "{url}");
        }
        assert!(!repo.join(".claude").exists());
    }

    #[test]
    fn unreadable_settings_are_reported_and_never_overwritten() {
        let home = scratch_home("broken");
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::write(home.join(".claude/settings.json"), "{ not json").unwrap();

        assert!(install(&home, 4318).is_err());
        assert_eq!(fs::read_to_string(home.join(".claude/settings.json")).unwrap(), "{ not json");
    }

    #[test]
    fn settings_saved_with_a_byte_order_mark_still_load() {
        let home = scratch_home("bom");
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::write(home.join(".claude/settings.json"), format!("\u{feff}{USERS}")).unwrap();

        install(&home, 4318).unwrap();

        assert_eq!(read(&home.join(".claude/settings.json"))["model"], "opus");
    }
}
