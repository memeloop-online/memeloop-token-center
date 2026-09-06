use super::conversation_hints::safe_conversation_hint;
use super::*;

/// Result of bounded usage extraction from a buffered JSON or SSE body.
pub(super) enum ExtractedUsage {
    Missing,
    Valid(TokenUsage),
    Invalid,
}

pub(super) fn merge_streaming_usage(current: &mut TokenUsage, next: TokenUsage) -> Result<(), ()> {
    current.input_tokens = current.input_tokens.max(next.input_tokens);
    current.cached_input_tokens = current.cached_input_tokens.max(next.cached_input_tokens);
    current.cache_write_tokens = current.cache_write_tokens.max(next.cache_write_tokens);
    current.output_tokens = current.output_tokens.max(next.output_tokens);
    if let Some(next_tier) = next.service_tier {
        match current.service_tier.as_deref() {
            None => current.service_tier = Some(next_tier),
            Some(current_tier) if current_tier == next_tier => {}
            Some(_) => return Err(()),
        }
    }
    Ok(())
}

pub(super) fn extract_usage_checked(body: &[u8]) -> ExtractedUsage {
    if let Ok(value) = serde_json::from_slice::<Value>(body) {
        return match usage_from_value_checked(&value) {
            Ok(Some(usage)) => ExtractedUsage::Valid(usage),
            Ok(None) => ExtractedUsage::Missing,
            Err(()) => ExtractedUsage::Invalid,
        };
    }
    let mut result: Option<TokenUsage> = None;
    for line in body.split(|byte| *byte == b'\n') {
        let Some(line) = line
            .strip_prefix(b"data: ")
            .or_else(|| line.strip_prefix(b"data:"))
        else {
            continue;
        };
        let line = trim_ascii_whitespace(line);
        if line.is_empty() || line == b"[DONE]" {
            continue;
        }
        let Ok(value) = serde_json::from_slice::<Value>(line) else {
            return ExtractedUsage::Invalid;
        };
        match usage_from_value_checked(&value) {
            Err(()) => return ExtractedUsage::Invalid,
            Ok(None) => continue,
            Ok(Some(next)) => {
                let current = result.get_or_insert_with(TokenUsage::default);
                if merge_streaming_usage(current, next).is_err() {
                    return ExtractedUsage::Invalid;
                }
            }
        }
    }
    match result {
        Some(usage) => ExtractedUsage::Valid(usage),
        None => ExtractedUsage::Missing,
    }
}

/// A completed Responses event sometimes carries the entire output and usage
/// instead of preceding output-delta events. It must durably start delivery
/// before forwarding in that shape; a pure terminal lifecycle marker does not.
pub(super) fn completed_response_has_billable_result(value: &Value) -> bool {
    let Some(response) = value.get("response").and_then(Value::as_object) else {
        return false;
    };
    response
        .get("output")
        .and_then(Value::as_array)
        .is_some_and(|output| !output.is_empty())
        || response
            .get("usage")
            .and_then(Value::as_object)
            .is_some_and(|usage| !usage.is_empty())
}

pub(super) fn trim_ascii_whitespace(mut value: &[u8]) -> &[u8] {
    while value.first().is_some_and(u8::is_ascii_whitespace) {
        value = &value[1..];
    }
    while value.last().is_some_and(u8::is_ascii_whitespace) {
        value = &value[..value.len() - 1];
    }
    value
}

pub(super) fn should_capture_buffered_usage(
    is_sse: bool,
    content_type: Option<&HeaderValue>,
) -> bool {
    if is_sse {
        return false;
    }
    let Some(media_type) = content_type
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim)
    else {
        // Some compatible upstreams omit Content-Type even though their body
        // is JSON. Preserve the existing usage parsing contract for them.
        return content_type.is_none();
    };
    media_type.eq_ignore_ascii_case("application/json")
        || media_type
            .get(media_type.len().saturating_sub("+json".len())..)
            .is_some_and(|suffix| suffix.eq_ignore_ascii_case("+json"))
}

#[cfg(test)]
pub(super) fn completed_response_id(
    status: StatusCode,
    transport_complete: bool,
    responses_sse: bool,
    streamed_response_id: Option<String>,
    buffered_tail: &[u8],
) -> Option<String> {
    if !status.is_success() || !transport_complete {
        return None;
    }
    if responses_sse {
        streamed_response_id
    } else {
        extract_response_id(buffered_tail)
    }
}

pub(super) fn extract_response_id(body: &[u8]) -> Option<String> {
    const MAX_RESPONSE_ID_SCAN_BYTES: usize = 2 * 1024 * 1024;

    fn id_from_value(value: &Value) -> Option<String> {
        value
            .pointer("/response/id")
            .or_else(|| value.get("id"))
            .and_then(Value::as_str)
            .and_then(safe_conversation_hint)
    }

    let body = body.get(..body.len().min(MAX_RESPONSE_ID_SCAN_BYTES))?;
    if let Ok(value) = serde_json::from_slice::<Value>(body) {
        return id_from_value(&value);
    }

    let mut top_level_id = None;
    for line in body.split(|byte| *byte == b'\n') {
        let Some(data) = line.strip_prefix(b"data:") else {
            continue;
        };
        let data = data.strip_prefix(b" ").unwrap_or(data);
        let data = data.strip_suffix(b"\r").unwrap_or(data);
        if data == b"[DONE]" {
            continue;
        }
        let Ok(value) = serde_json::from_slice::<Value>(data) else {
            continue;
        };
        if let Some(response_id) = value
            .pointer("/response/id")
            .and_then(Value::as_str)
            .and_then(safe_conversation_hint)
        {
            return Some(response_id);
        }
        if top_level_id.is_none() {
            top_level_id = value
                .get("id")
                .and_then(Value::as_str)
                .and_then(safe_conversation_hint);
        }
    }
    top_level_id
}

