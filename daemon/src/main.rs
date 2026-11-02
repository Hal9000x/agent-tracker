mod api;
mod auth;
mod claude_code;
mod codex;
mod demo;
mod model;
mod otlp;
mod repo_sync;
mod setup;
mod store;
mod transcript;

use std::io::{ErrorKind, IsTerminal, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;

const USAGE: &str = "usage: agent-tracker [--host 127.0.0.1] [--port 4318] [--data <folder>] [--no-open] [--public-url https://...]
       --public-url, or a --host other than this machine, makes the tracker require sign-in:
       set AGENT_TRACKER_PASSWORD (12+ characters) and optionally AGENT_TRACKER_USERNAME (default admin)
       agent-tracker install [--port 4318]   add the tracker's hooks to Claude Code and Codex
       agent-tracker uninstall               take them out again
       agent-tracker install-web             in a clone: track its Claude Code on the web sessions here
       agent-tracker uninstall-web           in a clone: stop that
       agent-tracker --version";

#[derive(Debug, PartialEq)]
enum Mode {
    Serve,
    Install,
    Uninstall,
    InstallWeb,
    UninstallWeb,
    Version,
}

struct Args {
    mode: Mode,
    host: String,
    port: u16,
    data: PathBuf,
    open: bool,
    /// Where the tracker is reachable from the internet, if it is.
    public_url: Option<String>,
}

/// A tracker that other machines can reach requires sign-in.
fn needs_sign_in(args: &Args) -> bool {
    let loopback = args.host == "localhost" || args.host.trim_matches(['[', ']']).parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback());
    args.public_url.is_some() || !loopback
}

/// A tracker other machines can reach must be told the address they use, so that it knows
/// its host name and whether passwords and cookies travel over https.
fn hosting_problem(args: &Args) -> Option<String> {
    (needs_sign_in(args) && args.public_url.is_none()).then(|| {
        format!("--host {} lets other machines reach the tracker: also give --public-url, the address they use (https://..., or http://... on a network you trust)", args.host)
    })
}

/// The owner's credentials from the environment, and the ingest token from the data folder.
fn sign_in_from_env(args: &Args) -> auth::Auth {
    let password = std::env::var("AGENT_TRACKER_PASSWORD").unwrap_or_default();
    if password.chars().count() < 12 {
        fail("A tracker reachable from other machines requires sign-in. Set AGENT_TRACKER_PASSWORD (12 characters or more) and optionally AGENT_TRACKER_USERNAME (default admin), then start it again.".into());
    }
    let username = std::env::var("AGENT_TRACKER_USERNAME").ok().filter(|u| !u.is_empty()).unwrap_or_else(|| "admin".into());
    let token = auth::load_ingest_token(&args.data).unwrap_or_else(|e| fail(format!("cannot keep the ingest token in {}: {e}", args.data.display())));
    auth::Auth::new(username, password, token, args.public_url.as_deref())
}

/// The per-user data folder, so traces stay in one place however the tracker was started.
fn default_data_dir() -> PathBuf {
    let var = |name| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    let home = std::env::home_dir();
    let base = if cfg!(windows) {
        var("LOCALAPPDATA")
    } else if cfg!(target_os = "macos") {
        home.map(|h| h.join("Library").join("Application Support"))
    } else {
        var("XDG_DATA_HOME").or_else(|| home.map(|h| h.join(".local").join("share")))
    };
    base.map_or_else(|| PathBuf::from("data"), |b| b.join("agent-tracker"))
}

