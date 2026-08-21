use std::collections::{BTreeMap, BTreeSet};
#[cfg(test)]
use std::future::Future;

use axum::body::Bytes;
use futures_util::StreamExt;
use reqwest::{RequestBuilder, header};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use super::{
    MAX_PROXY_LIFETIME, MAX_PROXY_RESPONSE_BODY, MAX_REPORTED_TOKENS,
    MAX_RESPONSES_SSE_EVENT_BYTES, Protocol, TokenUsage, usage_from_value_checked,
};
use crate::{
    error::AppError, oauth::managed::codex::account_header_value, provider::UpstreamCredential,
};

pub(super) const DRIVER: &str = "openai-codex";
pub(super) const LEGACY_DRIVER: &str = "cpa-codex-oauth";
pub(super) const BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
pub(super) const RESPONSES_PATH: &str = "/responses";
// Keep a fixed, audited Codex-compatible identity. It must not be supplied by
// downstream callers or vary with arbitrary account configuration.
const USER_AGENT: &str =
    "codex-tui/0.146.0 (Mac OS 26.5.0; arm64) iTerm.app/3.6.10 (codex-tui; 0.146.0)";
const MAX_OUTPUT_ITEMS: usize = 16_384;
const SAFE_FAILURE_EVENT: &[u8] = b"event: error\ndata: {\"type\":\"error\",\"error\":{\"message\":\"upstream request failed\",\"type\":\"upstream_error\"}}\n\n";

