use axum::http::{HeaderMap, header};
use serde_json::Value;

pub(super) fn conversation_hints(
    headers: &HeaderMap,
    body: &Value,
) -> crate::conversation::ConversationHints {
    let session_id = first_hint_header(
        headers,
        &[
            "x-mtc-conversation-id",
            "x-claude-code-session-id",
            "x-codex-session-id",
            "x-conversation-id",
            "x-session-id",
        ],
    )
    .or_else(|| {
        first_hint_pointer(
            body,
            &[
                "/metadata/conversation_id",
                "/metadata/session_id",
                "/metadata/thread_id",
                "/conversation_id",
                "/session_id",
                "/thread_id",
                "/prompt_cache_key",
            ],
        )
    });
    let turn_id = first_hint_header(headers, &["x-mtc-turn-id"])
        .or_else(|| first_hint_pointer(body, &["/metadata/turn_id", "/metadata/message_id"]));
    let parent_turn_id = first_hint_header(headers, &["x-mtc-parent-turn-id"]).or_else(|| {
        first_hint_pointer(
            body,
            &[
                "/metadata/parent_turn_id",
                "/metadata/previous_response_id",
                "/previous_response_id",
            ],
        )
    });
    let branch_id = first_hint_header(headers, &["x-mtc-branch-id"])
        .or_else(|| first_hint_pointer(body, &["/metadata/branch_id", "/branch_id"]));
    let compaction = headers
        .get("x-mtc-compaction")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| matches!(value.trim(), "1" | "true" | "yes"))
        || body
            .pointer("/metadata/compaction")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        || body
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|value| value == "compaction");
    // Deliberately narrower than the other compatibility hints: subagent
    // ancestry is security-sensitive and must never be inferred from UA,
    // originator, client name, branch ids, or semantic similarity.
    let explicit_subagent_marker = headers
        .get("x-mtc-subagent")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.trim() == "true")
        || body
            .pointer("/metadata/subagent")
            .and_then(Value::as_bool)
            .unwrap_or(false);
    let subagent = explicit_subagent_marker && parent_turn_id.is_some();
    let session_name = first_hint_header(headers, &["x-mtc-session-name"])
        .or_else(|| first_hint_pointer(body, &["/metadata/session_name"]));
    let traceparent = headers
        .get("traceparent")
        .and_then(|value| value.to_str().ok())
        .and_then(parse_traceparent);
    let trace_id = first_hint_header(headers, &["x-mtc-trace-id"])
        .or_else(|| first_hint_pointer(body, &["/metadata/trace_id"]))
        .and_then(|value| safe_trace_identifier(&value, 32))
        .or_else(|| traceparent.as_ref().map(|(trace_id, _)| trace_id.clone()));
    let span_id = first_hint_header(headers, &["x-mtc-span-id"])
        .or_else(|| first_hint_pointer(body, &["/metadata/span_id"]))
        .and_then(|value| safe_trace_identifier(&value, 16));
    let parent_span_id = first_hint_header(headers, &["x-mtc-parent-span-id"])
        .or_else(|| first_hint_pointer(body, &["/metadata/parent_span_id"]))
        .and_then(|value| safe_trace_identifier(&value, 16))
        // In an inbound W3C trace context, traceparent's span id identifies
        // the caller/parent span. The service does not invent a child span id.
        .or_else(|| traceparent.as_ref().map(|(_, span_id)| span_id.clone()));
    let agent_id = first_hint_header(headers, &["x-mtc-agent-id"])
        .or_else(|| first_hint_pointer(body, &["/metadata/agent_id"]));
    let parent_agent_id = first_hint_header(headers, &["x-mtc-parent-agent-id"])
        .or_else(|| first_hint_pointer(body, &["/metadata/parent_agent_id"]));
    let task_kind = first_hint_header(headers, &["x-mtc-task-kind"])
        .or_else(|| first_hint_pointer(body, &["/metadata/task_kind"]));
    let labels = execution_labels(headers, body);

    crate::conversation::ConversationHints {
        session_id,
        turn_id,
        parent_turn_id,
        branch_id,
        compaction,
        subagent,
        session_name,
        trace_id,
        span_id,
        parent_span_id,
        agent_id,
        parent_agent_id,
        task_kind,
        labels,
    }
}

