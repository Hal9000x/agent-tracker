use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceResponse;
use prost::Message;
use rust_embed::Embed;
use serde_json::Value;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{Stream, StreamExt};

use crate::auth::{Auth, SignIn};
use crate::claude_code::ClaudeCode;
use crate::codex::Codex;
use crate::demo;
use crate::model::SpanUpdate;
use crate::otlp;
use crate::store::Store;

pub struct App {
    pub store: Store,
    pub claude_code: ClaudeCode,
    pub codex: Codex,
    /// The --host address; requests naming any other host are refused.
    pub bind_host: String,
    /// Set when the tracker is reachable from the internet; None serves anyone who can reach it.
    pub auth: Option<Auth>,
}

#[derive(Embed)]
#[folder = "../web/dist"]
struct Assets;

pub fn router(app: Arc<App>) -> Router {
    let ingest = Router::new()
        .route("/api/spans", post(ingest_spans))
        .route("/api/hooks/claude-code", post(claude_code_hook))
        .route("/api/hooks/codex", post(codex_hook))
        .route("/v1/traces", post(otlp_traces))
        .route_layer(middleware::from_fn_with_state(app.clone(), require_token));
    let owner = Router::new()
        .route("/api/traces", get(list_traces))
        .route("/api/traces/{id}", get(get_trace).delete(delete_trace))
        .route("/api/stream", get(stream))
        .route("/api/demo", post(play_demo))
        .route("/api/logout", post(logout))
        .route_layer(middleware::from_fn_with_state(app.clone(), require_sign_in));
    Router::new()
        .merge(ingest)
        .merge(owner)
        .route("/api/login", post(login))
        .route("/api/session", get(session))
        .fallback(static_files)
        .layer(DefaultBodyLimit::max(64 << 20))
        .layer(middleware::from_fn_with_state(app.clone(), guard))
        .with_state(app)
}

/// Keeps web pages out: a foreign Host header means DNS rebinding, and writes must
/// use a content type that a browser cannot send cross-site without a CORS preflight
/// (which this server never answers).
async fn guard(State(app): State<Arc<App>>, req: Request, next: Next) -> Response {
    let public_host = app.auth.as_ref().and_then(Auth::public_host);
    match refusal(&app.bind_host, public_host, &req) {
        Some(refused) => refused.into_response(),
        None => next.run(req).await,
    }
}

async fn require_token(State(app): State<Arc<App>>, req: Request, next: Next) -> Response {
    match &app.auth {
        Some(auth) if !auth.has_token(req.headers()) => (StatusCode::UNAUTHORIZED, "send the ingest token as Authorization: Bearer <token>").into_response(),
        _ => next.run(req).await,
    }
}

async fn require_sign_in(State(app): State<Arc<App>>, req: Request, next: Next) -> Response {
    match &app.auth {
        Some(auth) if !auth.signed_in(req.headers()) => (StatusCode::UNAUTHORIZED, "sign in first").into_response(),
        _ => next.run(req).await,
    }
}

#[derive(serde::Deserialize)]
struct Credentials {
    username: String,
    password: String,
}

/// Who is signing in, for counting failures: the peer's address or, when the peer is a proxy
/// on this machine or a private network, the client address that proxy appended to
/// X-Forwarded-For. Entries before the last one come from the client, so they're ignored.
fn client_address(peer: Option<IpAddr>, headers: &HeaderMap) -> String {
    let private = |ip: IpAddr| match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => v6.is_loopback() || v6.segments()[0] & 0xfe00 == 0xfc00,
    };
    let forwarded = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()).and_then(|v| v.rsplit(',').next()).map(str::trim);
    match (peer, forwarded.filter(|f| !f.is_empty())) {
        (Some(ip), Some(client)) if private(ip) => client.to_string(),
        (Some(ip), _) => ip.to_string(),
        (None, client) => client.unwrap_or("unknown").to_string(),
    }
}

/// Sign-in is open to anyone, so it reads only a small body.
const SIGN_IN_BODY_LIMIT: usize = 4096;

