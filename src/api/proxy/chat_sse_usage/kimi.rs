use super::*;

// Only the native Kimi adapter uses this projection. OpenAI's explicit strict
// contract remains unchanged. Metadata extensions cannot change identity,
// choice sequence or token accounting; delta remains intact and billable.
pub(super) fn parse(data: &[u8]) -> Result<CanonicalChatChunk, &'static str> {
    let mut chunk = crate::api::sse::parse_unique_json(data).map_err(|_| "kimi_json_invalid")?;
    if chunk.get("error").is_some_and(|error| !error.is_null()) {
        return Err("provider_error");
    }
    project(
        &mut chunk,
        &[
            "id",
            "object",
            "model",
            "choices",
            "created",
            "system_fingerprint",
            "obfuscation",
            "moderation",
            "service_tier",
            "usage",
        ],
    )
    .ok_or("kimi_envelope_type")?;
    let choices = chunk
        .get_mut("choices")
        .ok_or("kimi_choices_missing")?
        .as_array_mut()
        .ok_or("kimi_choices_type")?;
    let mut parsed_choices = Vec::with_capacity(choices.len());
    for mut choice in std::mem::take(choices) {
        project(
            &mut choice,
            &["index", "delta", "finish_reason", "logprobs"],
        )
        .ok_or("kimi_choice_type")?;
        parsed_choices.push(
            serde_json::from_value::<CanonicalChatChoice>(choice)
                .map_err(|error| schema_reason("choice", &error))?,
        );
    }
    let mut parsed_usage = None;
    if let Some(usage) = chunk.get_mut("usage").filter(|usage| !usage.is_null()) {
        *usage = crate::api::kimi_transport::usage::normalize(usage)?;
        // Unknown accounting fields are not treated as zero, guessed, or
        // silently discarded. Only Kimi's documented cache alias is mapped.
        parsed_usage = Some(
            serde_json::from_value::<CanonicalChatUsage>(usage.take())
                .map_err(|error| schema_reason("usage", &error))?,
        );
    }
    let mut parsed: CanonicalChatChunk =
        serde_json::from_value(chunk).map_err(|error| schema_reason("envelope", &error))?;
    parsed.choices = parsed_choices;
    parsed.usage = parsed_usage;
    Ok(parsed)
}

fn project(value: &mut Value, keys: &[&str]) -> Option<()> {
    value
        .as_object_mut()?
        .retain(|key, _| keys.contains(&key.as_str()));
    Some(())
}

fn schema_reason(layer: &str, error: &serde_json::Error) -> &'static str {
    // Inspect locally only. Serde's original error can contain arbitrary
    // provider field names/values, so never include it in logs or responses.
    let error = error.to_string();
    let kind = if error.starts_with("unknown field") {
        "unknown"
    } else if error.starts_with("missing field") {
        "missing"
    } else {
        "type"
    };
    match (layer, kind) {
        ("choice", "unknown") => "kimi_choice_unknown_field",
        ("choice", "missing") => "kimi_choice_missing_field",
        ("choice", _) => "kimi_choice_field_type",
        ("usage", "unknown") => "kimi_usage_unknown_field",
        ("usage", "missing") => "kimi_usage_missing_field",
        ("usage", _) => "kimi_usage_field_type",
        (_, "unknown") => "kimi_envelope_unknown_field",
        (_, "missing") => "kimi_envelope_missing_field",
        _ => "kimi_envelope_field_type",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn terminal() -> Value {
        json!({"id":"cmpl-xxx","object":"chat.completion.chunk","model":"kimi-k2.6",
            "choices":[{"index":0,"delta":{},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":19,"completion_tokens":13,"total_tokens":32,"cached_tokens":12}})
    }

    #[test]
    fn documented_cache_count_and_metadata_are_kimi_only() {
        let mut value = terminal();
        value["provider_metadata"] = json!({"future":"ignored metadata"});
        value["choices"][0]["provider_metadata"] = json!({"future":true});
        value["usage"]["prompt_tokens_details"] = json!({"cached_tokens":12});
        let data = serde_json::to_vec(&value).unwrap();
        let mut kimi = ChatSseUsageState::for_kimi();
        kimi.observe_data(&data);
        assert!(kimi.terminal_ready());
        let usage = kimi.usage().unwrap();
        assert_eq!(
            (
                usage.input_tokens,
                usage.cached_input_tokens,
                usage.output_tokens
            ),
            (7, 12, 13)
        );
        let mut strict = ChatSseUsageState::default();
        strict.observe_data(&data);
        assert_eq!(strict.invalid_reason(), Some("chat_chunk_schema"));
    }

    #[test]
    fn malformed_core_and_unknown_accounting_fail_closed_with_static_reasons() {
        let cases = [
            ("/usage/cached_tokens", json!(-1), "kimi_cached_tokens_type"),
            (
                "/usage/cached_tokens",
                json!("canary-secret"),
                "kimi_cached_tokens_type",
            ),
            (
                "/usage/cached_tokens",
                json!(20),
                "usage_detail_exceeds_total",
            ),
            (
                "/usage/prompt_tokens_details",
                json!({"cached_tokens":11}),
                "kimi_cached_tokens_conflict",
            ),
            (
                "/usage/provider-secret",
                json!(123),
                "kimi_usage_unknown_field",
            ),
            ("/usage/total_tokens", json!(31), "usage_total_mismatch"),
            ("/choices/0/index", json!(1), "chat_choice_index"),
            (
                "/choices/0/finish_reason",
                json!("canary-secret"),
                "chat_finish_sequence",
            ),
            (
                "/error",
                json!({"message":"canary-secret"}),
                "provider_error",
            ),
        ];
        for (pointer, replacement, expected) in cases {
            let mut value = terminal();
            let (parent, key) = pointer.rsplit_once('/').unwrap();
            value.pointer_mut(parent).unwrap()[key] = replacement;
            let mut state = ChatSseUsageState::for_kimi();
            state.observe_data(&serde_json::to_vec(&value).unwrap());
            assert_eq!(state.invalid_reason(), Some(expected));
            assert!(!state.terminal_ready());
        }
        let mut value = terminal();
        value["usage"]
            .as_object_mut()
            .unwrap()
            .remove("total_tokens");
        assert!(matches!(
            parse(&serde_json::to_vec(&value).unwrap()),
            Err("kimi_usage_missing_field")
        ));
        let mut value = terminal();
        value["choices"][0]["delta"] = Value::Null;
        assert!(matches!(
            parse(&serde_json::to_vec(&value).unwrap()),
            Err("kimi_choice_field_type")
        ));
        assert!(matches!(
            parse(br#"{"choices":[],"choices":[]}"#),
            Err("kimi_json_invalid")
        ));
    }
}