fn parse_args(mut it: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut args = Args { mode: Mode::Serve, host: "127.0.0.1".into(), port: 4318, data: default_data_dir(), open: true, public_url: None };
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or(format!("{flag} needs a value"));
        match flag.as_str() {
            "install" => args.mode = Mode::Install,
            "uninstall" => args.mode = Mode::Uninstall,
            "install-web" => args.mode = Mode::InstallWeb,
            "uninstall-web" => args.mode = Mode::UninstallWeb,
            "-V" | "--version" => args.mode = Mode::Version,
            "--host" => args.host = value()?,
            "--port" => args.port = value()?.parse().map_err(|e| format!("--port: {e}"))?,
            "--data" => args.data = value()?.into(),
            "--no-open" => args.open = false,
            "--public-url" => {
                let url = value()?;
                if !url.starts_with("https://") && !url.starts_with("http://") {
                    return Err(format!("--public-url {url}: give the full address, like https://tracker.example.com"));
                }
                args.public_url = Some(url.trim_end_matches('/').to_string());
            }
            "-h" | "--help" => return Err(String::new()),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(args)
}

/// Prints the error and, in an interactive console, waits for Enter so a
/// double-clicked window stays open long enough to read it.
fn fail(msg: String) -> ! {
    eprintln!("{msg}");
    if std::io::stdin().is_terminal() {
        eprintln!("Press Enter to close.");
        let _ = std::io::stdin().read_line(&mut String::new());
    }
    std::process::exit(1)
}

/// Whether an Agent Tracker already answers on this address.
fn tracker_running(host: &str, port: u16) -> bool {
    let Ok(mut conn) = TcpStream::connect((host, port)) else { return false };
    let _ = conn.set_read_timeout(Some(Duration::from_secs(1)));
    // /api/session answers without sign-in, so this also finds a hosted tracker.
    if conn.write_all(b"GET /api/session HTTP/1.0\r\nHost: localhost\r\n\r\n").is_err() {
        return false;
    }
    let mut head = [0u8; 512];
    let n = conn.read(&mut head).unwrap_or(0);
    let head = String::from_utf8_lossy(&head[..n]);
    // The status line echoes the request's HTTP version, so check only the code.
    head.split_whitespace().nth(1) == Some("200") && head.contains("application/json")
}

fn open_browser(url: &str) {
    let opened = if cfg!(windows) {
        Command::new("cmd").args(["/C", "start", "", url]).spawn()
    } else if cfg!(target_os = "macos") {
        Command::new("open").arg(url).spawn()
    } else {
        Command::new("xdg-open").arg(url).spawn()
    };
    if let Err(e) = opened {
        eprintln!("could not open a browser ({e}); open {url} yourself");
    }
}

#[tokio::main]
async fn main() {
    let args = parse_args(std::env::args().skip(1)).unwrap_or_else(|e| {
        eprintln!("{e}\n{USAGE}");
        std::process::exit(2)
    });
    let home = || std::env::home_dir().unwrap_or_else(|| fail("cannot find your home folder".into()));
    let repo = || std::env::current_dir().unwrap_or_else(|e| fail(format!("cannot read the current folder: {e}")));
    let done = match &args.mode {
        Mode::Serve => None,
        Mode::Version => return println!("agent-tracker {}", env!("CARGO_PKG_VERSION")),
        Mode::Install => Some(setup::install(&home(), args.port)),
        Mode::Uninstall => Some(setup::uninstall(&home())),
        Mode::InstallWeb => Some(setup::install_web(&repo(), &args.data)),
        Mode::UninstallWeb => Some(setup::uninstall_web(&repo(), &args.data)),
    };
    // These run from a terminal, so a failure needs no "Press Enter" pause.
    match done {
        Some(Ok(lines)) => return lines.iter().for_each(|line| println!("{line}")),
        Some(Err(e)) => {
            eprintln!("{e}");
            std::process::exit(1)
        }
        None => {}
    }
    if let Some(problem) = hosting_problem(&args) {
        fail(problem);
    }
    let browse_host = if args.host == "0.0.0.0" { "127.0.0.1" } else { args.host.as_str() };
    let url = format!("http://{browse_host}:{}", args.port);

    let listener = match TcpListener::bind((args.host.as_str(), args.port)).await {
        Ok(l) => l,
        Err(e) if e.kind() == ErrorKind::AddrInUse && tracker_running(browse_host, args.port) => {
            println!("Agent Tracker is already running at {url}");
            if args.open {
                open_browser(&url);
            }
            return;
        }
        Err(e) => fail(format!("cannot listen on {}:{}: {e}", args.host, args.port)),
    };
    let store = store::Store::open(&args.data).unwrap_or_else(|e| fail(format!("cannot open data dir {}: {e}", args.data.display())));
    let auth = needs_sign_in(&args).then(|| sign_in_from_env(&args));
    let signed_in_as = auth.as_ref().map(|a| a.username().to_string());
    // A hosted tracker hears from other machines only, so it never opens transcript or
    // rollout files on its own; web sessions send their transcript text along.
    let hosted = auth.is_some();
    let app = Arc::new(api::App {
        store,
        claude_code: claude_code::ClaudeCode::new(claude_code::projects_dir().filter(|_| !hosted)),
        codex: if hosted { codex::Codex::new(None) } else { codex::Codex::default() },
        bind_host: args.host.clone(),
        auth,
    });

    println!("Agent Tracker {} is running at {url}", env!("CARGO_PKG_VERSION"));
    if let Some(public) = &args.public_url {
        println!("  public: {public}");
        if public.starts_with("http://") {
            println!("  warning: without https, passwords and tokens cross the network readable");
        }
    }
    if let Some(user) = signed_in_as {
        println!("  sign-in required (user {user}); the ingest token for hooks is in the Connect panel");
    }
    println!("  data:  {}", args.data.display());
    println!("  OTLP:  POST /v1/traces   spans: POST /api/spans");
    println!("  hooks: POST /api/hooks/claude-code   POST /api/hooks/codex");
    println!("  Press Ctrl+C to stop.");
    if args.open {
        open_browser(&url);
    }
    tokio::spawn(sync_web_sessions(app.clone(), args.data.clone()));
    // Sign-in counts failures per client address.
    axum::serve(listener, api::router(app).into_make_service_with_connect_info::<std::net::SocketAddr>()).await.unwrap();
}

/// How often the clones `install-web` registered are fetched for new web-session events.
const WEB_SYNC_INTERVAL: Duration = Duration::from_secs(15);

/// Feeds the events Claude Code on the web sessions push to their repositories into the tracker.
async fn sync_web_sessions(app: Arc<api::App>, data: PathBuf) {
    let mut sync = repo_sync::RepoSync::new(&data);
    loop {
        let (back, events) = tokio::task::spawn_blocking(move || {
            let events = sync.poll();
            (sync, events)
        })
        .await
        .expect("web session sync");
        sync = back;
        for ev in events {
            if let Err(e) = api::web_session_event(&app, &ev) {
                eprintln!("web session sync: {e}");
            }
        }
        tokio::time::sleep(WEB_SYNC_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    /// A tracker on a free port.
    async fn serve() -> (u16, Arc<api::App>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Arc::new(api::App {
            store: store::Store::in_memory(),
            claude_code: claude_code::ClaudeCode::default(),
            codex: codex::Codex::new(None),
            bind_host: "127.0.0.1".into(),
            auth: None,
        });
        let router = api::router(app.clone());
        tokio::spawn(async move { axum::serve(listener, router).await });
        (port, app)
    }

    /// The shells Codex may run a hook command in: PowerShell on Windows (where a bare `@-` doesn't parse), else sh.
    fn shells(command: &str) -> Vec<Command> {
        if cfg!(windows) {
            let mut powershell = Command::new("powershell.exe");
            powershell.args(["-NoProfile", "-Command", command]);
            let mut cmd = Command::new("cmd.exe");
            #[cfg(windows)]
            std::os::windows::process::CommandExt::raw_arg(&mut cmd, format!("/C {command}"));
            vec![powershell, cmd]
        } else {
            let mut sh = Command::new("sh");
            sh.args(["-c", command]);
            vec![sh]
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_shipped_codex_hooks_deliver_events_from_every_shell() {
        let (port, app) = serve().await;
        let hooks: serde_json::Value = serde_json::from_str(include_str!("../../integrations/codex/hooks.json")).unwrap();
        let key = if cfg!(windows) { "commandWindows" } else { "command" };
        let mut commands: Vec<String> = hooks["hooks"].as_object().unwrap().values().flat_map(|groups| groups.as_array().unwrap())
            .flat_map(|group| group["hooks"].as_array().unwrap())
            .map(|handler| handler[key].as_str().unwrap().replace(":4318/", &format!(":{port}/")))
            .collect();
        commands.dedup();
        assert_eq!(commands.len(), 1, "every event uses the same command");

        for (i, mut shell) in shells(&commands[0]).into_iter().enumerate() {
            let turn = format!("turn-{i}");
            let event = serde_json::json!({"session_id": "s", "turn_id": turn, "hook_event_name": "UserPromptSubmit", "prompt": "hi"});
            let out = tokio::task::spawn_blocking(move || {
                let mut child = shell.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
                child.stdin.take().unwrap().write_all(event.to_string().as_bytes()).unwrap();
                child.wait_with_output().unwrap()
            })
            .await
            .unwrap();
            let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            assert!(out.status.success(), "shell {i} failed: {stderr}");
            assert_eq!(stdout.trim(), "{}", "Codex parses the hook's stdout as its answer");
            assert!(app.store.has_trace(&turn), "shell {i} delivered no event");
        }
    }

    #[test]
    fn reads_commands_and_keeps_data_in_the_per_user_folder() {
        let parse = |line: &str| parse_args(line.split_whitespace().map(String::from));
        let serve = parse("").unwrap();
        assert_eq!(serve.mode, Mode::Serve);
        assert!(serve.data.is_absolute() && serve.data.ends_with("agent-tracker"), "{}", serve.data.display());
        assert_eq!(parse("--data here").unwrap().data, PathBuf::from("here"));
        let install = parse("install --port 9000").unwrap();
        assert_eq!((install.mode, install.port), (Mode::Install, 9000));
        assert_eq!(parse("uninstall").unwrap().mode, Mode::Uninstall);
        assert_eq!(parse("install-web --data here").unwrap().mode, Mode::InstallWeb);

        assert!(!needs_sign_in(&parse("").unwrap()));
        assert!(!needs_sign_in(&parse("--host ::1").unwrap()));
        assert!(needs_sign_in(&parse("--host 0.0.0.0").unwrap()));
        // Reachable from elsewhere, it must be told where, and so whether it is behind https.
        assert!(hosting_problem(&parse("--host 0.0.0.0").unwrap()).is_some());
        assert!(hosting_problem(&parse("--host 0.0.0.0 --public-url http://10.0.0.5:4318").unwrap()).is_none());
        assert!(hosting_problem(&parse("").unwrap()).is_none());
        assert!(needs_sign_in(&parse("--public-url https://t.example.org/").unwrap()), "behind a proxy on this machine");
        assert_eq!(parse("--public-url https://t.example.org/").unwrap().public_url.as_deref(), Some("https://t.example.org"));
        assert!(parse("--public-url t.example.org").is_err());
        assert!(parse("instal").is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn detects_a_running_tracker_and_nothing_else() {
        let (port, _app) = serve().await;
        assert!(tokio::task::spawn_blocking(move || tracker_running("127.0.0.1", port)).await.unwrap());

        let other = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let other_port = other.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for mut conn in other.incoming().flatten() {
                let _ = conn.write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n");
            }
        });
        assert!(!tokio::task::spawn_blocking(move || tracker_running("127.0.0.1", other_port)).await.unwrap());
    }
}
