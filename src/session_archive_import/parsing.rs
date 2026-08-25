use super::*;

pub(super) fn parse_record(
    line: &[u8],
) -> Result<Option<(ArchiveRecord, String)>, Box<dyn std::error::Error + Send + Sync>> {
    if line.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    let record: ArchiveRecord = serde_json::from_slice(line)?;
    if !matches!(record.schema_version, 1 | 2) {
        return Err(format!(
            "unsupported cpa-session-archive schema {}",
            record.schema_version
        )
        .into());
    }
    if record.request_id.is_empty()
        || record.request_id.len() > 512
        || record.request_id.chars().any(char::is_control)
        || record.session_id.len() > 1024
        || record.session_id.chars().any(char::is_control)
        || record.completed_at < record.started_at
    {
        return Err("archive record contains an invalid identity or time range".into());
    }
    let mut canonical_hasher = blake3::Hasher::new();
    serde_json::to_writer(&mut canonical_hasher, &record)?;
    let record_digest = canonical_hasher.finalize().to_hex().to_string();
    Ok(Some((record, record_digest)))
}

pub(super) fn archive_record_inside_overlap(record: &ArchiveRecord, lower_bound: i64) -> bool {
    let started_at = record.started_at.timestamp_millis();
    let completed_at = record.completed_at.timestamp_millis();
    // This exactly mirrors the source delta cursor: long-running or late-completed
    // records remain eligible when either endpoint is inside the overlap window.
    started_at >= lower_bound || completed_at >= lower_bound
}

pub(super) fn archived_credential_hash(record: &ArchiveRecord) -> Result<Option<String>, AppError> {
    let candidate = match record.schema_version {
        // Schema 1 defines key_id itself as the legacy bare digest.  Do not
        // broaden that older envelope by interpreting labels or prefixes.
        1 => nonempty(&record.key_id),
        2 => match nonempty(&record.credential_hash) {
            // An explicit schema-2 field is authoritative: malformed data must
            // not fall back to key_id.  Only the specified sha256 prefix is
            // stripped, and the digest is canonicalized for proof stability.
            Some(value) => {
                return normalize_schema_v2_sha256(value).map(Some).ok_or_else(|| {
                    AppError::BadRequest("archive credential hash is malformed".into())
                });
            }
            None => nonempty(&record.key_id),
        },
        _ => None,
    };
    candidate
        .map(|value| {
            normalize_bare_sha256(value)
                .ok_or_else(|| AppError::BadRequest("archive credential hash is malformed".into()))
        })
        .transpose()
}

pub(super) fn normalize_bare_sha256(value: &str) -> Option<String> {
    (value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| value.to_ascii_lowercase())
}

pub(super) fn normalize_schema_v2_sha256(value: &str) -> Option<String> {
    normalize_bare_sha256(value.strip_prefix("sha256:").unwrap_or(value))
}

pub(super) fn payload_bytes(value: &Value) -> Result<Option<Vec<u8>>, serde_json::Error> {
    match value {
        Value::Null => Ok(None),
        Value::String(value) => Ok(Some(value.as_bytes().to_vec())),
        value => serde_json::to_vec(value).map(Some),
    }
}

pub(super) fn structured_request(value: &Value) -> Option<&Value> {
    match value {
        Value::Array(_) | Value::Object(_) => Some(value),
        _ => None,
    }
}

pub(super) fn conversation_hints(record: &ArchiveRecord) -> ConversationHints {
    let turn_id = first_facet(record, "turn.id").map(str::to_owned);
    let parent_turn_id = first_facet(record, "parent.turn.id")
        .or_else(|| metadata_string(record, "parent_response_id"))
        .map(str::to_owned);
    let branch_id = first_facet(record, "branch.id").map(str::to_owned);
    let compaction = record
        .facets
        .get("request.kind")
        .is_some_and(|values| values.iter().any(|value| value.contains("compaction")));
    // Archive envelopes have no trusted transport header. Accept only the
    // typed metadata marker, and only when the archive also names a parent.
    let subagent = parent_turn_id.is_some()
        && record
            .metadata
            .get("subagent")
            .and_then(Value::as_bool)
            .unwrap_or(false);
    ConversationHints {
        session_id: nonempty(&record.session_id).map(str::to_owned),
        turn_id,
        parent_turn_id,
        branch_id,
        compaction,
        subagent,
        session_name: metadata_string(record, "session_name").and_then(safe_execution_metadata),
        trace_id: metadata_string(record, "trace_id").and_then(safe_execution_metadata),
        span_id: metadata_string(record, "span_id").and_then(safe_execution_metadata),
        parent_span_id: metadata_string(record, "parent_span_id").and_then(safe_execution_metadata),
        agent_id: metadata_string(record, "agent_id").and_then(safe_execution_metadata),
        parent_agent_id: metadata_string(record, "parent_agent_id")
            .and_then(safe_execution_metadata),
        task_kind: metadata_string(record, "task_kind").and_then(safe_execution_metadata),
        labels: archive_execution_labels(record),
    }
}

fn safe_execution_metadata(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        None
    } else {
        Some(value.to_owned())
    }
}

fn archive_execution_labels(record: &ArchiveRecord) -> std::collections::BTreeMap<String, String> {
    record
        .metadata
        .get("session_labels")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|labels| labels.iter())
        .filter_map(|(key, value)| {
            let normalized = key.to_ascii_lowercase().replace('-', "_");
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
            if key.is_empty()
                || key.len() > 64
                || secret_like
                || !key
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
            {
                return None;
            }
            let value = value.as_str()?.trim();
            if value.is_empty() || value.len() > 128 || value.chars().any(char::is_control) {
                return None;
            }
            Some((key.clone(), value.to_owned()))
        })
        .take(16)
        .collect()
}

