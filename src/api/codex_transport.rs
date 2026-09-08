use std::collections::{BTreeMap, BTreeSet};
#[cfg(test)]
use std::future::Future;

use axum::body::Bytes;
use futures_util::StreamExt;
use http::header;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use super::super::sse::{
    BoundedSseEvent, BoundedSseFramer, ResponseIdentityGate, ResponsesStreamingSanitizer,
    SseFramerRejection, is_response_metadata_event, is_sse_field_line, parse_sse_event,
    parse_unique_json, trim_ascii,
};
use super::{
    MAX_PROXY_LIFETIME, MAX_PROXY_RESPONSE_BODY, MAX_REPORTED_TOKENS,
    MAX_RESPONSES_SSE_EVENT_BYTES, Protocol, TokenUsage, upstream_response::UpstreamResponse,
};
use crate::{
    error::AppError, oauth::managed::codex::account_header_value, provider::UpstreamCredential,
};

#[path = "codex_transport/bad_request.rs"]
mod bad_request;
#[cfg(test)]
use bad_request::codex_transient_error;
pub(super) use bad_request::{
    BadRequestDisposition, BadRequestUnclassifiableReason, classify_bad_request,
};

pub(super) const DRIVER: &str = "openai-codex";
pub(super) const BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
pub(super) const RESPONSES_PATH: &str = "/responses";
// A conservative identity remains the fallback when a downstream caller is
// not a recognized first-party Codex client. Native Codex may safely preserve
// a narrowly-defined client identity below, but account configuration never
// controls either header.
pub(super) const USER_AGENT: &str = crate::oauth::managed::codex::USER_AGENT;
const DEFAULT_ORIGINATOR: &str = "codex-tui";
const MAX_CODEX_ORIGINATOR_BYTES: usize = 128;
const MAX_CODEX_USER_AGENT_BYTES: usize = 512;
const EXACT_CODEX_ORIGINATORS: &[&str] = &[
    "codex_cli_rs",
    "codex-tui",
    "codex_vscode",
    "codex_atlas",
    "codex_chatgpt_desktop",
    "codex-chrome-extension-sidepanel",
];
const MAX_OUTPUT_ITEMS: usize = 16_384;
// A 400 is normally a client-side rejection and must not affect account
// health. Keep the exceptional definite-rejection inspection deliberately
// small: it happens before any downstream bytes are delivered and its body is
// never retained, archived, logged, or returned.
const MISSING_CONTENT_TYPE_SNIFF_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

pub(super) fn is_driver(driver: &str) -> bool {
    driver == DRIVER
}

#[cfg(test)]
tokio::task_local! {
    static TEST_ENDPOINT: String;
}

const CLIENT_OUTPUT_LIMIT_FIELDS: &[&str] = &[
    "max_output_tokens",
    "max_completion_tokens",
    "max_tokens",
    "output_token_limits",
    "reservation_token_bounds",
];

const UNSUPPORTED_FIELDS: &[&str] = &[
    "temperature",
    "top_p",
    "truncation",
    "context_management",
    "user",
    "previous_response_id",
    "generate",
    "prompt_cache_retention",
    "prompt_cache_options",
    "safety_identifier",
    "stream_options",
];
const PASSTHROUGH_HEADERS: &[&str] = &[
    "version",
    "x-codex-beta-features",
    "x-codex-turn-metadata",
    "x-client-request-id",
    "x-codex-window-id",
    "thread-id",
];
const MAX_PASSTHROUGH_HEADER_BYTES: usize = 4 * 1024;
const IMAGE_GENERATION_TOOL_TYPE: &str = "image_generation";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CodexClientIdentity<'a> {
    originator: &'a str,
    user_agent: &'a str,
}

/// Preserve only an auditable subset of the downstream Codex fingerprint.
///
/// These two fields are compatibility metadata for the fixed native Codex
/// endpoint, not a general header-forwarding mechanism. In particular, no
/// authentication, forwarding, browser, proxy, or response header can reach
/// the upstream through this path.
fn codex_client_identity(headers: &http::HeaderMap) -> CodexClientIdentity<'_> {
    select_codex_client_identity(
        single_visible_header(headers, "originator"),
        single_visible_header(headers, header::USER_AGENT.as_str()),
    )
}

fn select_codex_client_identity<'a>(
    originator: Option<&'a str>,
    user_agent: Option<&'a str>,
) -> CodexClientIdentity<'a> {
    match (originator, user_agent) {
        (Some(originator), Some(user_agent))
            if is_allowed_codex_originator(originator)
                && is_matching_codex_user_agent(originator, user_agent) =>
        {
            CodexClientIdentity {
                originator,
                user_agent,
            }
        }
        _ => CodexClientIdentity {
            originator: DEFAULT_ORIGINATOR,
            user_agent: USER_AGENT,
        },
    }
}

fn single_visible_header<'a>(headers: &'a http::HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?;
    values.next().is_none().then_some(value)
}

fn is_allowed_codex_originator(originator: &str) -> bool {
    if !bounded_visible_ascii(originator, MAX_CODEX_ORIGINATOR_BYTES) {
        return false;
    }
    EXACT_CODEX_ORIGINATORS.contains(&originator)
        || originator
            .strip_prefix("Codex ")
            .is_some_and(is_bounded_codex_originator_suffix)
}

fn is_bounded_codex_originator_suffix(suffix: &str) -> bool {
    !suffix.is_empty()
        && suffix.len() <= MAX_CODEX_ORIGINATOR_BYTES - "Codex ".len()
        && suffix
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b' ' | b'-' | b'_' | b'.'))
}

fn is_matching_codex_user_agent(originator: &str, user_agent: &str) -> bool {
    if !bounded_visible_ascii(user_agent, MAX_CODEX_USER_AGENT_BYTES) {
        return false;
    }
    let expected_prefix = match originator {
        "codex_cli_rs" => "codex_cli_rs/",
        "codex-tui" => "codex-tui/",
        "codex_vscode" => "codex_vscode/",
        "codex_atlas" => "codex_atlas/",
        "codex_chatgpt_desktop" => "codex_chatgpt_desktop/",
        "codex-chrome-extension-sidepanel" => "codex-chrome-extension-sidepanel/",
        originator if originator.starts_with("Codex ") => "Codex ",
        _ => return false,
    };
    user_agent.starts_with(expected_prefix)
}

fn bounded_visible_ascii(value: &str, max_bytes: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_bytes
        && value.bytes().all(|byte| matches!(byte, b' '..=b'~'))
}

pub(super) struct PreparedCodexRequest {
    pub downstream_stream: bool,
    pub output_token_ceiling: i64,
    pub session_id: String,
}

pub(super) fn validate_protocol(protocol: Protocol) -> Result<(), AppError> {
    if matches!(protocol, Protocol::OpenAiResponses) {
        Ok(())
    } else {
        Err(AppError::BadRequest(
            "OpenAI Codex supports the Responses protocol only".into(),
        ))
    }
}

pub(super) fn outbound_base_url(configured: &str) -> String {
    #[cfg(test)]
    if let Ok(endpoint) = TEST_ENDPOINT.try_with(Clone::clone) {
        return endpoint;
    }
    configured.to_owned()
}