pub(super) fn usage_from_value_checked(value: &Value) -> Result<Option<TokenUsage>, ()> {
    let Some(usage) = value
        .get("usage")
        .or_else(|| value.pointer("/message/usage"))
        .or_else(|| value.pointer("/response/usage"))
    else {
        return Ok(None);
    };
    if usage.is_null() {
        return Ok(None);
    }
    let usage = usage.as_object().ok_or(())?;
    let integer = |field: &str| -> Result<Option<i64>, ()> {
        usage
            .get(field)
            .map(|value| value.as_i64().ok_or(()))
            .transpose()
    };
    let input = match integer("input_tokens")? {
        Some(value) => Some(value),
        None => integer("prompt_tokens")?,
    };
    let output = match integer("output_tokens")? {
        Some(value) => Some(value),
        None => integer("completion_tokens")?,
    };
    let (reported_input, output) = match (input, output) {
        (Some(input), Some(output)) => (input, output),
        (Some(input), None) => (input, 0),
        (None, Some(output)) => (0, output),
        // Some OpenAI-compatible providers emit a metadata-only `usage`
        // object (for example only `total_tokens`). Treat that exactly like
        // omitted usage so the caller charges the already-reserved ceilings.
        // A present input/output field with an invalid type still fails above.
        (None, None) => return Ok(None),
    };
    let details_integer = |details_field: &str| -> Result<Option<i64>, ()> {
        let Some(details) = usage.get(details_field) else {
            return Ok(None);
        };
        let details = details.as_object().ok_or(())?;
        details
            .get("cached_tokens")
            .map(|value| value.as_i64().ok_or(()))
            .transpose()
    };
    let cached_input = match details_integer("input_tokens_details")? {
        Some(value) => value,
        None => match details_integer("prompt_tokens_details")? {
            Some(value) => value,
            None => integer("cache_read_input_tokens")?.unwrap_or_default(),
        },
    };
    let cache_write = if let Some(value) = integer("cache_creation_input_tokens")? {
        value
    } else if let Some(details) = usage.get("cache_creation") {
        let details = details.as_object().ok_or(())?;
        let detail_integer = |field: &str| -> Result<i64, ()> {
            details
                .get(field)
                .map(|value| value.as_i64().ok_or(()))
                .transpose()
                .map(Option::unwrap_or_default)
        };
        detail_integer("ephemeral_5m_input_tokens")?
            .checked_add(detail_integer("ephemeral_1h_input_tokens")?)
            .ok_or(())?
    } else {
        0
    };
    // OpenAI prompt/input counts include cached tokens; Anthropic input_tokens
    // excludes its separately reported cache read/write counters.
    let input_includes_cache = usage.contains_key("input_tokens_details")
        || usage.contains_key("prompt_tokens_details")
        || usage.contains_key("prompt_tokens");
    let uncached_input = if input_includes_cache {
        reported_input.checked_sub(cached_input).ok_or(())?
    } else {
        reported_input
    };
    let service_tier_value = value
        .get("service_tier")
        .or_else(|| value.pointer("/response/service_tier"));
    let service_tier = match service_tier_value {
        None => None,
        Some(value) => {
            let tier = value.as_str().ok_or(())?;
            if !is_supported_service_tier(tier) {
                return Err(());
            }
            Some(tier.to_owned())
        }
    };
    let parsed = TokenUsage {
        input_tokens: uncached_input,
        cached_input_tokens: cached_input,
        cache_write_tokens: cache_write,
        output_tokens: output,
        service_tier,
    };
    if [
        parsed.input_tokens,
        parsed.cached_input_tokens,
        parsed.cache_write_tokens,
        parsed.output_tokens,
    ]
    .into_iter()
    .all(|tokens| (0..=MAX_REPORTED_TOKENS).contains(&tokens))
        && parsed
            .input_tokens
            .checked_add(parsed.cached_input_tokens)
            .and_then(|tokens| tokens.checked_add(parsed.cache_write_tokens))
            .is_some()
    {
        Ok(Some(parsed))
    } else {
        Err(())
    }
}

#[cfg(test)]
pub(super) fn usage_from_value(value: &Value) -> Option<TokenUsage> {
    usage_from_value_checked(value).ok().flatten()
}

pub(super) fn is_supported_service_tier(tier: &str) -> bool {
    matches!(
        tier,
        "default" | "auto" | "priority" | "flex" | "scale" | "batch" | "standard_only"
    )
}

pub(super) fn append_bounded(capture: &mut Vec<u8>, chunk: &[u8], maximum: usize) {
    if chunk.len() >= maximum {
        capture.clear();
        capture.extend_from_slice(&chunk[chunk.len() - maximum..]);
        return;
    }
    let overflow = capture
        .len()
        .saturating_add(chunk.len())
        .saturating_sub(maximum);
    if overflow > 0 {
        capture.drain(..overflow);
    }
    capture.extend_from_slice(chunk);
}
