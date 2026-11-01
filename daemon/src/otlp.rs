//! OTLP/HTTP trace ingest. Maps the OpenTelemetry GenAI semantic conventions
//! (and OpenInference span kinds) onto the tracker's span model.

use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue, any_value};
use opentelemetry_proto::tonic::trace::v1::status::StatusCode;
use prost::Message;
use serde_json::{Map, Value};

use crate::model::{Kind, SpanUpdate, Status};

pub fn decode(body: &[u8], content_type: &str) -> Result<ExportTraceServiceRequest, String> {
    if content_type.starts_with("application/json") {
        serde_json::from_slice(body).map_err(|e| format!("invalid OTLP JSON: {e}"))
    } else {
        ExportTraceServiceRequest::decode(body).map_err(|e| format!("invalid OTLP protobuf: {e}"))
    }
}

pub fn to_updates(req: ExportTraceServiceRequest) -> Vec<SpanUpdate> {
    let mut out = Vec::new();
    for rs in req.resource_spans {
        let service = rs
            .resource
            .map(|r| attrs_to_map(r.attributes))
            .and_then(|m| m.get("service.name").cloned());
        for ss in rs.scope_spans {
            for span in ss.spans {
                let trace_id = hex(&span.trace_id);
                let mut attrs = attrs_to_map(span.attributes);
                if let Some(svc) = &service {
                    attrs.insert("service.name".into(), svc.clone());
                }
                let str_attr = |k: &str| attrs.get(k).and_then(Value::as_str).map(str::to_string);
                let first = |keys: &[&str]| keys.iter().find_map(|k| attrs.get(*k)).map(parse_json_string);
                let int_attr = |keys: &[&str]| keys.iter().find_map(|k| attrs.get(*k).and_then(Value::as_u64));

                let kind = kind_of(str_attr("gen_ai.operation.name"), str_attr("openinference.span.kind"));
                let status = span.status.as_ref();
                let error = status.filter(|s| s.code == StatusCode::Error as i32).map(|s| s.message.clone());
                let update = SpanUpdate {
                    parent_id: (!span.parent_span_id.is_empty()).then(|| hex(&span.parent_span_id)),
                    kind: Some(kind),
                    agent: str_attr("gen_ai.agent.name").or_else(|| (kind == Kind::Agent).then(|| span.name.clone())),
                    start: Some(span.start_time_unix_nano as f64 / 1e6),
                    end: (span.end_time_unix_nano > 0).then(|| span.end_time_unix_nano as f64 / 1e6),
                    status: Some(if error.is_some() { Status::Error } else { Status::Ok }),
                    error: error.filter(|m| !m.is_empty()),
                    input: first(&["gen_ai.input.messages", "gen_ai.prompt", "gen_ai.tool.call.arguments", "input.value"]),
                    output: first(&["gen_ai.output.messages", "gen_ai.completion", "gen_ai.tool.call.result", "output.value"]),
                    model: str_attr("gen_ai.response.model")
                        .or_else(|| str_attr("gen_ai.request.model"))
                        .or_else(|| str_attr("llm.model_name")),
                    tokens_in: int_attr(&["gen_ai.usage.input_tokens", "llm.token_count.prompt"]),
                    tokens_out: int_attr(&["gen_ai.usage.output_tokens", "llm.token_count.completion"]),
                    links: span.links.iter().filter(|l| l.trace_id == span.trace_id).map(|l| hex(&l.span_id)).collect(),
                    name: Some(span.name),
                    ..SpanUpdate::new(trace_id, hex(&span.span_id))
                };
                out.push(SpanUpdate { attrs, ..update });
            }
        }
    }
    out
}