async fn login(State(app): State<Arc<App>>, req: Request) -> Response {
    let Some(auth) = &app.auth else { return StatusCode::NO_CONTENT.into_response() };
    let peer = req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip());
    let client = client_address(peer, req.headers());
    let Ok(body) = axum::body::to_bytes(req.into_body(), SIGN_IN_BODY_LIMIT).await else {
        return (StatusCode::PAYLOAD_TOO_LARGE, "sign-in body too large").into_response();
    };
    let Ok(c) = serde_json::from_slice::<Credentials>(&body) else {
        return (StatusCode::BAD_REQUEST, "send {\"username\": ..., \"password\": ...}").into_response();
    };
    match auth.sign_in(&client, &c.username, &c.password) {
        SignIn::Accepted(cookie) => (StatusCode::NO_CONTENT, [(header::SET_COOKIE, cookie)]).into_response(),
        SignIn::Wrong => (StatusCode::UNAUTHORIZED, "wrong username or password").into_response(),
        SignIn::Locked => (StatusCode::TOO_MANY_REQUESTS, "too many failed sign-ins; try again in a minute").into_response(),
    }
}

async fn logout(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    match &app.auth {
        Some(auth) => (StatusCode::NO_CONTENT, [(header::SET_COOKIE, auth.sign_out(&headers))]).into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    }
}

/// Whether sign-in is needed and done; a signed-in owner also gets what setting up hooks takes.
async fn session(State(app): State<Arc<App>>, headers: HeaderMap) -> Json<Value> {
    Json(match &app.auth {
        None => serde_json::json!({"auth": false, "signed_in": true}),
        Some(auth) if auth.signed_in(&headers) => {
            serde_json::json!({"auth": true, "signed_in": true, "ingest_token": auth.ingest_token(), "public_url": auth.public_url()})
        }
        Some(_) => serde_json::json!({"auth": true, "signed_in": false}),
    })
}

fn refusal(bind_host: &str, public_host: Option<&str>, req: &Request) -> Option<(StatusCode, &'static str)> {
    let header = |name| req.headers().get(name).and_then(|v| v.to_str().ok()).unwrap_or_default();
    if !host_allowed(bind_host, public_host, header(header::HOST)) {
        return Some((StatusCode::FORBIDDEN, "unexpected Host header"));
    }
    let essence = header(header::CONTENT_TYPE).split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
    if req.method() == Method::POST && essence != "application/json" && essence != "application/x-protobuf" {
        return Some((StatusCode::UNSUPPORTED_MEDIA_TYPE, "send application/json or application/x-protobuf"));
    }
    None
}

/// Bound to every address, any host name is fine, unless a public URL says which one to expect.
fn host_allowed(bind_host: &str, public_host: Option<&str>, host: &str) -> bool {
    if host.is_empty() || (public_host.is_none() && matches!(bind_host, "0.0.0.0" | "::" | "[::]")) {
        return true;
    }
    let name = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => host.rsplit_once(':').map_or(host, |(name, _)| name),
    };
    matches!(name, "localhost" | "127.0.0.1" | "::1") || name == bind_host.trim_matches(['[', ']']) || public_host.is_some_and(|p| name.eq_ignore_ascii_case(p))
}

type ApiError = (StatusCode, String);

fn now_ms() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64() * 1000.0)
}

/// Handles a Claude Code hook event a web session pushed to its repository (see `repo_sync`),
/// timed from when it happened in the session rather than when it was fetched.
pub fn web_session_event(app: &App, ev: &Value) -> std::io::Result<()> {
    let at = ev["agent_tracker_time"].as_f64().unwrap_or_else(now_ms);
    let mut updates = app.claude_code.handle(ev, at, |t| app.store.has_trace(t));
    for u in &mut updates {
        if u.attrs.contains_key("source") {
            u.attrs.insert("source".into(), "claude-code-web".into());
        }
    }
    app.store.ingest(updates, now_ms())
}

fn ingest(app: &App, updates: Vec<SpanUpdate>) -> Result<(), ApiError> {
    app.store.ingest(updates, now_ms()).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("write failed: {e}")))
}

/// Accepts one span update or an array of them.
async fn ingest_spans(State(app): State<Arc<App>>, body: Bytes) -> Result<StatusCode, ApiError> {
    let bad = |e: serde_json::Error| (StatusCode::BAD_REQUEST, e.to_string());
    let updates: Vec<SpanUpdate> = match serde_json::from_slice::<Value>(&body).map_err(bad)? {
        v @ Value::Array(_) => serde_json::from_value(v).map_err(bad)?,
        v => vec![serde_json::from_value(v).map_err(bad)?],
    };
    if updates.iter().any(|u| u.trace_id.is_empty() || u.span_id.is_empty()) {
        return Err((StatusCode::BAD_REQUEST, "trace_id and span_id must be non-empty".into()));
    }
    ingest(&app, updates)?;
    Ok(StatusCode::ACCEPTED)
}

