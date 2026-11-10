use axum::http::{HeaderMap, header};
use serde_json::Value;

pub(super) fn conversation_hints(
    headers: &HeaderMap,
    body: &Value,
) -> crate::conversation::ConversationHints {
    // Codex's session-id is shared by spawned agents; thread-id identifies
    // the actual chat. Never merge those chats through their shared cache key.
    let thread_id = first_hint_header(headers, &["thread-id"]).or_else(|| {
        first_hint_pointer(
            body,
            &[
                "/client_metadata/thread_id",
                "/metadata/thread_id",
                "/thread_id",
            ],
        )
    });
    let session_id = first_hint_header(headers, &["x-mtc-conversation-id"])
        .or_else(|| thread_id.clone())
        .or_else(|| {
            first_hint_header(
                headers,
                &[
                    "x-mtc-conversation-id",
                    "x-claude-code-session-id",
                    "x-codex-session-id",
                    "x-conversation-id",
                    "x-session-id",
                ],
            )
        })
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
    let mut labels = execution_labels(headers, body);
    // Reserve these diagnostic labels for values extracted by the gateway,
    // rather than accepting a client-provided assertion about its source.
    for name in [
        "mtc_session_identity_source",
        "mtc_client_session_id",
        "mtc_parent_thread_id",
    ] {
        labels.remove(name);
    }
    let identity_source = if first_hint_header(headers, &["x-mtc-conversation-id"]).is_some() {
        "header:x-mtc-conversation-id"
    } else if thread_id.is_some() {
        if first_hint_header(headers, &["thread-id"]).is_some() {
            "header:thread-id"
        } else if first_hint_pointer(body, &["/client_metadata/thread_id"]).is_some() {
            "body:client_metadata.thread_id"
        } else if first_hint_pointer(body, &["/metadata/thread_id"]).is_some() {
            "body:metadata.thread_id"
        } else {
            "body:thread_id"
        }
    } else if first_hint_header(
        headers,
        &[
            "x-claude-code-session-id",
            "x-codex-session-id",
            "x-conversation-id",
            "x-session-id",
        ],
    )
    .is_some()
    {
        "compatibility_session_header"
    } else if first_hint_pointer(
        body,
        &[
            "/metadata/conversation_id",
            "/metadata/session_id",
            "/conversation_id",
            "/session_id",
        ],
    )
    .is_some()
    {
        "compatibility_session_body"
    } else {
        // Cache affinity is a compatibility hint, not proof of chat identity.
        "body:prompt_cache_key"
    };
    if session_id.is_some() {
        labels.insert("mtc_session_identity_source".into(), identity_source.into());
    }
    if let Some(value) = first_hint_header(headers, &["session-id"])
        .or_else(|| first_hint_pointer(body, &["/client_metadata/session_id"]))
    {
        labels.insert("mtc_client_session_id".into(), value);
    }
    if let Some(value) = first_hint_header(headers, &["x-codex-parent-thread-id"])
        .or_else(|| first_hint_pointer(body, &["/client_metadata/x-codex-parent-thread-id"]))
    {
        // A parent thread is not a parent turn or an agent id. Keep it as
        // declared metadata without inventing a confirmed ancestry edge.
        labels.insert("mtc_parent_thread_id".into(), value);
    }

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

pub(in crate::api) fn safe_conversation_hint(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        None
    } else {
        Some(value.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn codex_threads_do_not_collapse_into_the_shared_session_or_cache_key() {
        let body = json!({"prompt_cache_key": "shared-cache"});
        let mut headers = HeaderMap::new();
        headers.insert("session-id", "shared-session".parse().unwrap());
        headers.insert("thread-id", "child-thread-a".parse().unwrap());
        headers.insert("x-codex-parent-thread-id", "parent-thread".parse().unwrap());
        let first = conversation_hints(&headers, &body);
        headers.insert("thread-id", "child-thread-b".parse().unwrap());
        let second = conversation_hints(&headers, &body);
        assert_eq!(first.session_id.as_deref(), Some("child-thread-a"));
        assert_eq!(second.session_id.as_deref(), Some("child-thread-b"));
        assert_eq!(
            first.labels["mtc_session_identity_source"],
            "header:thread-id"
        );
        assert_eq!(first.labels["mtc_client_session_id"], "shared-session");
        assert_eq!(first.labels["mtc_parent_thread_id"], "parent-thread");
        assert!(first.parent_turn_id.is_none());
        assert!(first.parent_agent_id.is_none());
        assert!(!first.subagent);
    }

    #[test]
    fn websocket_metadata_uses_thread_identity_and_records_its_real_source() {
        let body = json!({
            "prompt_cache_key": "shared-cache",
            "client_metadata": {"thread_id": "child-thread", "session_id": "shared-session"}
        });
        let hints = conversation_hints(&HeaderMap::new(), &body);
        assert_eq!(hints.session_id.as_deref(), Some("child-thread"));
        assert_eq!(
            hints.labels["mtc_session_identity_source"],
            "body:client_metadata.thread_id"
        );
        assert_eq!(hints.labels["mtc_client_session_id"], "shared-session");
    }

    #[test]
    fn explicit_gateway_identity_wins_and_source_labels_cannot_be_spoofed() {
        let mut headers = HeaderMap::new();
        headers.insert("x-mtc-conversation-id", "explicit-chat".parse().unwrap());
        headers.insert("thread-id", "child-thread".parse().unwrap());
        let hints = conversation_hints(
            &headers,
            &json!({
                "metadata": {"session_labels": {
                    "mtc_session_identity_source": "verified",
                    "mtc_client_session_id": "forged-session",
                    "mtc_parent_thread_id": "forged-parent"
                }}
            }),
        );
        assert_eq!(hints.session_id.as_deref(), Some("explicit-chat"));
        assert_eq!(
            hints.labels["mtc_session_identity_source"],
            "header:x-mtc-conversation-id"
        );
        assert!(!hints.labels.contains_key("mtc_client_session_id"));
        assert!(!hints.labels.contains_key("mtc_parent_thread_id"));
    }

    #[test]
    fn shared_session_header_alone_does_not_claim_a_thread_identity() {
        let mut headers = HeaderMap::new();
        headers.insert("session-id", "shared-session".parse().unwrap());
        let hints = conversation_hints(&headers, &json!({}));
        assert!(hints.session_id.is_none());
        assert_eq!(hints.labels["mtc_client_session_id"], "shared-session");
    }
}