/// Unit-test-only task-local endpoint substitution. The production artifact
/// has no corresponding configuration field, environment variable, or code
/// path; persisted Codex accounts must still pass the fixed-base check.
#[cfg(test)]
pub(super) async fn with_test_endpoint<F>(endpoint: String, future: F) -> F::Output
where
    F: Future,
{
    TEST_ENDPOINT.scope(endpoint, future).await
}

/// Rewrite only the upstream wire document. The original downstream body is
/// archived by `proxy()` before this document is sent.
#[cfg(test)]
pub(super) fn prepare_request(
    request: &mut Value,
    upstream_model: &str,
    config: &Value,
) -> Result<PreparedCodexRequest, AppError> {
    prepare_request_with_id(request, upstream_model, config, Uuid::nil())
}

pub(super) fn prepare_request_with_id(
    request: &mut Value,
    upstream_model: &str,
    config: &Value,
    request_id: Uuid,
) -> Result<PreparedCodexRequest, AppError> {
    validate_route_config(config)?;
    let object = request
        .as_object_mut()
        .ok_or_else(|| AppError::BadRequest("request body must be a JSON object".into()))?;
    let downstream_stream = match object.get("stream") {
        None => false,
        Some(Value::Bool(stream)) => *stream,
        Some(_) => return Err(AppError::BadRequest("stream must be a boolean".into())),
    };
    if CLIENT_OUTPUT_LIMIT_FIELDS
        .iter()
        .any(|field| object.contains_key(*field))
    {
        return Err(AppError::BadRequest(
            "Codex OAuth routes do not accept client output-token limits".into(),
        ));
    }
    validate_service_tier(object.get("service_tier"))?;
    let output_token_ceiling = trusted_reservation_token_bound(config, upstream_model)?;

    object.insert("model".to_owned(), Value::String(upstream_model.to_owned()));
    object.insert("stream".to_owned(), Value::Bool(true));
    object.insert("store".to_owned(), Value::Bool(false));
    ensure_image_generation_tool(object, upstream_model);
    if object
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty())
    {
        object.insert("parallel_tool_calls".to_owned(), Value::Bool(true));
    } else {
        object.remove("parallel_tool_calls");
    }
    match object.get("instructions") {
        None | Some(Value::Null) => {
            object.insert("instructions".to_owned(), Value::String(String::new()));
        }
        Some(Value::String(_)) => {}
        Some(_) => {
            return Err(AppError::BadRequest(
                "instructions must be a string or null".into(),
            ));
        }
    }
    for field in UNSUPPORTED_FIELDS {
        object.remove(*field);
    }
    normalize_include(object)?;
    let session_id = normalize_prompt_cache_key(object, request_id)?;
    normalize_string_input(object);
    rewrite_system_roles(request);

    Ok(PreparedCodexRequest {
        downstream_stream,
        output_token_ceiling,
        session_id,
    })
}

fn ensure_image_generation_tool(object: &mut Map<String, Value>, upstream_model: &str) {
    if upstream_model.ends_with("spark") {
        return;
    }
    let tools = object
        .entry("tools".to_owned())
        .or_insert_with(|| Value::Array(Vec::new()));
    let Some(tools) = tools.as_array_mut() else {
        return;
    };
    if tools.iter().any(is_image_generation_tool) {
        return;
    }
    tools.push(json!({
        "type": IMAGE_GENERATION_TOOL_TYPE,
        "output_format": "png"
    }));
}

fn is_image_generation_tool(tool: &Value) -> bool {
    let Some(tool) = tool.as_object() else {
        return false;
    };
    match tool.get("type").and_then(Value::as_str) {
        Some(IMAGE_GENERATION_TOOL_TYPE) => true,
        Some("function") => tool.get("name").and_then(Value::as_str) == Some("image_gen.imagegen"),
        Some("namespace") if tool.get("name").and_then(Value::as_str) == Some("image_gen") => tool
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| {
                tools.iter().any(|tool| {
                    tool.get("type").and_then(Value::as_str) == Some("function")
                        && tool.get("name").and_then(Value::as_str) == Some("imagegen")
                })
            }),
        _ => false,
    }
}

fn normalize_prompt_cache_key(
    object: &mut Map<String, Value>,
    request_id: Uuid,
) -> Result<String, AppError> {
    let session_id = match object.get("prompt_cache_key") {
        None | Some(Value::Null) => request_id.to_string(),
        Some(Value::String(value))
            if !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control) =>
        {
            value.clone()
        }
        Some(_) => {
            return Err(AppError::BadRequest(
                "prompt_cache_key must be a bounded non-empty string".into(),
            ));
        }
    };
    object.insert(
        "prompt_cache_key".to_owned(),
        Value::String(session_id.clone()),
    );
    Ok(session_id)
}

pub(super) fn validate_route_config(config: &Value) -> Result<(), AppError> {
    let Some(object) = config.as_object() else {
        return Err(AppError::BadRequest(
            "OpenAI Codex account has invalid fixed transport configuration".into(),
        ));
    };
    if object.len() != 3
        || object.get("base_url").and_then(Value::as_str) != Some(BASE_URL)
        || object.get("network_scope").and_then(Value::as_str) != Some("public")
        || reservation_bounds(config).is_none()
    {
        return Err(AppError::BadRequest(
            "OpenAI Codex account has invalid fixed transport configuration".into(),
        ));
    }
    Ok(())
}

fn trusted_reservation_token_bound(config: &Value, upstream_model: &str) -> Result<i64, AppError> {
    let bounds = reservation_bounds(config).ok_or_else(|| {
        AppError::BadRequest("OpenAI Codex account requires trusted reservation metadata".into())
    })?;
    for (model, bound) in bounds {
        if model.is_empty()
            || model.len() > 500
            || model.chars().any(char::is_control)
            || !bound
                .as_i64()
                .is_some_and(|value| (1..=MAX_REPORTED_TOKENS).contains(&value))
        {
            return Err(AppError::BadRequest(
                "OpenAI Codex reservation metadata is invalid".into(),
            ));
        }
    }
    bounds
        .get(upstream_model)
        .and_then(Value::as_i64)
        .ok_or_else(|| {
            AppError::BadRequest(
                "OpenAI Codex route has no trusted reservation bound for its upstream model".into(),
            )
        })
}

/// Existing imported Codex rows used the old field name. Treat it as the same
/// conservative reservation bound internally, but never advertise or create
/// new accounts with that legacy spelling.
fn reservation_bounds(config: &Value) -> Option<&Map<String, Value>> {
    let current = config
        .get("reservation_token_bounds")
        .and_then(Value::as_object);
    let legacy = config.get("output_token_limits").and_then(Value::as_object);
    match (current, legacy) {
        (Some(bounds), None) | (None, Some(bounds)) => Some(bounds),
        _ => None,
    }
}

fn validate_service_tier(value: Option<&Value>) -> Result<(), AppError> {
    match value {
        None => Ok(()),
        Some(Value::String(tier))
            if matches!(
                tier.as_str(),
                "default" | "auto" | "standard_only" | "priority"
            ) =>
        {
            Ok(())
        }
        Some(_) => Err(AppError::BadRequest(
            "Codex OAuth service_tier must be default, auto, standard_only, or priority".into(),
        )),
    }
}