async fn list_traces(State(app): State<Arc<App>>) -> impl IntoResponse {
    Json(app.store.traces())
}

async fn get_trace(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<impl IntoResponse, StatusCode> {
    app.store.trace(&id).map(Json).ok_or(StatusCode::NOT_FOUND)
}

async fn delete_trace(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<StatusCode, ApiError> {
    match app.store.delete(&id, now_ms()) {
        Ok(true) => Ok(StatusCode::NO_CONTENT),
        Ok(false) => Err((StatusCode::NOT_FOUND, "no such trace".into())),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, format!("write failed: {e}"))),
    }
}

/// `update` events carry merged spans plus their trace summaries. `resync`
/// means this subscriber fell behind and should refetch.
async fn stream(State(app): State<Arc<App>>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let events = BroadcastStream::new(app.store.subscribe()).map(|msg| {
        Ok(match msg {
            Ok(update) => Event::default().event("update").json_data(&*update).expect("update serializes"),
            Err(_) => Event::default().event("resync").data(""),
        })
    });
    Sse::new(events).keep_alive(KeepAlive::default())
}

async fn claude_code_hook(State(app): State<Arc<App>>, Json(ev): Json<Value>) -> Result<Json<Value>, ApiError> {
    ingest(&app, app.claude_code.handle(&ev, now_ms(), |t| app.store.has_trace(t)))?;
    // An empty object tells Claude Code the hook made no decision.
    Ok(Json(Value::Object(Default::default())))
}

async fn codex_hook(State(app): State<Arc<App>>, Json(ev): Json<Value>) -> Result<Json<Value>, ApiError> {
    ingest(&app, app.codex.handle(&ev, now_ms(), |t| app.store.has_trace(t)))?;
    // Codex reads the hook's stdout; an empty object asks for nothing.
    Ok(Json(Value::Object(Default::default())))
}

/// Replays the built-in demo recording in real time; its traces arrive over the stream.
async fn play_demo(State(app): State<Arc<App>>) -> StatusCode {
    let now = now_ms();
    let steps = demo::plan(now, now as u64);
    tokio::spawn(async move {
        for (at, span) in steps {
            let wait = at - now_ms();
            if wait > 0.0 {
                tokio::time::sleep(Duration::from_secs_f64(wait / 1000.0)).await;
            }
            if let Err(e) = app.store.ingest(vec![span], at) {
                eprintln!("demo stopped: {e}");
                return;
            }
        }
    });
    StatusCode::ACCEPTED
}

async fn otlp_traces(State(app): State<Arc<App>>, headers: HeaderMap, body: Bytes) -> Result<Response, ApiError> {
    let header = |name| headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or_default();
    if !header(header::CONTENT_ENCODING).is_empty() && header(header::CONTENT_ENCODING) != "identity" {
        return Err((
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "compressed OTLP is not supported; set OTEL_EXPORTER_OTLP_COMPRESSION=none".into(),
        ));
    }
    let content_type = header(header::CONTENT_TYPE);
    let req = otlp::decode(&body, content_type).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    ingest(&app, otlp::to_updates(req))?;
    Ok(if content_type.starts_with("application/json") {
        ([(header::CONTENT_TYPE, "application/json")], "{}").into_response()
    } else {
        let empty = ExportTraceServiceResponse::default().encode_to_vec();
        ([(header::CONTENT_TYPE, "application/x-protobuf")], empty).into_response()
    })
}