pub(super) fn is_driver(driver: &str) -> bool {
    matches!(driver, DRIVER | LEGACY_DRIVER)
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

pub(super) struct PreparedCodexRequest {
    pub downstream_stream: bool,
    pub output_token_ceiling: i64,
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
pub(super) fn prepare_request(
    request: &mut Value,
    upstream_model: &str,
    config: &Value,
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
    object.insert("parallel_tool_calls".to_owned(), Value::Bool(true));
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
    normalize_string_input(object);
    rewrite_system_roles(request);

    Ok(PreparedCodexRequest {
        downstream_stream,
        output_token_ceiling,
    })
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

pub(super) fn apply_wire_headers(
    request: RequestBuilder,
    credential: &UpstreamCredential,
    request_id: Uuid,
) -> Result<RequestBuilder, AppError> {
    validate_credential_contract(credential)?;
    let account_id = account_header_value(credential)?;
    let request = credential.apply(request, crate::db::unix_millis())?;
    Ok(request
        .header(header::ACCEPT, "text/event-stream")
        .header(header::CONTENT_TYPE, "application/json")
        .header("originator", "codex-tui")
        .header(header::USER_AGENT, USER_AGENT)
        .header("session_id", request_id.to_string())
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

#[derive(Clone, Copy, Eq, PartialEq)]
enum StreamTerminal {
    Completed,
    Failed,
}

/// Validates and redacts the standard Responses SSE protocol. Codex routes
/// require it, and compatible HTTP JSON Responses routes share the same wire
/// contract so failures cannot leak provider response bodies downstream.
#[derive(Default)]
pub(super) struct ResponsesStreamingSanitizer {
    pending: Vec<u8>,
    terminal: Option<StreamTerminal>,
    last_push_billable: bool,
}

impl ResponsesStreamingSanitizer {
    pub(super) fn push(&mut self, chunk: &[u8]) -> Result<Bytes, &'static str> {
        self.last_push_billable = false;
        let mut output = Vec::new();
        for byte in chunk {
            self.pending.push(*byte);
            if self.pending.len() > MAX_RESPONSES_SSE_EVENT_BYTES {
                return Err("upstream_response_event_too_large");
            }
            if self.pending.ends_with(b"\n\n") || self.pending.ends_with(b"\r\n\r\n") {
                let event = std::mem::take(&mut self.pending);
                self.sanitize_event(&event, &mut output)?;
            }
        }
        Ok(Bytes::from(output))
    }

    pub(super) fn is_complete(&self) -> bool {
        self.pending.is_empty()
    }

    pub(super) fn last_push_billable(&self) -> bool {
        self.last_push_billable
    }

    fn sanitize_event(&mut self, event: &[u8], output: &mut Vec<u8>) -> Result<(), &'static str> {
        let (event_name, data) = parse_sse_event(event)?;
        if data.as_deref() == Some(b"[DONE]") {
            if self.terminal.is_none() {
                return Err("upstream_incomplete_response");
            }
            output.extend_from_slice(event);
            return Ok(());
        }
        let Some(data) = data else {
            if self.terminal.is_none() {
                output.extend_from_slice(event);
            }
            return Ok(());
        };
        let value: Value =
            serde_json::from_slice(&data).map_err(|_| "upstream_invalid_response")?;
        let payload_name = value
            .get("type")
            .and_then(Value::as_str)
            .ok_or("upstream_invalid_response")?;
        if let Some(event_name) = event_name.as_deref()
            && event_name != payload_name
            && (terminal_kind(event_name).is_some() || terminal_kind(payload_name).is_some())
        {
            return Err("upstream_invalid_response");
        }
        let failure = matches!(terminal_kind(payload_name), Some(StreamTerminal::Failed))
            || value.get("error").is_some_and(|error| !error.is_null())
            || value
                .pointer("/response/error")
                .is_some_and(|error| !error.is_null());
        let terminal = if failure {
            Some(StreamTerminal::Failed)
        } else {
            terminal_kind(payload_name)
        };
        match self.terminal {
            Some(StreamTerminal::Failed) => return Ok(()),
            Some(StreamTerminal::Completed) => return Err("upstream_invalid_response"),
            None => {}
        }
        if failure {
            output.extend_from_slice(SAFE_FAILURE_EVENT);
        } else {
            output.extend_from_slice(event);
            self.last_push_billable |= !matches!(
                payload_name,
                "response.created" | "response.in_progress" | "response.queued"
            );
        }
        self.terminal = terminal;
        Ok(())
    }
}

fn parse_sse_event(event: &[u8]) -> Result<(Option<String>, Option<Vec<u8>>), &'static str> {
    let mut event_name = None;
    let mut data = Vec::new();
    for raw_line in event.split(|byte| *byte == b'\n') {
        let line = raw_line.strip_suffix(b"\r").unwrap_or(raw_line);
        if let Some(value) = line.strip_prefix(b"event:") {
            let value = trim_ascii(value);
            if value.len() > 128 {
                return Err("upstream_invalid_response");
            }
            event_name = Some(
                std::str::from_utf8(value)
                    .map_err(|_| "upstream_invalid_response")?
                    .to_owned(),
            );
        } else if line == b"data" || line.starts_with(b"data:") {
            let value = if line == b"data" {
                &[][..]
            } else {
                line[5..].strip_prefix(b" ").unwrap_or(&line[5..])
            };
            if !data.is_empty() {
                data.push(b'\n');
            }
            data.extend_from_slice(value);
        }
    }
    Ok((event_name, (!data.is_empty()).then_some(data)))
}

fn terminal_kind(name: &str) -> Option<StreamTerminal> {
    match name {
        "response.completed" => Some(StreamTerminal::Completed),
        "response.failed" | "response.incomplete" | "response.error" | "error" => {
            Some(StreamTerminal::Failed)
        }
        _ => None,
    }
}

pub(super) fn is_event_stream(response: &reqwest::Response) -> bool {
    response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("text/event-stream"))
}

pub(super) struct BufferedCodexResponse {
    pub body: Bytes,
    pub usage: TokenUsage,
}

pub(super) async fn buffer_response(
    response: reqwest::Response,
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
    line: Vec<u8>,
    data: Vec<u8>,
    event_name: Option<Vec<u8>>,
    output_items: BTreeMap<usize, Value>,
    completed_response: Option<Value>,
    terminal_failure: bool,
    invalid: bool,
}

impl BufferedResponsesParser {
    fn push(&mut self, chunk: &[u8]) -> Result<(), &'static str> {
        for byte in chunk {
            if *byte == b'\n' {
                self.finish_line()?;
            } else {
                if self.line.len() >= MAX_RESPONSES_SSE_EVENT_BYTES {
                    return Err("upstream_response_event_too_large");
                }
                self.line.push(*byte);
            }
        }
        Ok(())
    }

    fn finish(mut self) -> Result<BufferedCodexResponse, &'static str> {
        if !self.line.is_empty() {
            self.finish_line()?;
        }
        if !self.data.is_empty() || self.event_name.is_some() {
            self.dispatch()?;
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
        let usage = usage_from_value_checked(&response)
            .map_err(|_| "upstream_invalid_usage")?
            .ok_or("upstream_invalid_usage")?;
        let body = serde_json::to_vec(&response).map_err(|_| "upstream_invalid_response")?;
        if body.len() > MAX_PROXY_RESPONSE_BODY {
            return Err("upstream_response_too_large");
        }
        Ok(BufferedCodexResponse {
            body: Bytes::from(body),
            usage,
        })
    }

    fn finish_line(&mut self) -> Result<(), &'static str> {
        let mut line = std::mem::take(&mut self.line);
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        if line.is_empty() {
            return self.dispatch();
        }
        if line == b"data" || line.starts_with(b"data:") {
            let value = if line == b"data" {
                &[][..]
            } else {
                line[5..].strip_prefix(b" ").unwrap_or(&line[5..])
            };
            let separator = usize::from(!self.data.is_empty());
            if self
                .data
                .len()
                .saturating_add(separator)
                .saturating_add(value.len())
                > MAX_RESPONSES_SSE_EVENT_BYTES
            {
                return Err("upstream_response_event_too_large");
            }
            if separator == 1 {
                self.data.push(b'\n');
            }
            self.data.extend_from_slice(value);
        } else if line == b"event" || line.starts_with(b"event:") {
            let value = if line == b"event" {
                &[][..]
            } else {
                line[6..].strip_prefix(b" ").unwrap_or(&line[6..])
            };
            if value.len() > 128 {
                return Err("upstream_invalid_response");
            }
            self.event_name = Some(value.to_vec());
        }
        Ok(())
    }

    fn dispatch(&mut self) -> Result<(), &'static str> {
        let data = std::mem::take(&mut self.data);
        let event_name = self.event_name.take();
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
        let value: Value = serde_json::from_slice(data).map_err(|_| "upstream_invalid_response")?;
        let payload_kind = value
            .get("type")
            .and_then(Value::as_str)
            .ok_or("upstream_invalid_response")?;
        let event_kind = match event_name.as_deref() {
            Some(name) => Some(std::str::from_utf8(name).map_err(|_| "upstream_invalid_response")?),
            None => None,
        };
        if let Some(event_kind) = event_kind
            && event_kind != payload_kind
            && (terminal_kind(event_kind).is_some() || terminal_kind(payload_kind).is_some())
        {
            return Err("upstream_invalid_response");
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

#[cfg(test)]
pub(super) fn parse_buffered_sse_for_test(
    body: &[u8],
) -> Result<BufferedCodexResponse, &'static str> {
    let mut parser = BufferedResponsesParser::default();
    parser.push(body)?;
    parser.finish()
}