fn parse_traceparent(value: &str) -> Option<(String, String)> {
    let mut parts = value.trim().split('-');
    let version = parts.next()?;
    let trace_id = parts.next()?;
    let parent_span_id = parts.next()?;
    let flags = parts.next()?;
    if parts.next().is_some()
        || version != "00"
        || trace_id.len() != 32
        || parent_span_id.len() != 16
        || flags.len() != 2
        || ![version, trace_id, parent_span_id, flags]
            .into_iter()
            .all(|part| part.bytes().all(|byte| byte.is_ascii_hexdigit()))
        || trace_id.bytes().all(|byte| byte == b'0')
        || parent_span_id.bytes().all(|byte| byte == b'0')
    {
        return None;
    }
    Some((
        trace_id.to_ascii_lowercase(),
        parent_span_id.to_ascii_lowercase(),
    ))
}

fn safe_trace_identifier(value: &str, expected_length: usize) -> Option<String> {
    let value = value.trim();
    (value.len() == expected_length
        && value.bytes().all(|byte| byte.is_ascii_hexdigit())
        && !value.bytes().all(|byte| byte == b'0'))
    .then(|| value.to_ascii_lowercase())
}

fn execution_labels(
    headers: &HeaderMap,
    body: &Value,
) -> std::collections::BTreeMap<String, String> {
    const MAX_LABELS: usize = 16;
    const MAX_HEADER_BYTES: usize = 2_048;
    let header_labels = headers
        .get("x-mtc-session-labels")
        .and_then(|value| value.to_str().ok())
        .filter(|value| value.len() <= MAX_HEADER_BYTES)
        .and_then(|value| serde_json::from_str::<serde_json::Map<String, Value>>(value).ok());
    let labels = header_labels.as_ref().or_else(|| {
        body.pointer("/metadata/session_labels")
            .and_then(Value::as_object)
    });
    labels
        .into_iter()
        .flat_map(|labels| labels.iter())
        .filter_map(|(key, value)| {
            let key = safe_execution_label_key(key)?;
            let value = value.as_str().and_then(safe_execution_label_value)?;
            Some((key, value))
        })
        .take(MAX_LABELS)
        .collect()
}

fn safe_execution_label_key(value: &str) -> Option<String> {
    let value = value.trim();
    let normalized = value.to_ascii_lowercase().replace('-', "_");
    let secret_like = [
        "authorization",
        "bearer",
        "cookie",
        "credential",
        "password",
        "private",
        "secret",
        "token",
        "api_key",
    ]
    .iter()
    .any(|needle| normalized.contains(needle));
    if value.is_empty()
        || value.len() > 64
        || secret_like
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        None
    } else {
        Some(value.to_owned())
    }
}

fn safe_execution_label_value(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value.len() > 128 || value.chars().any(char::is_control) {
        None
    } else {
        Some(value.to_owned())
    }
}

pub(super) fn client_name(headers: &HeaderMap) -> Option<String> {
    first_hint_header(headers, &["x-mtc-client-name", header::USER_AGENT.as_str()])
}

fn first_hint_header(headers: &HeaderMap, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        headers
            .get(*name)
            .and_then(|value| value.to_str().ok())
            .and_then(safe_conversation_hint)
    })
}

fn first_hint_pointer(body: &Value, pointers: &[&str]) -> Option<String> {
    pointers.iter().find_map(|pointer| {
        body.pointer(pointer)
            .and_then(Value::as_str)
            .and_then(safe_conversation_hint)
    })
}

pub(super) fn safe_conversation_hint(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        None
    } else {
        Some(value.to_owned())
    }
}