fn kind_of(op: Option<String>, openinference: Option<String>) -> Kind {
    match op.as_deref() {
        Some("invoke_agent" | "create_agent") => return Kind::Agent,
        Some("execute_tool") => return Kind::Tool,
        Some("chat" | "text_completion" | "generate_content" | "embeddings") => return Kind::Llm,
        _ => {}
    }
    match openinference.as_deref().map(str::to_ascii_uppercase).as_deref() {
        Some("AGENT") => Kind::Agent,
        Some("TOOL") => Kind::Tool,
        Some("LLM" | "EMBEDDING") => Kind::Llm,
        _ => Kind::Step,
    }
}

/// GenAI payload attributes are JSON encoded as strings; show them as JSON.
fn parse_json_string(v: &Value) -> Value {
    match v.as_str().map(serde_json::from_str::<Value>) {
        Some(Ok(parsed)) if parsed.is_object() || parsed.is_array() => parsed,
        _ => v.clone(),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn attrs_to_map(kvs: Vec<KeyValue>) -> Map<String, Value> {
    kvs.into_iter().filter_map(|kv| Some((kv.key, any_to_json(kv.value?)))).collect()
}

fn any_to_json(v: AnyValue) -> Value {
    use any_value::Value as V;
    match v.value {
        Some(V::StringValue(s)) => Value::from(s),
        Some(V::BoolValue(b)) => Value::from(b),
        Some(V::IntValue(i)) => Value::from(i),
        Some(V::DoubleValue(d)) => Value::from(d),
        Some(V::BytesValue(b)) => Value::from(hex(&b)),
        Some(V::ArrayValue(a)) => Value::Array(a.values.into_iter().map(any_to_json).collect()),
        Some(V::KvlistValue(kv)) => Value::Object(attrs_to_map(kv.values)),
        _ => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = include_str!("../fixtures/otlp-genai.json");

    #[test]
    fn maps_genai_json_export() {
        let spans = to_updates(decode(SAMPLE.as_bytes(), "application/json").unwrap());
        let by = |id: &str| spans.iter().find(|s| s.span_id == id).unwrap();

        let agent = by("1111111111111111");
        assert_eq!(agent.trace_id, "5b8efff798038103d269b633813fc60c");
        assert_eq!((agent.kind, agent.agent.as_deref(), agent.parent_id.as_deref()), (Some(Kind::Agent), Some("weather-agent"), None));
        assert_eq!(agent.start, Some(1_700_000_000_000.0));
        assert_eq!(agent.end, Some(1_700_000_002_500.0));
        assert_eq!(agent.attrs["service.name"], "demo-svc");

        let chat = by("2222222222222222");
        assert_eq!((chat.kind, chat.parent_id.as_deref()), (Some(Kind::Llm), Some("1111111111111111")));
        assert_eq!((chat.model.as_deref(), chat.tokens_in, chat.tokens_out), (Some("claude-sonnet-5-5"), Some(120), Some(30)));
        assert!(chat.input.as_ref().unwrap().is_array(), "input messages JSON string is parsed");

        let tool = by("3333333333333333");
        assert_eq!((tool.kind, tool.status, tool.error.as_deref()), (Some(Kind::Tool), Some(Status::Error), Some("timeout")));
        assert_eq!(tool.links, vec!["2222222222222222".to_string()]);
    }

    #[test]
    fn protobuf_and_json_decode_to_the_same_spans() {
        let req: ExportTraceServiceRequest = serde_json::from_str(SAMPLE).unwrap();
        let bytes = req.encode_to_vec();
        let from_proto = to_updates(decode(&bytes, "application/x-protobuf").unwrap());
        let from_json = to_updates(decode(SAMPLE.as_bytes(), "application/json").unwrap());
        assert_eq!(from_proto, from_json);
        assert_eq!(from_proto.len(), 3);
    }

    #[test]
    fn openinference_kind_is_a_fallback() {
        assert_eq!(kind_of(None, Some("tool".into())), Kind::Tool);
        assert_eq!(kind_of(Some("execute_tool".into()), Some("LLM".into())), Kind::Tool);
        assert_eq!(kind_of(None, None), Kind::Step);
    }
}
