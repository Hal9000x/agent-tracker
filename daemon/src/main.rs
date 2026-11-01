mod api;
mod claude_code;
mod model;
mod otlp;
mod store;

use std::path::PathBuf;
use std::sync::Arc;

use tokio::net::TcpListener;

const USAGE: &str = "usage: agent-tracker [--host 127.0.0.1] [--port 4318] [--data ./data]";

struct Args {
    host: String,
    port: u16,
    data: PathBuf,
}

fn parse_args(mut it: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut args = Args { host: "127.0.0.1".into(), port: 4318, data: PathBuf::from("data") };
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or(format!("{flag} needs a value"));
        match flag.as_str() {
            "--host" => args.host = value()?,
            "--port" => args.port = value()?.parse().map_err(|e| format!("--port: {e}"))?,
            "--data" => args.data = value()?.into(),
            "-h" | "--help" => return Err(String::new()),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(args)
}

#[tokio::main]
async fn main() {
    let args = parse_args(std::env::args().skip(1)).unwrap_or_else(|e| {
        eprintln!("{e}\n{USAGE}");
        std::process::exit(2)
    });
    let store = store::Store::open(&args.data).unwrap_or_else(|e| {
        eprintln!("cannot open data dir {}: {e}", args.data.display());
        std::process::exit(1)
    });
    let app = Arc::new(api::App { store, claude_code: claude_code::ClaudeCode::default() });
    let listener = TcpListener::bind((args.host.as_str(), args.port)).await.unwrap_or_else(|e| {
        eprintln!("cannot listen on {}:{}: {e}", args.host, args.port);
        std::process::exit(1)
    });
    println!("agent-tracker listening on http://{}", listener.local_addr().unwrap());
    println!("  data:  {}", args.data.display());
    println!("  OTLP:  POST /v1/traces   spans: POST /api/spans   Claude Code hooks: POST /api/hooks/claude-code");
    axum::serve(listener, api::router(app)).await.unwrap();
}