fn normalize_include(object: &mut Map<String, Value>) -> Result<(), AppError> {
    let mut include = match object.remove("include") {
        None => Vec::new(),
        Some(Value::Array(values)) => values,
        Some(_) => {
            return Err(AppError::BadRequest(
                "include must be an array of strings".into(),
            ));
        }
    };
    if include.len() > 256
        || include.iter().any(|entry| {
            !entry.as_str().is_some_and(|entry| {
                !entry.is_empty() && entry.len() <= 256 && !entry.chars().any(char::is_control)
            })
        })
    {
        return Err(AppError::BadRequest(
            "include must be a bounded array of strings".into(),
        ));
    }
    let mut seen = BTreeSet::new();
    include.retain(|entry| {
        entry
            .as_str()
            .is_some_and(|entry| seen.insert(entry.to_owned()))
    });
    if seen.insert("reasoning.encrypted_content".to_owned()) {
        include.push(Value::String("reasoning.encrypted_content".to_owned()));
    }
    object.insert("include".to_owned(), Value::Array(include));
    Ok(())
}

fn normalize_string_input(object: &mut Map<String, Value>) {
    let Some(Value::String(input)) = object.get("input") else {
        return;
    };
    let message = json!({
        "type": "message",
        "role": "user",
        "content": [{"type": "input_text", "text": input}]
    });
    object.insert("input".to_owned(), Value::Array(vec![message]));
}