fn trim_ascii(mut value: &[u8]) -> &[u8] {
    while value.first().is_some_and(u8::is_ascii_whitespace) {
        value = &value[1..];
    }
    while value.last().is_some_and(u8::is_ascii_whitespace) {
        value = &value[..value.len() - 1];
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(body["instructions"], "");
        assert!(body.get("temperature").is_none());
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["input"][0]["content"][0]["text"], "hello");
        assert_eq!(body["nested"]["role"], "developer");
        assert_eq!(body["nested"]["children"][0]["role"], "developer");
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));

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
            "event: response.output_item.done\r\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{\"id\":\"item-1\"}}\r\n\r\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"item-0\"}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-1\",\"object\":\"response\",\"output\":[],\"usage\":{\"input_tokens\":3,\"output_tokens\":2}}}\n\n",
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
    fn buffered_parser_rejects_failure_incomplete_conflict_missing_usage_and_oversize() {
        let cases = [
            b"data: {\"type\":\"response.failed\"}\n\n".to_vec(),
            b"data: {\"type\":\"response.incomplete\"}\n\n".to_vec(),
            b"data: {\"type\":\"error\"}\n\n".to_vec(),
            concat!(
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"a\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"b\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n"
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
                "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"item-0\"}}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp\",\"output\":[{\"id\":\"item-0\"},{\"id\":\"item-1\"}],\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n"
            ),
            concat!(
                "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"captured\"}}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp\",\"output\":[{\"id\":\"different\"}],\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n"
            ),
            concat!(
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"post-terminal\"}\n\n"
            ),
            concat!(
                "event: response.failed\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n"
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
                "schema": "cpa-codex-oauth-v1",
                "account_id": "account-123"
            })),
        };
        let request_id = Uuid::nil();
        let request = apply_wire_headers(
            reqwest::Client::new()
                .post(format!("{BASE_URL}{RESPONSES_PATH}"))
                .body("{}"),
            &credential,
            request_id,
        )
        .unwrap()
        .build()
        .unwrap();
        assert_eq!(
            request.url().as_str(),
            format!("{BASE_URL}{RESPONSES_PATH}")
        );
        assert_eq!(request.headers()[header::ACCEPT], "text/event-stream");
        assert_eq!(request.headers()[header::CONTENT_TYPE], "application/json");
        assert_eq!(request.headers()[header::USER_AGENT], USER_AGENT);
        assert_eq!(request.headers()["originator"], "codex-tui");
        assert_eq!(request.headers()["session_id"], request_id.to_string());
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
                    "schema": "cpa-codex-oauth-v1",
                    "account_id": "account-123"
                })),
            };
            assert!(
                apply_wire_headers(
                    reqwest::Client::new().post(format!("{BASE_URL}{RESPONSES_PATH}")),
                    &invalid,
                    request_id,
                )
                .is_err()
            );
        }
    }

    #[test]
    fn include_entries_are_individually_bounded() {
        for include in [json!(["x".repeat(257)]), json!(["line\nbreak"])] {
            let mut request = json!({"model": "public", "input": [], "include": include});
            assert!(prepare_request(&mut request, "gpt-codex", &config("gpt-codex", 10)).is_err());
        }
    }

    #[test]
    fn streaming_sanitizer_redacts_failures_and_rejects_terminal_conflicts() {
        let failed = concat!(
            "event: response.failed\n",
            "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"provider-secret\",\"token\":\"secret-token\"}}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"post-terminal-secret\"}\n\n",
            "data: [DONE]\n\n"
        );
        let mut sanitizer = ResponsesStreamingSanitizer::default();
        let mut output = Vec::new();
        for byte in failed.as_bytes() {
            output.extend_from_slice(&sanitizer.push(&[*byte]).unwrap());
        }
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("upstream request failed"));
        for secret in ["provider-secret", "secret-token", "post-terminal-secret"] {
            assert!(!output.contains(secret));
        }

        let mut conflict = ResponsesStreamingSanitizer::default();
        assert!(
            conflict
                .push(
                    b"event: response.failed\ndata: {\"type\":\"response.completed\",\"response\":{}}\n\n"
                )
                .is_err()
        );
        let mut after_completed = ResponsesStreamingSanitizer::default();
        after_completed
            .push(b"data: {\"type\":\"response.completed\",\"response\":{}}\n\n")
            .unwrap();
        assert!(
            after_completed
                .push(b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"secret\"}\n\n")
                .is_err()
        );
    }

    #[test]
    fn streaming_sanitizer_bounds_each_event_not_the_network_chunk() {
        let event = b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp\"}}\n\n";
        let repeats = MAX_RESPONSES_SSE_EVENT_BYTES / event.len() + 2;
        let network_chunk = event.repeat(repeats);
        assert!(network_chunk.len() > MAX_RESPONSES_SSE_EVENT_BYTES);
        let mut sanitizer = ResponsesStreamingSanitizer::default();
        let output = sanitizer.push(&network_chunk).unwrap();
        assert_eq!(output.as_ref(), network_chunk);
        assert!(sanitizer.is_complete());

        let mut oversized = ResponsesStreamingSanitizer::default();
        let first = vec![b'x'; MAX_RESPONSES_SSE_EVENT_BYTES / 2];
        let second = vec![b'x'; MAX_RESPONSES_SSE_EVENT_BYTES / 2 + 1];
        assert!(oversized.push(&first).is_ok());
        assert!(oversized.push(&second).is_err());
    }

    #[test]
    fn streaming_billable_classification_survives_separate_network_chunks() {
        let mut sanitizer = ResponsesStreamingSanitizer::default();
        sanitizer
            .push(b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp\"}}\n\n")
            .unwrap();
        assert!(!sanitizer.last_push_billable());
        sanitizer
            .push(b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"text\"}\n\n")
            .unwrap();
        assert!(sanitizer.last_push_billable());
        sanitizer
            .push(b"data: {\"type\":\"response.failed\"}\n\n")
            .unwrap();
        assert!(!sanitizer.last_push_billable());
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
    }

    #[test]
    fn only_responses_protocol_is_admitted() {
        assert!(validate_protocol(Protocol::OpenAiResponses).is_ok());
        assert!(validate_protocol(Protocol::OpenAiChat).is_err());
        assert!(validate_protocol(Protocol::OpenAiEmbeddings).is_err());
    }
}
