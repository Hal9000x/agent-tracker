use std::convert::Infallible;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
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

use crate::claude_code::ClaudeCode;
use crate::model::SpanUpdate;
use crate::otlp;
use crate::store::Store;

pub struct App {
    pub store: Store,
    pub claude_code: ClaudeCode,
}

#[derive(Embed)]
#[folder = "../web/dist"]
struct Assets;

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/api/spans", post(ingest_spans))
        .route("/api/traces", get(list_traces))
        .route("/api/traces/{id}", get(get_trace))
        .route("/api/stream", get(stream))
        .route("/api/hooks/claude-code", post(claude_code_hook))
        .route("/v1/traces", post(otlp_traces))
        .fallback(static_files)
        .layer(DefaultBodyLimit::max(64 << 20))
        .with_state(app)
}

type ApiError = (StatusCode, String);

fn now_ms() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64() * 1000.0)
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
        router(Arc::new(App { store: Store::in_memory(), claude_code: ClaudeCode::default() }))
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
    async fn rejects_bad_spans() {
        let app = app();
        let (status, _) = send(&app, "POST", "/api/spans", "application/json", r#"{"trace_id":"","span_id":"a"}"#).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = send(&app, "POST", "/api/spans", "application/json", r#"{"span_id":"a"}"#).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(send(&app, "GET", "/api/traces/nope", "", "").await.0, StatusCode::NOT_FOUND);
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
    }
}