fn rewrite_system_roles(value: &mut Value) {
    match value {
        Value::Array(values) => {
            for value in values {
                rewrite_system_roles(value);
            }
        }
        Value::Object(object) => {
            if object.get("role").and_then(Value::as_str) == Some("system") {
                object.insert("role".to_owned(), Value::String("developer".to_owned()));
            }
            for value in object.values_mut() {
                rewrite_system_roles(value);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

#[cfg(test)]
pub(super) fn apply_reqwest_wire_headers(
    request: reqwest::RequestBuilder,
    credential: &UpstreamCredential,
    session_id: &str,
) -> Result<reqwest::RequestBuilder, AppError> {
    validate_credential_contract(credential)?;
    let account_id = account_header_value(credential)?;
    let request = credential.apply(request, crate::db::unix_millis())?;
    Ok(request
        .header(header::ACCEPT, "text/event-stream")
        // The native Responses parser consumes SSE framing directly. Do not
        // negotiate a content coding that this transport does not decode.
        .header(header::ACCEPT_ENCODING, "identity")
        .header(header::CONTENT_TYPE, "application/json")
        .header("originator", "codex-tui")
        .header(header::USER_AGENT, USER_AGENT)
        .header("session-id", session_id)
        .header("chatgpt-account-id", account_id))
}

pub(super) fn apply_wreq_wire_headers(
    request: wreq::RequestBuilder,
    downstream_headers: &http::HeaderMap,
    credential: &UpstreamCredential,
    session_id: &str,
    now: i64,
) -> Result<wreq::RequestBuilder, AppError> {
    validate_credential_contract(credential)?;
    let account_id = account_header_value(credential)?;
    let Some((credential_header, credential_value)) = credential.request_header(now)? else {
        return Err(AppError::BadRequest(
            "OpenAI Codex credential is missing authorization".into(),
        ));
    };
    let client_identity = codex_client_identity(downstream_headers);
    let mut request = request.default_headers(false);
    for name in PASSTHROUGH_HEADERS {
        if let Some(value) = downstream_headers.get(*name) {
            if value.as_bytes().len() > MAX_PASSTHROUGH_HEADER_BYTES {
                return Err(AppError::BadRequest(
                    "Codex metadata header exceeds its size limit".into(),
                ));
            }
            request = request.header(*name, value.clone());
        }
    }
    Ok(request
        .header(credential_header, credential_value)
        .header(header::ACCEPT, "text/event-stream")
        .header(header::ACCEPT_ENCODING, "identity")
        .header(header::CONTENT_TYPE, "application/json")
        .header("originator", client_identity.originator)
        .header(header::USER_AGENT, client_identity.user_agent)
        .header("session-id", session_id)
        .header("chatgpt-account-id", account_id))
}

pub(super) fn validate_credential_contract(
    credential: &UpstreamCredential,
) -> Result<(), AppError> {
    if !matches!(
        credential,
        UpstreamCredential::OAuth { header, prefix, .. }
            if header == "authorization" && prefix == "Bearer "
    ) {
        return Err(AppError::BadRequest(
            "OpenAI Codex credential has an invalid authorization contract".into(),
        ));
    }
    let _ = account_header_value(credential)?;
    Ok(())
}

pub(super) fn is_event_stream(response: &UpstreamResponse) -> bool {
    let mut values = response.headers().get_all(header::CONTENT_TYPE).iter();
    let Some(value) = values.next() else {
        return false;
    };
    if values.next().is_some() {
        return false;
    }
    value
        .to_str()
        .ok()
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("text/event-stream"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ResponseAdmissionError {
    Invalid(&'static str),
    Ambiguous(&'static str),
}

pub(super) async fn admit_event_stream_response(
    response: UpstreamResponse,
) -> Result<UpstreamResponse, ResponseAdmissionError> {
    if is_event_stream(&response) {
        return Ok(response);
    }
    if response.headers().contains_key(header::CONTENT_TYPE) {
        return Err(ResponseAdmissionError::Invalid(
            "upstream_invalid_content_type",
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_PROXY_RESPONSE_BODY as u64)
    {
        return Err(ResponseAdmissionError::Invalid(
            "upstream_response_too_large",
        ));
    }

    let mut parts = response.into_parts();
    let deadline = tokio::time::Instant::now() + MISSING_CONTENT_TYPE_SNIFF_TIMEOUT;
    let mut prefetched = Vec::new();
    let mut inspected = Vec::new();
    let mut sanitizer = ResponsesStreamingSanitizer::default();
    loop {
        let next = tokio::time::timeout_at(deadline, parts.stream.next())
            .await
            .map_err(|_| ResponseAdmissionError::Ambiguous("upstream_timeout"))?;
        let Some(next) = next else {
            return Err(ResponseAdmissionError::Invalid(
                "upstream_invalid_content_type",
            ));
        };
        let chunk = next.map_err(|_| ResponseAdmissionError::Ambiguous("upstream_stream"))?;
        let mut accepted_at = None;
        for (index, byte) in chunk.iter().enumerate() {
            if inspected.len() == MAX_RESPONSES_SSE_EVENT_BYTES {
                return Err(ResponseAdmissionError::Invalid(
                    "upstream_response_event_too_large",
                ));
            }
            inspected.push(*byte);
            sanitizer
                .push(std::slice::from_ref(byte))
                .map_err(ResponseAdmissionError::Invalid)?;
            if sanitizer.saw_protocol_event() {
                accepted_at = Some(index + 1);
                break;
            }
        }
        if let Some(accepted_at) = accepted_at {
            validate_missing_content_type_prefix(&inspected)
                .map_err(ResponseAdmissionError::Invalid)?;
            prefetched.push(chunk.slice(..accepted_at));
            if accepted_at < chunk.len() {
                prefetched.push(chunk.slice(accepted_at..));
            }
            return Ok(UpstreamResponse::from_prefetched_parts(
                parts,
                prefetched,
                http::HeaderValue::from_static("text/event-stream"),
            ));
        }
        if !chunk.is_empty() {
            prefetched.push(chunk);
        }
    }
}

fn validate_missing_content_type_prefix(prefix: &[u8]) -> Result<(), &'static str> {
    // Headerless admission uses the same CR/LF/CRLF bounded scanner as
    // delivery; it must not grow a fourth LF-only parser with divergent EOF
    // and line-ending semantics.
    let mut framer = BoundedSseFramer::default();
    let batch = framer.push(prefix);
    if batch.rejection.is_some() || !framer.is_complete() {
        return Err("upstream_invalid_content_type");
    }
    for event in batch.events {
        for line in event.lines {
            let line = line.value;
            if line.starts_with(b":")
                || is_sse_field_line(&line, b"event")
                || is_sse_field_line(&line, b"data")
                || is_sse_field_line(&line, b"id")
                || is_sse_field_line(&line, b"retry")
            {
                continue;
            }
            return Err("upstream_invalid_content_type");
        }
    }
    Ok(())
}

pub(super) fn content_type_class(response: &UpstreamResponse) -> &'static str {
    let Some(value) = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim)
    else {
        return "missing";
    };
    if value.eq_ignore_ascii_case("text/event-stream") {
        "sse"
    } else if value.eq_ignore_ascii_case("application/json") || value.ends_with("+json") {
        "json"
    } else if value.eq_ignore_ascii_case("text/html") {
        "html"
    } else if value.starts_with("text/") {
        "text"
    } else {
        "other"
    }
}

pub(super) fn http_version_class(response: &UpstreamResponse) -> &'static str {
    match response.version() {
        http::Version::HTTP_09 => "0.9",
        http::Version::HTTP_10 => "1.0",
        http::Version::HTTP_11 => "1.1",
        http::Version::HTTP_2 => "2",
        http::Version::HTTP_3 => "3",
        _ => "other",
    }
}

pub(super) struct BufferedCodexResponse {
    pub body: Bytes,
    pub usage: TokenUsage,
}

pub(super) async fn buffer_response(
    response: UpstreamResponse,
) -> Result<BufferedCodexResponse, &'static str> {
    if !is_event_stream(&response) {
        return Err("upstream_invalid_content_type");
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_PROXY_RESPONSE_BODY as u64)
    {
        return Err("upstream_response_too_large");
    }
    let deadline = tokio::time::Instant::now() + MAX_PROXY_LIFETIME;
    let mut parser = BufferedResponsesParser::default();
    let mut total = 0_usize;
    let mut stream = response.bytes_stream();
    loop {
        let next = tokio::time::timeout_at(deadline, stream.next())
            .await
            .map_err(|_| "upstream_timeout")?;
        let Some(next) = next else { break };
        let chunk = next.map_err(|_| "upstream_stream")?;
        total = total.saturating_add(chunk.len());
        if total > MAX_PROXY_RESPONSE_BODY {
            return Err("upstream_response_too_large");
        }
        parser.push(&chunk)?;
    }
    parser.finish()
}

#[derive(Default)]
struct BufferedResponsesParser {
    framer: BoundedSseFramer,
    output_items: BTreeMap<usize, Value>,
    identity: ResponseIdentityGate,
    completed_response: Option<Value>,
    terminal_failure: bool,
    invalid: bool,
}

impl BufferedResponsesParser {
    fn push(&mut self, chunk: &[u8]) -> Result<(), &'static str> {
        let batch = self.framer.push(chunk);
        if let Some(rejection) = batch.rejection {
            return Err(match rejection {
                SseFramerRejection::EventLimit => "upstream_response_event_too_large",
                SseFramerRejection::BatchLimit => "upstream_response_event_batch_too_large",
            });
        }
        for event in batch.events {
            self.dispatch(event)?;
        }
        Ok(())
    }

    fn finish(mut self) -> Result<BufferedCodexResponse, &'static str> {
        if !self.framer.is_complete() {
            return Err("upstream_incomplete_response");
        }
        if self.invalid || self.terminal_failure {
            return Err("upstream_failed_response");
        }
        let mut response = self
            .completed_response
            .take()
            .ok_or("upstream_incomplete_response")?;
        let completed_output = response
            .get("output")
            .and_then(Value::as_array)
            .ok_or("upstream_invalid_response")?;
        if !self.output_items.is_empty() {
            if self.output_items.len() > MAX_OUTPUT_ITEMS
                || self
                    .output_items
                    .keys()
                    .copied()
                    .ne(0..self.output_items.len())
            {
                return Err("upstream_invalid_response");
            }
            let output = self.output_items.into_values().collect::<Vec<_>>();
            if completed_output.is_empty() {
                response
                    .as_object_mut()
                    .ok_or("upstream_invalid_response")?
                    .insert("output".to_owned(), Value::Array(output));
            } else if completed_output != &output {
                return Err("upstream_invalid_response");
            }
        }
        let usage = canonical_responses_usage(&response).map_err(|_| "upstream_invalid_usage")?;
        let body = serde_json::to_vec(&response).map_err(|_| "upstream_invalid_response")?;
        if body.len() > MAX_PROXY_RESPONSE_BODY {
            return Err("upstream_response_too_large");
        }
        Ok(BufferedCodexResponse {
            body: Bytes::from(body),
            usage,
        })
    }

    fn dispatch(&mut self, event: BoundedSseEvent) -> Result<(), &'static str> {
        let (event_name, data) = parse_sse_event(&event)?;
        let Some(data) = data else {
            return Ok(());
        };
        if data.is_empty() {
            return Ok(());
        }
        let data = trim_ascii(&data);
        if data == b"[DONE]" {
            return if self.completed_response.is_some() || self.terminal_failure {
                Ok(())
            } else {
                Err("upstream_incomplete_response")
            };
        }
        let value = parse_unique_json(data)?;
        let payload_kind = value
            .get("type")
            .and_then(Value::as_str)
            .ok_or("upstream_invalid_response")?;
        if payload_kind != "error" && !payload_kind.starts_with("response.") {
            return Err("upstream_invalid_response");
        }
        let event_kind = event_name.as_deref();
        if event_kind.is_some_and(|event_kind| event_kind != payload_kind) {
            return Err("upstream_invalid_response");
        }
        if is_response_metadata_event(payload_kind) {
            return Ok(());
        }
        let kind = payload_kind;
        if self.completed_response.is_some() || self.terminal_failure {
            self.invalid = true;
            return Ok(());
        }
        if value.get("error").is_some_and(|error| !error.is_null())
            || value
                .pointer("/response/error")
                .is_some_and(|error| !error.is_null())
        {
            self.terminal_failure = true;
            return Ok(());
        }
        self.identity.observe(kind, &value)?;
        match kind {
            "response.output_item.done" => {
                if self.completed_response.is_some() || self.terminal_failure {
                    self.invalid = true;
                    return Ok(());
                }
                let index = value
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .and_then(|value| usize::try_from(value).ok())
                    .filter(|value| *value < MAX_OUTPUT_ITEMS)
                    .ok_or("upstream_invalid_response")?;
                let item = value
                    .get("item")
                    .filter(|item| item.is_object())
                    .cloned()
                    .ok_or("upstream_invalid_response")?;
                if self.output_items.insert(index, item).is_some() {
                    self.invalid = true;
                }
            }
            "response.completed" => {
                if self.completed_response.is_some() || self.terminal_failure {
                    self.invalid = true;
                    return Ok(());
                }
                self.completed_response = Some(
                    value
                        .get("response")
                        .filter(|response| response.is_object())
                        .cloned()
                        .ok_or("upstream_invalid_response")?,
                );
            }
            "response.failed" | "response.incomplete" | "response.error" | "error" => {
                if self.completed_response.is_some() || self.terminal_failure {
                    self.invalid = true;
                }
                self.terminal_failure = true;
            }
            _ => {}
        }
        Ok(())
    }
}

/// Parse the one canonical Responses API usage shape shared by buffered and
/// direct Codex delivery. Callers must pass the completed `response` object,
/// never an outer SSE event envelope.
pub(super) fn canonical_responses_usage(response: &Value) -> Result<TokenUsage, ()> {
    let usage = response.get("usage").and_then(Value::as_object).ok_or(())?;
    let required_integer = |field: &str| -> Result<i64, ()> {
        usage
            .get(field)
            .and_then(Value::as_i64)
            .filter(|value| (0..=MAX_REPORTED_TOKENS).contains(value))
            .ok_or(())
    };
    let reported_input = required_integer("input_tokens")?;
    let output_tokens = required_integer("output_tokens")?;
    let total_tokens = usage
        .get("total_tokens")
        .and_then(Value::as_i64)
        .filter(|value| *value >= 0)
        .ok_or(())?;
    if reported_input.checked_add(output_tokens) != Some(total_tokens) {
        return Err(());
    }
    let (cached_input_tokens, cache_write_tokens) = match usage.get("input_tokens_details") {
        None | Some(Value::Null) => (0, 0),
        Some(Value::Object(details)) => {
            let cached = details
                .get("cached_tokens")
                .and_then(Value::as_i64)
                .filter(|value| (0..=reported_input).contains(value))
                .ok_or(())?;
            let cache_write = match details.get("cache_write_tokens") {
                None => 0,
                Some(value) => value
                    .as_i64()
                    .filter(|value| (0..=reported_input).contains(value))
                    .ok_or(())?,
            };
            (cached, cache_write)
        }
        Some(_) => return Err(()),
    };
    if !matches!(
        cached_input_tokens.checked_add(cache_write_tokens),
        Some(total) if total <= reported_input
    ) {
        return Err(());
    }
    if usage
        .get("output_tokens_details")
        .is_some_and(|details| !details.is_null() && !details.is_object())
    {
        return Err(());
    }
    let service_tier = match response.get("service_tier") {
        None => None,
        Some(Value::String(tier))
            if matches!(
                tier.as_str(),
                "default" | "auto" | "standard_only" | "priority"
            ) =>
        {
            Some(tier.clone())
        }
        Some(_) => return Err(()),
    };
    Ok(TokenUsage {
        input_tokens: reported_input
            .checked_sub(cached_input_tokens)
            .and_then(|tokens| tokens.checked_sub(cache_write_tokens))
            .ok_or(())?,
        cached_input_tokens,
        cache_write_tokens,
        output_tokens,
        service_tier,
    })
}

#[cfg(test)]
pub(super) fn parse_buffered_sse_for_test(
    body: &[u8],
) -> Result<BufferedCodexResponse, &'static str> {
    let mut parser = BufferedResponsesParser::default();
    parser.push(body)?;
    parser.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_codex_cli_identity_passes_through() {
        let identity = select_codex_client_identity(
            Some("codex_cli_rs"),
            Some("codex_cli_rs/0.150.0 (Linux 6.8.0; x86_64) terminal/0.1"),
        );
        assert_eq!(identity.originator, "codex_cli_rs");
        assert_eq!(
            identity.user_agent,
            "codex_cli_rs/0.150.0 (Linux 6.8.0; x86_64) terminal/0.1"
        );
    }

    #[test]
    fn unsafe_or_mismatched_client_identity_falls_back_to_the_conservative_default() {
        let oversized_user_agent = format!("codex_cli_rs/{}", "x".repeat(512));
        for (originator, user_agent) in [
            ("codex-untrusted", "codex-untrusted/0.150.0"),
            ("codex_cli_rs", "codex-tui/0.150.0"),
            ("Codex \u{1f}Injected", "Codex client/0.150.0"),
            ("codex_cli_rs", "codex_cli_rs/0.150.0\nInjected"),
            ("codex_cli_rs", oversized_user_agent.as_str()),
        ] {
            let identity = select_codex_client_identity(Some(originator), Some(user_agent));
            assert_eq!(
                identity,
                CodexClientIdentity {
                    originator: DEFAULT_ORIGINATOR,
                    user_agent: USER_AGENT,
                },
                "{originator:?} / {user_agent:?}"
            );
        }
    }

    #[test]
    fn retryable_bad_request_classification_requires_known_transient_semantics() {
        assert!(codex_transient_error(&json!({
            "error": {"type": "temporarily_unavailable", "message": "private detail"}
        })));
        assert!(codex_transient_error(&json!({
            "error": {"type": "request_error", "message": "The service is under high demand."}
        })));
        assert!(!codex_transient_error(&json!({
            "error": {"type": "invalid_request_error", "message": "context is too long"}
        })));
        assert!(!codex_transient_error(&json!({
            "error": {"type": "invalid_request_error"}
        })));
    }

    fn config(model: &str, limit: i64) -> Value {
        json!({
            "base_url": BASE_URL,
            "network_scope": "public",
            "reservation_token_bounds": {model: limit}
        })
    }

    #[test]
    fn rewrite_is_idempotent_and_preserves_downstream_stream_capture() {
        let mut body = json!({
            "model": "public",
            "input": "hello",
            "stream": false,
            "store": true,
            "parallel_tool_calls": false,
            "temperature": 0.5,
            "include": ["reasoning.encrypted_content", "reasoning.encrypted_content"],
            "nested": {"role": "system", "children": [{"role": "system"}]}
        });
        let plan = prepare_request(&mut body, "gpt-codex", &config("gpt-codex", 65_536)).unwrap();
        assert!(!plan.downstream_stream);
        assert_eq!(plan.output_token_ceiling, 65_536);
        assert_eq!(body["stream"], true);
        assert_eq!(body["store"], false);
        assert_eq!(body["parallel_tool_calls"], true);
        assert_eq!(
            body["tools"],
            json!([{"type": "image_generation", "output_format": "png"}])
        );
        assert_eq!(body["instructions"], "");
        assert!(body.get("temperature").is_none());
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["input"][0]["content"][0]["text"], "hello");
        assert_eq!(body["nested"]["role"], "developer");
        assert_eq!(body["nested"]["children"][0]["role"], "developer");
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(body["prompt_cache_key"], Uuid::nil().to_string());
        assert_eq!(plan.session_id, Uuid::nil().to_string());

        let mut with_tools = json!({
            "model": "public",
            "input": [],
            "tools": [{"type": "function", "name": "lookup"}],
            "parallel_tool_calls": false
        });
        prepare_request(&mut with_tools, "gpt-codex", &config("gpt-codex", 65_536)).unwrap();
        assert_eq!(with_tools["parallel_tool_calls"], true);
        assert_eq!(with_tools["tools"].as_array().unwrap().len(), 2);

        let once = body.clone();
        let second = prepare_request(&mut body, "gpt-codex", &config("gpt-codex", 65_536)).unwrap();
        assert!(
            second.downstream_stream,
            "wire stream is true after first rewrite"
        );
        assert_eq!(body, once);

        let mut null_instructions = json!({"model": "public", "input": [], "instructions": null});
        prepare_request(
            &mut null_instructions,
            "gpt-codex",
            &config("gpt-codex", 10),
        )
        .unwrap();
        assert_eq!(null_instructions["instructions"], "");

        let mut spark = json!({"model": "public", "input": []});
        prepare_request(
            &mut spark,
            "gpt-5.3-codex-spark",
            &config("gpt-5.3-codex-spark", 10),
        )
        .unwrap();
        assert!(spark.get("tools").is_none());
        assert!(spark.get("parallel_tool_calls").is_none());

        let mut existing_image_tool = json!({
            "model": "public",
            "input": [],
            "tools": [{"type": "image_generation", "output_format": "jpeg"}]
        });
        prepare_request(
            &mut existing_image_tool,
            "gpt-codex",
            &config("gpt-codex", 10),
        )
        .unwrap();
        assert_eq!(existing_image_tool["tools"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn prompt_cache_key_is_preserved_and_reused_as_the_session_id() {
        let mut request = json!({
            "model": "public",
            "input": [],
            "prompt_cache_key": "downstream-session-42"
        });
        let plan = prepare_request(&mut request, "gpt-codex", &config("gpt-codex", 10)).unwrap();
        assert_eq!(plan.session_id, "downstream-session-42");
        assert_eq!(request["prompt_cache_key"], "downstream-session-42");

        for invalid in [
            Value::String(String::new()),
            Value::String("x".repeat(257)),
            json!(7),
        ] {
            let mut request = json!({"model": "public", "input": [], "prompt_cache_key": invalid});
            assert!(prepare_request(&mut request, "gpt-codex", &config("gpt-codex", 10)).is_err());
        }
    }

    #[test]
    fn admission_rejects_client_limits_missing_mismatched_and_invalid_metadata() {
        for field in CLIENT_OUTPUT_LIMIT_FIELDS {
            let mut request = json!({"model": "public", "input": []});
            request
                .as_object_mut()
                .unwrap()
                .insert((*field).to_owned(), Value::from(10));
            assert!(prepare_request(&mut request, "gpt-codex", &config("gpt-codex", 10)).is_err());
        }
        let cases = [
            json!({"base_url": BASE_URL}),
            config("other", 10),
            config("gpt-codex", 0),
            config("gpt-codex", MAX_REPORTED_TOKENS + 1),
            json!({"base_url": BASE_URL, "reservation_token_bounds": {"gpt-codex": "10"}}),
        ];
        for config in cases {
            let mut request = json!({"model": "public", "input": []});
            assert!(prepare_request(&mut request, "gpt-codex", &config).is_err());
        }

        let legacy = json!({
            "base_url": BASE_URL,
            "network_scope": "public",
            "output_token_limits": {"gpt-codex": 10}
        });
        let mut request = json!({"model": "public", "input": []});
        assert!(prepare_request(&mut request, "gpt-codex", &legacy).is_ok());

        for tier in ["flex", "scale", "batch"] {
            let mut request = json!({"model": "public", "input": [], "service_tier": tier});
            assert!(prepare_request(&mut request, "gpt-codex", &config("gpt-codex", 10)).is_err());
        }
        for tier in ["default", "auto", "standard_only", "priority"] {
            let mut request = json!({"model": "public", "input": [], "service_tier": tier});
            assert!(prepare_request(&mut request, "gpt-codex", &config("gpt-codex", 10)).is_ok());
            assert_eq!(request["service_tier"], tier);
        }
    }

    fn completed_stream() -> Vec<u8> {
        concat!(
            "event: response.queued\r\n",
            "data: {\"type\":\"response.queued\",\"response\":{\"id\":\"resp-1\"}}\r\n\r\n",
            "event: response.output_item.done\r\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{\"id\":\"item-1\"}}\r\n\r\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"item-0\"}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-1\",\"object\":\"response\",\"output\":[],\"usage\":{\"input_tokens\":3,\"output_tokens\":2,\"total_tokens\":5}}}\n\n",
            "data: [DONE]\n\n"
        )
        .as_bytes()
        .to_vec()
    }

    #[test]
    fn buffered_parser_handles_one_byte_crlf_multiple_events_and_output_order() {
        let mut parser = BufferedResponsesParser::default();
        for byte in completed_stream() {
            parser.push(&[byte]).unwrap();
        }
        let result = parser.finish().unwrap();
        let body: Value = serde_json::from_slice(&result.body).unwrap();
        assert_eq!(body["id"], "resp-1");
        assert_eq!(body["output"][0]["id"], "item-0");
        assert_eq!(body["output"][1]["id"], "item-1");
        assert_eq!(result.usage.input_tokens, 3);
        assert_eq!(result.usage.output_tokens, 2);
    }

    #[test]
    fn buffered_parser_drops_response_metadata_before_response_identity() {
        let stream = concat!(
            "event: response.metadata\n",
            "data: {\"type\":\"response.metadata\",\"response_id\":\"resp-metadata\"}\n\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-metadata\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-metadata\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n",
            "data: [DONE]\n\n"
        );
        let result = parse_buffered_sse_for_test(stream.as_bytes()).unwrap();
        let response: Value = serde_json::from_slice(&result.body).unwrap();
        assert_eq!(response["id"], "resp-metadata");
        assert_eq!(result.usage.input_tokens, 1);
        assert_eq!(result.usage.output_tokens, 1);
        assert!(response.get("metadata").is_none());

        for metadata in [
            b"data: {\"type\":\"response.metadata.extra\",\"response_id\":\"resp-metadata\"}\n\n"
                .as_slice(),
            b"event: response.created\ndata: {\"type\":\"response.metadata\",\"response_id\":\"resp-metadata\"}\n\n"
                .as_slice(),
        ] {
            let mut parser = BufferedResponsesParser::default();
            assert_eq!(parser.push(metadata), Err("upstream_invalid_response"));
        }
    }

    fn completed_stream_with_usage(usage: &Value) -> Vec<u8> {
        format!(
            concat!(
                "data: {{\"type\":\"response.queued\",\"response\":{{\"id\":\"resp-usage\"}}}}\n\n",
                "data: {{\"type\":\"response.output_item.done\",\"output_index\":0,",
                "\"item\":{{\"id\":\"item-billable\",\"type\":\"message\",",
                "\"role\":\"assistant\",\"content\":[{{\"type\":\"output_text\",",
                "\"text\":\"billable output\"}}]}}}}\n\n",
                "data: {{\"type\":\"response.completed\",\"response\":{{",
                "\"id\":\"resp-usage\",\"output\":[],\"service_tier\":\"priority\",",
                "\"usage\":{usage}}}}}\n\n"
            ),
            usage = usage
        )
        .into_bytes()
    }

    #[test]
    fn buffered_parser_requires_canonical_consistent_responses_usage() {
        let valid = json!({
            "input_tokens": 10,
            "input_tokens_details": {"cached_tokens": 3, "cache_write_tokens": 2},
            "output_tokens": 2,
            "output_tokens_details": null,
            "total_tokens": 12
        });
        let parsed = parse_buffered_sse_for_test(&completed_stream_with_usage(&valid)).unwrap();
        assert_eq!(
            parsed.usage,
            TokenUsage {
                input_tokens: 5,
                cached_input_tokens: 3,
                cache_write_tokens: 2,
                output_tokens: 2,
                service_tier: Some("priority".to_owned()),
            }
        );
        let response: Value = serde_json::from_slice(&parsed.body).unwrap();
        assert_eq!(response["output"][0]["id"], "item-billable");

        let nullable_details = json!({
            "input_tokens": 10,
            "input_tokens_details": null,
            "output_tokens": 2,
            "output_tokens_details": null,
            "total_tokens": 12
        });
        let parsed =
            parse_buffered_sse_for_test(&completed_stream_with_usage(&nullable_details)).unwrap();
        assert_eq!(
            parsed.usage,
            TokenUsage {
                input_tokens: 10,
                output_tokens: 2,
                service_tier: Some("priority".to_owned()),
                ..TokenUsage::default()
            }
        );

        let missing_output_tokens = json!({"input_tokens": 10, "total_tokens": 10});
        // Keep one complete buffered path assertion: malformed canonical
        // usage must retain the public invalid-usage classification rather
        // than becoming a successful response with a fallback charge.
        assert!(matches!(
            parse_buffered_sse_for_test(&completed_stream_with_usage(&missing_output_tokens)),
            Err("upstream_invalid_usage")
        ));

        // The remaining matrix belongs to the canonical usage boundary. The
        // outer SSE parser has independent identity/framing validation, so
        // exercising the shape here keeps these assertions about token
        // accounting rather than incidental transport rejection timing.
        for malformed in [
            missing_output_tokens,
            json!({"input_tokens": 10, "output_tokens": 2}),
            json!({"input_tokens": 10, "output_tokens": 2, "total_tokens": 11}),
            json!({"prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12}),
            json!({"input_tokens": 10, "input_tokens_details": {}, "output_tokens": 2, "total_tokens": 12}),
            json!({"input_tokens": 10, "input_tokens_details": {"cached_tokens": 11}, "output_tokens": 2, "total_tokens": 12}),
            json!({"input_tokens": 10, "input_tokens_details": {"cached_tokens": 9, "cache_write_tokens": 2}, "output_tokens": 2, "total_tokens": 12}),
            json!({"input_tokens": 10, "output_tokens": 2, "output_tokens_details": 1, "total_tokens": 12}),
            json!({"input_tokens": 10, "output_tokens": -1, "total_tokens": 9}),
        ] {
            assert!(
                canonical_responses_usage(&json!({
                    "id": "resp-usage",
                    "output": [],
                    "service_tier": "priority",
                    "usage": malformed,
                }))
                .is_err()
            );
        }
    }

    #[test]
    fn buffered_parser_requires_a_single_matching_completed_response_id() {
        let completed = |id: Option<&str>| {
            match id {
            Some(id) => format!(
                "data: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"{id}\",\"output\":[],\"usage\":{{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}}}}\n\n"
            ),
            None => "data: {\"type\":\"response.completed\",\"response\":{\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n".to_owned(),
        }
        };
        let created = |id: &str| {
            format!("data: {{\"type\":\"response.created\",\"response\":{{\"id\":\"{id}\"}}}}\n\n")
        };

        let mut matching = BufferedResponsesParser::default();
        matching
            .push(format!("{}{}", created("resp-1"), completed(Some("resp-1"))).as_bytes())
            .unwrap();
        assert!(matching.finish().is_ok());

        // Response IDs share the streaming conversation-ID contract: outer
        // whitespace is normalized before binding, whereas whitespace-only
        // values are not an identifier.
        let mut normalized = BufferedResponsesParser::default();
        normalized
            .push(
                format!(
                    "{}{}",
                    created("  resp-trimmed "),
                    completed(Some("resp-trimmed  "))
                )
                .as_bytes(),
            )
            .unwrap();
        assert!(normalized.finish().is_ok());

        for stream in [
            format!("{}{}", created("resp-1"), completed(None)),
            format!("{}{}", created("resp-1"), completed(Some("resp-2"))),
            format!("{}{}", completed(Some("resp-1")), completed(Some("resp-1"))),
            completed(Some(" \t ")),
        ] {
            let mut parser = BufferedResponsesParser::default();
            // Lifecycle identity is now rejected as soon as the malformed
            // event closes. A delayed EOF rejection is also acceptable, but
            // no malformed lifecycle may finish successfully.
            match parser.push(stream.as_bytes()) {
                Err(_) => {}
                Ok(()) => assert!(parser.finish().is_err()),
            }
        }

        let mut queued_mismatch = BufferedResponsesParser::default();
        queued_mismatch
            .push(
                concat!(
                    "data: {\"type\":\"response.queued\",\"response\":{\"id\":\"resp-a\"}}\n\n",
                    "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"item-a\"}}\n\n",
                    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-b\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n"
                )
                .as_bytes(),
            )
            .unwrap_err();

        for lifecycle in [
            "response.queued",
            "response.created",
            "response.in_progress",
        ] {
            let mut missing_id = BufferedResponsesParser::default();
            missing_id
                .push(
                    format!("data: {{\"type\":\"{lifecycle}\",\"response\":{{}}}}\n\n").as_bytes(),
                )
                .unwrap_err();
        }
        let mut item_before_id = BufferedResponsesParser::default();
        item_before_id
            .push(
                b"data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"item-a\"}}\n\n",
            )
            .unwrap_err();
    }

    #[test]
    fn buffered_parser_rejects_duplicate_keys_at_every_semantic_level() {
        for stream in [
            concat!(
                "data: {\"type\":\"response.created\",\"type\":\"response.queued\",\"response\":{\"id\":\"a\"}}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"a\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n"
            ),
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"a\",\"id\":\"b\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"a\",\"output\":[],\"usage\":{\"input_tokens\":1,\"input_tokens\":0,\"output_tokens\":1,\"total_tokens\":2}}}\n\n",
        ] {
            let mut parser = BufferedResponsesParser::default();
            parser.push(stream.as_bytes()).unwrap_err();
        }
    }

    #[test]
    fn buffered_parser_rejects_failure_incomplete_conflict_missing_usage_and_oversize() {
        let cases = [
            b"data: {\"type\":\"response.failed\"}\n\n".to_vec(),
            b"data: {\"type\":\"response.incomplete\"}\n\n".to_vec(),
            b"data: {\"type\":\"error\"}\n\n".to_vec(),
            concat!(
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"a\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"b\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n"
            ).as_bytes().to_vec(),
            b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"a\"}}\n\n".to_vec(),
        ];
        for stream in cases {
            let mut parser = BufferedResponsesParser::default();
            parser.push(&stream).unwrap();
            assert!(parser.finish().is_err());
        }

        let mut parser = BufferedResponsesParser::default();
        let oversized = vec![b'x'; MAX_RESPONSES_SSE_EVENT_BYTES + 1];
        assert!(parser.push(&oversized).is_err());
    }

    #[test]
    fn buffered_parser_rejects_partial_or_mismatched_completed_output() {
        let cases = [
            concat!(
                "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp\"}}\n\n",
                "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"item-0\"}}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp\",\"output\":[{\"id\":\"item-0\"},{\"id\":\"item-1\"}],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n"
            ),
            concat!(
                "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp\"}}\n\n",
                "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"captured\"}}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp\",\"output\":[{\"id\":\"different\"}],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n"
            ),
            concat!(
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"post-terminal\"}\n\n"
            ),
            concat!(
                "event: response.failed\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n"
            ),
        ];
        for stream in cases {
            let mut parser = BufferedResponsesParser::default();
            if parser.push(stream.as_bytes()).is_ok() {
                assert!(parser.finish().is_err());
            }
        }
    }

    #[test]
    fn wire_request_uses_the_fixed_path_and_controlled_headers() {
        let credential = UpstreamCredential::OAuth {
            access_token: "access-secret".to_owned(),
            refresh_token: Some("refresh-secret".to_owned()),
            expires_at: Some(i64::MAX),
            header: "authorization".to_owned(),
            prefix: "Bearer ".to_owned(),
            adapter_state: Some(json!({
                "schema": "openai-codex-oauth-v1",
                "account_id": "account-123"
            })),
            proxy_url: None,
            proxy_network_scope: None,
        };
        let request_id = Uuid::nil();
        let request = apply_reqwest_wire_headers(
            reqwest::Client::new()
                .post(format!("{BASE_URL}{RESPONSES_PATH}"))
                .body("{}"),
            &credential,
            &request_id.to_string(),
        )
        .unwrap()
        .build()
        .unwrap();
        assert_eq!(
            request.url().as_str(),
            format!("{BASE_URL}{RESPONSES_PATH}")
        );
        assert_eq!(request.headers()[header::ACCEPT], "text/event-stream");
        assert_eq!(request.headers()[header::ACCEPT_ENCODING], "identity");
        assert_eq!(request.headers()[header::CONTENT_TYPE], "application/json");
        assert!(request.headers().get(header::CONNECTION).is_none());
        assert_eq!(request.headers()[header::USER_AGENT], USER_AGENT);
        assert_eq!(request.headers()["originator"], "codex-tui");
        assert_eq!(request.headers()["session-id"], request_id.to_string());
        assert_eq!(request.headers()["chatgpt-account-id"], "account-123");
        assert_eq!(
            request.headers()[header::AUTHORIZATION],
            "Bearer access-secret"
        );
        assert!(request.headers().get("anthropic-version").is_none());
        assert!(request.headers().get("anthropic-beta").is_none());

        for (header, prefix) in [("x-api-key", "Bearer "), ("authorization", "Token ")] {
            let invalid = UpstreamCredential::OAuth {
                access_token: "access-secret".to_owned(),
                refresh_token: Some("refresh-secret".to_owned()),
                expires_at: Some(i64::MAX),
                header: header.to_owned(),
                prefix: prefix.to_owned(),
                adapter_state: Some(json!({
                    "schema": "openai-codex-oauth-v1",
                    "account_id": "account-123"
                })),
                proxy_url: None,
                proxy_network_scope: None,
            };
            assert!(
                apply_reqwest_wire_headers(
                    reqwest::Client::new().post(format!("{BASE_URL}{RESPONSES_PATH}")),
                    &invalid,
                    &request_id.to_string(),
                )
                .is_err()
            );
        }
    }

    #[test]
    fn downstream_responses_lite_hint_is_not_forwarded_upstream() {
        assert!(!PASSTHROUGH_HEADERS.contains(&"x-openai-internal-codex-responses-lite"));
    }

    #[test]
    fn include_entries_are_individually_bounded() {
        for include in [json!(["x".repeat(257)]), json!(["line\nbreak"])] {
            let mut request = json!({"model": "public", "input": [], "include": include});
            assert!(prepare_request(&mut request, "gpt-codex", &config("gpt-codex", 10)).is_err());
        }
    }

    #[tokio::test]
    async fn missing_content_type_admission_preserves_a_large_network_chunk() {
        let event = b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp\"}}\n\n";
        let repeats = MAX_RESPONSES_SSE_EVENT_BYTES / event.len() + 2;
        let body = Bytes::from(event.repeat(repeats));
        assert!(body.len() > MAX_RESPONSES_SSE_EVENT_BYTES);
        let response = UpstreamResponse::for_test(
            http::HeaderMap::new(),
            http::Version::HTTP_2,
            vec![Ok(body.clone())],
        );
        let response = admit_event_stream_response(response).await.unwrap();
        assert!(is_event_stream(&response));
        assert_eq!(response.version(), http::Version::HTTP_2);
        let mut stream = response.bytes_stream();
        let mut actual = Vec::new();
        while let Some(chunk) = stream.next().await {
            actual.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(actual.as_slice(), body.as_ref());
    }

    #[tokio::test]
    async fn response_admission_rejects_non_sse_and_ambiguous_content_types() {
        let body = Bytes::from_static(b"{\"error\":{\"message\":\"secret\"}}");
        let missing = UpstreamResponse::for_test(
            http::HeaderMap::new(),
            http::Version::HTTP_2,
            vec![Ok(body.clone())],
        );
        assert!(matches!(
            admit_event_stream_response(missing).await,
            Err(ResponseAdmissionError::Invalid(
                "upstream_invalid_content_type"
            ))
        ));

        for content_type in ["application/json", "text/html", ""] {
            let mut headers = http::HeaderMap::new();
            headers.insert(
                header::CONTENT_TYPE,
                http::HeaderValue::from_bytes(content_type.as_bytes()).unwrap(),
            );
            let response =
                UpstreamResponse::for_test(headers, http::Version::HTTP_2, vec![Ok(body.clone())]);
            assert!(matches!(
                admit_event_stream_response(response).await,
                Err(ResponseAdmissionError::Invalid(
                    "upstream_invalid_content_type"
                ))
            ));
        }

        let mut duplicate = http::HeaderMap::new();
        duplicate.append(
            header::CONTENT_TYPE,
            http::HeaderValue::from_static("text/event-stream"),
        );
        duplicate.append(
            header::CONTENT_TYPE,
            http::HeaderValue::from_static("text/event-stream"),
        );
        let response = UpstreamResponse::for_test(duplicate, http::Version::HTTP_2, vec![Ok(body)]);
        assert!(matches!(
            admit_event_stream_response(response).await,
            Err(ResponseAdmissionError::Invalid(
                "upstream_invalid_content_type"
            ))
        ));
    }

    #[test]
    fn buffered_parser_rejects_done_before_terminal_and_non_utf8_event_names() {
        let mut early_done = BufferedResponsesParser::default();
        assert!(early_done.push(b"data: [DONE]\n\n").is_err());

        let mut invalid_event_name = BufferedResponsesParser::default();
        let mut event = b"event: ".to_vec();
        event.push(0xff);
        event.extend_from_slice(
            b"\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp\"}}\n\n",
        );
        assert!(invalid_event_name.push(&event).is_err());

        let mut duplicate_event = BufferedResponsesParser::default();
        assert!(
            duplicate_event
                .push(
                    concat!(
                        "event: provider-secret\n",
                        "event: response.created\n",
                        "data: {\"type\":\"response.created\",\"response\":{}}\n\n"
                    )
                    .as_bytes()
                )
                .is_err()
        );
    }

    #[test]
    fn only_responses_protocol_is_admitted() {
        assert!(validate_protocol(Protocol::OpenAiResponses).is_ok());
        assert!(validate_protocol(Protocol::OpenAiChat).is_err());
        assert!(validate_protocol(Protocol::OpenAiEmbeddings).is_err());
    }
}
