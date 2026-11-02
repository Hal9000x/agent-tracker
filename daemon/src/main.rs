mod api;
mod claude_code;
mod codex;
mod demo;
mod model;
mod otlp;
mod store;

use std::io::{ErrorKind, IsTerminal, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;

const USAGE: &str = "usage: agent-tracker [--host 127.0.0.1] [--port 4318] [--data ./data] [--no-open]";

struct Args {
    host: String,
    port: u16,
    data: PathBuf,
    open: bool,
}

fn parse_args(mut it: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut args = Args { host: "127.0.0.1".into(), port: 4318, data: PathBuf::from("data"), open: true };
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or(format!("{flag} needs a value"));
        match flag.as_str() {
            "--host" => args.host = value()?,
            "--port" => args.port = value()?.parse().map_err(|e| format!("--port: {e}"))?,
            "--data" => args.data = value()?.into(),
            "--no-open" => args.open = false,
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
    if conn.write_all(b"GET /api/traces HTTP/1.0\r\nHost: localhost\r\n\r\n").is_err() {
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
    let app = Arc::new(api::App {
        store,
        claude_code: claude_code::ClaudeCode::default(),
        codex: codex::Codex::default(),
        bind_host: args.host.clone(),
    });

    println!("Agent Tracker is running at {url}");
    println!("  data:  {}", args.data.display());
    println!("  OTLP:  POST /v1/traces   spans: POST /api/spans");
    println!("  hooks: POST /api/hooks/claude-code   POST /api/hooks/codex");
    println!("  Press Ctrl+C to stop.");
    if args.open {
        open_browser(&url);
    }
    axum::serve(listener, api::router(app)).await.unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn detects_a_running_tracker_and_nothing_else() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Arc::new(api::App {
            store: store::Store::in_memory(),
            claude_code: claude_code::ClaudeCode::default(),
            codex: codex::Codex::new(None),
            bind_host: "127.0.0.1".into(),
        });
        tokio::spawn(async move { axum::serve(listener, api::router(app)).await });
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