pub(super) fn first_facet<'a>(record: &'a ArchiveRecord, name: &str) -> Option<&'a str> {
    record
        .facets
        .get(name)
        .and_then(|values| values.iter().find_map(|value| nonempty(value)))
}

pub(super) fn metadata_string<'a>(record: &'a ArchiveRecord, name: &str) -> Option<&'a str> {
    record
        .metadata
        .get(name)
        .and_then(Value::as_str)
        .and_then(nonempty)
}

pub(super) fn archive_protocol(record: &ArchiveRecord) -> String {
    nonempty(&record.request_path)
        .or_else(|| first_facet(record, "request.path"))
        .or_else(|| metadata_string(record, "request_path"))
        .or_else(|| first_facet(record, "client"))
        .unwrap_or("session-archive")
        .to_owned()
}

pub(super) fn archive_usage(record: &ArchiveRecord) -> (i64, i64) {
    let usage = record
        .response
        .pointer("/response/usage")
        .or_else(|| record.response.pointer("/usage"));
    let Some(usage) = usage else {
        return (0, 0);
    };
    let Some(input_tokens) = usage.get("input_tokens").and_then(Value::as_i64) else {
        return (0, 0);
    };
    let Some(output_tokens) = usage.get("output_tokens").and_then(Value::as_i64) else {
        return (0, 0);
    };
    if input_tokens < 0 || output_tokens < 0 {
        (0, 0)
    } else {
        (input_tokens, output_tokens)
    }
}

pub(super) fn archive_error_code(record: &ArchiveRecord) -> Option<String> {
    let outcome = record.outcome.trim();
    let successful =
        outcome.eq_ignore_ascii_case("success") || outcome.eq_ignore_ascii_case("succeeded");
    if successful && record.status_code < 400 {
        return None;
    }
    if !outcome.is_empty() && !successful {
        let normalized: String = outcome
            .chars()
            .take(200)
            .map(|character| {
                if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | ':' | '-') {
                    character.to_ascii_lowercase()
                } else {
                    '_'
                }
            })
            .collect();
        return nonempty(&normalized).map(str::to_owned);
    }
    (record.status_code >= 400).then(|| format!("http_{}", record.status_code))
}

pub(super) fn nonempty(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

pub(super) fn digest(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

pub(super) fn validate_name(value: &str, label: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 200
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'))
    {
        return Err(format!("{label} contains unsupported characters"));
    }
    Ok(())
}

pub(super) async fn read_bounded_line(
    reader: &mut BufReader<tokio::fs::File>,
    maximum: usize,
) -> Result<Option<Vec<u8>>, Box<dyn std::error::Error + Send + Sync>> {
    if maximum > MAX_SESSION_ARCHIVE_LINE_BYTES {
        return Err(format!(
            "max line bytes exceeds the compiled-in {} MiB hard limit",
            MAX_SESSION_ARCHIVE_LINE_BYTES / (1024 * 1024)
        )
        .into());
    }
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok((!line.is_empty()).then_some(line));
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |position| position + 1);
        if line.len().saturating_add(take) > maximum {
            return Err(format!("archive JSONL record exceeds {maximum} bytes").into());
        }
        line.extend_from_slice(&available[..take]);
        reader.consume(take);
        if line.last() == Some(&b'\n') {
            return Ok(Some(line));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record(metadata: Value, facets: Value) -> ArchiveRecord {
        serde_json::from_value(json!({
            "schema_version": 2,
            "session_id": "archive-session",
            "request_id": "archive-request",
            "started_at": "2026-08-22T00:00:00Z",
            "completed_at": "2026-08-22T00:00:01Z",
            "metadata": metadata,
            "facets": facets
        }))
        .expect("archive record")
    }

    #[test]
    fn archive_subagent_marker_requires_a_typed_marker_and_explicit_parent() {
        let linked = conversation_hints(&record(
            json!({"subagent": true, "parent_response_id": "parent-response"}),
            json!({}),
        ));
        assert!(linked.subagent);
        assert_eq!(linked.parent_turn_id.as_deref(), Some("parent-response"));

        for candidate in [
            record(json!({"subagent": true}), json!({})),
            record(
                json!({"subagent": "true", "parent_response_id": "parent"}),
                json!({}),
            ),
            record(
                json!({"client_name": "subagent"}),
                json!({"parent.turn.id": ["parent"]}),
            ),
        ] {
            assert!(!conversation_hints(&candidate).subagent);
        }
    }

    #[test]
    fn archive_execution_metadata_keeps_only_bounded_non_secret_declarations() {
        let hints = conversation_hints(&record(
            json!({
                "session_name": "nightly fulfilment",
                "trace_id": "trace-7",
                "agent_id": "worker-2",
                "parent_agent_id": "scheduler",
                "task_kind": "background",
                "session_labels": {
                    "workflow": "fulfilment",
                    "environment": "production",
                    "access-token": "must-drop",
                    "numeric": 7
                }
            }),
            json!({}),
        ));
        assert_eq!(hints.session_name.as_deref(), Some("nightly fulfilment"));
        assert_eq!(hints.trace_id.as_deref(), Some("trace-7"));
        assert_eq!(hints.agent_id.as_deref(), Some("worker-2"));
        assert_eq!(hints.parent_agent_id.as_deref(), Some("scheduler"));
        assert_eq!(hints.task_kind.as_deref(), Some("background"));
        assert_eq!(hints.labels.len(), 2);
        assert!(!hints.labels.contains_key("access-token"));
        assert!(!hints.labels.contains_key("numeric"));
    }
}