/// Serves the built web UI; unknown paths get index.html so client routes work.
async fn static_files(uri: Uri) -> Response {
    let path = match uri.path().trim_start_matches('/') {
        "" => "index.html",
        p => p,
    };
    let (path, file) = match Assets::get(path) {
        Some(f) => (path, f),
        None => match Assets::get("index.html") {
            Some(f) => ("index.html", f),
            None => return (StatusCode::NOT_FOUND, "UI not built: run `npm run build` in web/").into_response(),
        },
    };
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    ([(header::CONTENT_TYPE, mime.as_ref())], file.data).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use tower::ServiceExt;

    fn app() -> Router {
        router(Arc::new(App { store: Store::in_memory(), claude_code: ClaudeCode::default(), codex: Codex::new(None), bind_host: "127.0.0.1".into(), auth: None }))
    }

    async fn send(app: &Router, method: &str, uri: &str, content_type: &str, body: impl Into<Body>) -> (StatusCode, Value) {
        let req = Request::builder().method(method).uri(uri).header("content-type", content_type).body(body.into()).unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    #[tokio::test]
    async fn spans_round_trip() {
        let app = app();
        let body = r#"[{"trace_id":"t","span_id":"a","kind":"agent","name":"planner","start":1},
                       {"trace_id":"t","span_id":"b","parent_id":"a","kind":"tool","start":2,"end":3}]"#;
        assert_eq!(send(&app, "POST", "/api/spans", "application/json", body).await.0, StatusCode::ACCEPTED);
        let one = r#"{"trace_id":"t","span_id":"a","end":4}"#;
        assert_eq!(send(&app, "POST", "/api/spans", "application/json", one).await.0, StatusCode::ACCEPTED);

        let (status, spans) = send(&app, "GET", "/api/traces/t", "", "").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(spans[0]["span_id"], "a");
        assert_eq!((spans[0]["end"].as_f64(), spans[0]["status"].as_str()), (Some(4.0), Some("ok")));
        let (_, traces) = send(&app, "GET", "/api/traces", "", "").await;
        assert_eq!((traces[0]["name"].as_str(), traces[0]["spans"].as_u64()), (Some("planner"), Some(2)));
    }

    #[tokio::test]
    async fn deletes_a_trace() {
        let app = app();
        send(&app, "POST", "/api/spans", "application/json", r#"{"trace_id":"t","span_id":"a"}"#).await;
        assert_eq!(send(&app, "DELETE", "/api/traces/t", "", "").await.0, StatusCode::NO_CONTENT);
        assert_eq!(send(&app, "GET", "/api/traces/t", "", "").await.0, StatusCode::NOT_FOUND);
        assert_eq!(send(&app, "DELETE", "/api/traces/t", "", "").await.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn rejects_bad_spans() {
        let app = app();
        let (status, _) = send(&app, "POST", "/api/spans", "application/json", r#"{"trace_id":"","span_id":"a"}"#).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = send(&app, "POST", "/api/spans", "application/json", r#"{"span_id":"a"}"#).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(send(&app, "GET", "/api/traces/nope", "", "").await.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn refuses_cross_site_writes_and_foreign_hosts() {
        let app = app();
        let span = r#"{"trace_id":"t","span_id":"a"}"#;
        // Browsers can send text/plain cross-site without a preflight; JSON and protobuf need one.
        assert_eq!(send(&app, "POST", "/api/spans", "text/plain", span).await.0, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(send(&app, "POST", "/v1/traces", "text/plain", "x").await.0, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(send(&app, "POST", "/api/demo", "", "").await.0, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(send(&app, "POST", "/api/spans", "application/json; charset=utf-8", span).await.0, StatusCode::ACCEPTED);

        // A DNS-rebinding page reaches 127.0.0.1 under its own host name.
        let get = |host: &str| Request::builder().uri("/api/traces").header("host", host).body(Body::empty()).unwrap();
        assert_eq!(app.clone().oneshot(get("attacker.example:4318")).await.unwrap().status(), StatusCode::FORBIDDEN);
        for ok in ["localhost:4318", "127.0.0.1:4318", "[::1]:4318", "localhost"] {
            assert_eq!(app.clone().oneshot(get(ok)).await.unwrap().status(), StatusCode::OK, "{ok}");
        }
        let open = router(Arc::new(App { store: Store::in_memory(), claude_code: ClaudeCode::default(), codex: Codex::new(None), bind_host: "0.0.0.0".into(), auth: None }));
        assert_eq!(open.oneshot(get("tracker.lan:4318")).await.unwrap().status(), StatusCode::OK, "--host 0.0.0.0 serves any host");
    }

    #[tokio::test]
    async fn otlp_json_and_hooks_ingest() {
        let app = app();
        let (status, _) = send(&app, "POST", "/v1/traces", "application/json", include_str!("../fixtures/otlp-genai.json")).await;
        assert_eq!(status, StatusCode::OK);
        let (_, spans) = send(&app, "GET", "/api/traces/5b8efff798038103d269b633813fc60c", "", "").await;
        assert_eq!(spans.as_array().unwrap().len(), 3);

        let hook = r#"{"session_id":"s","prompt_id":"p","hook_event_name":"UserPromptSubmit","prompt":"hi"}"#;
        let (status, body) = send(&app, "POST", "/api/hooks/claude-code", "application/json", hook).await;
        assert_eq!((status, body), (StatusCode::OK, serde_json::json!({})));
        let (_, spans) = send(&app, "GET", "/api/traces/p", "", "").await;
        assert_eq!(spans.as_array().unwrap().len(), 2);

        let hook = r#"{"session_id":"s","turn_id":"turn-1","hook_event_name":"UserPromptSubmit","prompt":"hi"}"#;
        let (status, body) = send(&app, "POST", "/api/hooks/codex", "application/json", hook).await;
        assert_eq!((status, body), (StatusCode::OK, serde_json::json!({})));
        let (_, spans) = send(&app, "GET", "/api/traces/turn-1", "", "").await;
        assert_eq!(spans[0]["attrs"]["source"], "codex");
    }

    const PASSWORD: &str = "correct horse battery";

    /// A tracker served at https://tracker.example.org, owned by "owner", with ingest token "tok-123".
    fn hosted() -> Router {
        let auth = Auth::new("owner".into(), PASSWORD.into(), "tok-123".into(), Some("https://tracker.example.org"));
        router(Arc::new(App { store: Store::in_memory(), claude_code: ClaudeCode::default(), codex: Codex::new(None), bind_host: "127.0.0.1".into(), auth: Some(auth) }))
    }

    async fn call(app: &Router, method: &str, uri: &str, headers: &[(&str, &str)], body: impl Into<Body>) -> (StatusCode, HeaderMap, Value) {
        let mut req = Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            req = req.header(*name, *value);
        }
        let res = app.clone().oneshot(req.body(body.into()).unwrap()).await.unwrap();
        let (status, headers) = (res.status(), res.headers().clone());
        let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (status, headers, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    const HOST: (&str, &str) = ("host", "tracker.example.org");
    const JSON: (&str, &str) = ("content-type", "application/json");

    async fn sign_in(app: &Router, password: &str) -> (StatusCode, HeaderMap) {
        let body = serde_json::json!({"username": "owner", "password": password}).to_string();
        let (status, headers, _) = call(app, "POST", "/api/login", &[HOST, JSON], body).await;
        (status, headers)
    }

    #[tokio::test]
    async fn a_hosted_tracker_serves_its_owner_and_token_holders_only() {
        let app = hosted();
        let unauthorized = StatusCode::UNAUTHORIZED;
        assert_eq!(call(&app, "GET", "/api/traces", &[HOST], "").await.0, unauthorized);
        assert_ne!(call(&app, "GET", "/", &[HOST], "").await.0, unauthorized, "the sign-in page loads");
        assert_eq!(call(&app, "GET", "/api/session", &[HOST], "").await.2, serde_json::json!({"auth": true, "signed_in": false}));

        let hook = r#"{"session_id":"s","prompt_id":"p","hook_event_name":"UserPromptSubmit","prompt":"hi"}"#;
        assert_eq!(call(&app, "POST", "/api/hooks/claude-code", &[HOST, JSON], hook).await.0, unauthorized);
        assert_eq!(call(&app, "POST", "/api/hooks/claude-code", &[HOST, JSON, ("authorization", "Bearer tok-12")], hook).await.0, unauthorized);
        assert_eq!(call(&app, "POST", "/api/hooks/claude-code", &[HOST, JSON, ("authorization", "Bearer tok-123")], hook).await.0, StatusCode::OK);

        assert_eq!(sign_in(&app, "correct horse batter").await.0, unauthorized);
        let (status, headers) = sign_in(&app, PASSWORD).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let cookie = headers["set-cookie"].to_str().unwrap();
        for attr in ["HttpOnly", "SameSite=Strict", "Secure", "Path=/"] {
            assert!(cookie.contains(attr), "{cookie}");
        }
        let session = cookie.split(';').next().unwrap();
        let signed_in = [HOST, ("cookie", session)];
        let (status, _, traces) = call(&app, "GET", "/api/traces", &signed_in, "").await;
        assert_eq!((status, traces[0]["trace_id"].as_str()), (StatusCode::OK, Some("p")));
        let expected = serde_json::json!({"auth": true, "signed_in": true, "ingest_token": "tok-123", "public_url": "https://tracker.example.org"});
        assert_eq!(call(&app, "GET", "/api/session", &signed_in, "").await.2, expected);

        let forged = [HOST, ("cookie", "agent_tracker_session=0123456789abcdef")];
        assert_eq!(call(&app, "GET", "/api/traces", &forged, "").await.0, unauthorized, "only issued sessions count");

        // A sign-in is not an ingest token, and the token is not a sign-in.
        let span = r#"{"trace_id":"t","span_id":"a"}"#;
        assert_eq!(call(&app, "POST", "/api/spans", &[HOST, JSON, ("cookie", session)], span).await.0, unauthorized);
        assert_eq!(call(&app, "GET", "/api/traces", &[HOST, ("authorization", "Bearer tok-123")], "").await.0, unauthorized);

        call(&app, "POST", "/api/logout", &[HOST, JSON, ("cookie", session)], "{}").await;
        assert_eq!(call(&app, "GET", "/api/traces", &signed_in, "").await.0, unauthorized, "signed out");
        assert_eq!(call(&app, "GET", "/api/session", &[("host", "attacker.example")], "").await.0, StatusCode::FORBIDDEN);
    }

    /// A sign-in attempt arriving from `peer`, carrying `forwarded` as X-Forwarded-For.
    async fn sign_in_via(app: &Router, peer: [u8; 4], forwarded: &str, password: &str) -> StatusCode {
        let body = serde_json::json!({"username": "owner", "password": password}).to_string();
        let req = Request::builder()
            .method("POST")
            .uri("/api/login")
            .header("host", HOST.1)
            .header("content-type", "application/json")
            .header("x-forwarded-for", forwarded)
            .extension(ConnectInfo(std::net::SocketAddr::from((peer, 50_000))))
            .body(Body::from(body))
            .unwrap();
        app.clone().oneshot(req).await.unwrap().status()
    }

    #[tokio::test]
    async fn failed_sign_ins_lock_out_only_the_client_making_them() {
        let app = hosted();
        let proxy = [127, 0, 0, 1];
        // A proxy appends the address it saw; whatever the client wrote before it is ignored.
        for i in 0..10 {
            assert_eq!(sign_in_via(&app, proxy, &format!("10.9.9.{i}, 198.51.100.7"), "guess").await, StatusCode::UNAUTHORIZED);
        }
        assert_eq!(sign_in_via(&app, proxy, "198.51.100.7", PASSWORD).await, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(sign_in_via(&app, proxy, "192.0.2.1", PASSWORD).await, StatusCode::NO_CONTENT, "the owner, elsewhere, still signs in");

        // A client reaching the tracker directly is counted by its own address, whatever it claims.
        let direct = [203, 0, 113, 50];
        for i in 0..10 {
            assert_eq!(sign_in_via(&app, direct, &format!("192.0.2.{i}"), "guess").await, StatusCode::UNAUTHORIZED);
        }
        assert_eq!(sign_in_via(&app, direct, "192.0.2.99", PASSWORD).await, StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn sign_in_reads_only_a_small_body() {
        let big = serde_json::json!({"username": "owner", "password": "x".repeat(10_000)}).to_string();
        assert_eq!(call(&hosted(), "POST", "/api/login", &[HOST, JSON], big).await.0, StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn a_hosted_tracker_on_every_address_still_checks_the_host_name() {
        let auth = Auth::new("owner".into(), PASSWORD.into(), "tok-123".into(), Some("https://tracker.example.org"));
        let app = router(Arc::new(App { store: Store::in_memory(), claude_code: ClaudeCode::default(), codex: Codex::new(None), bind_host: "0.0.0.0".into(), auth: Some(auth) }));
        assert_eq!(call(&app, "GET", "/api/session", &[HOST], "").await.0, StatusCode::OK);
        assert_eq!(call(&app, "GET", "/api/session", &[("host", "attacker.example")], "").await.0, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_local_tracker_needs_no_sign_in() {
        assert_eq!(call(&app(), "GET", "/api/session", &[], "").await.2, serde_json::json!({"auth": false, "signed_in": true}));
    }

    #[test]
    fn web_session_events_are_labelled_and_timed_when_they_happened() {
        let app = App { store: Store::in_memory(), claude_code: ClaudeCode::default(), codex: Codex::new(None), bind_host: "127.0.0.1".into(), auth: None };
        let ev = serde_json::json!({"session_id": "s", "prompt_id": "p", "hook_event_name": "UserPromptSubmit", "prompt": "hi", "agent_tracker_time": 1000.0});
        web_session_event(&app, &ev).unwrap();
        let trace = &app.store.traces()[0];
        assert_eq!((trace.source.as_deref(), trace.start), (Some("claude-code-web"), 1000.0));
    }
}
