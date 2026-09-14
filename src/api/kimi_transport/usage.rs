use serde_json::Value;

/// Kimi documents cached_tokens directly under usage. Preserve the OpenAI
/// nested spelling too, but never choose silently between conflicting totals.
/// Every Kimi consumer receives the same complete, validated accounting shape.
pub(in crate::api) fn normalize(value: &Value) -> Result<Value, &'static str> {
    let mut value = value.clone();
    let object = value.as_object_mut().ok_or("kimi_usage_type")?;
    if let Some(cached) = object.remove("cached_tokens") {
        let cached = cached.as_u64().ok_or("kimi_cached_tokens_type")?;
        let details = object
            .entry("prompt_tokens_details")
            .or_insert_with(|| serde_json::json!({}));
        if details.is_null() {
            *details = serde_json::json!({});
        }
        let details = details.as_object_mut().ok_or("kimi_usage_details_type")?;
        if let Some(nested) = details.get("cached_tokens")
            && nested.as_u64() != Some(cached)
        {
            return Err("kimi_cached_tokens_conflict");
        }
        details.insert("cached_tokens".into(), cached.into());
    }
    if object.keys().any(|key| {
        !matches!(
            key.as_str(),
            "prompt_tokens"
                | "completion_tokens"
                | "total_tokens"
                | "prompt_tokens_details"
                | "completion_tokens_details"
        )
    }) {
        return Err("kimi_usage_unknown_field");
    }
    let required = |key: &str| -> Result<i64, &'static str> {
        let value = object.get(key).ok_or("kimi_usage_missing_field")?;
        value
            .as_i64()
            .filter(|value| *value >= 0)
            .ok_or("kimi_usage_field_type")
    };
    let input = required("prompt_tokens")?;
    let output = required("completion_tokens")?;
    let total = required("total_tokens")?;
    if total <= 0 || input.checked_add(output) != Some(total) {
        return Err("usage_total_mismatch");
    }
    if input > crate::api::limits::MAX_REPORTED_TOKENS
        || output > crate::api::limits::MAX_REPORTED_TOKENS
    {
        return Err("kimi_usage_token_limit");
    }
    validate_details(
        object.get("prompt_tokens_details"),
        input,
        &[
            "cached_tokens",
            "audio_tokens",
            "image_tokens",
            "text_tokens",
        ],
    )?;
    validate_details(
        object.get("completion_tokens_details"),
        output,
        &[
            "accepted_prediction_tokens",
            "audio_tokens",
            "reasoning_tokens",
            "rejected_prediction_tokens",
            "text_tokens",
        ],
    )?;
    Ok(value)
}

fn validate_details(
    details: Option<&Value>,
    total: i64,
    fields: &[&str],
) -> Result<(), &'static str> {
    let Some(details) = details.filter(|details| !details.is_null()) else {
        return Ok(());
    };
    let details = details.as_object().ok_or("kimi_usage_details_type")?;
    for (key, value) in details {
        if !fields.contains(&key.as_str()) {
            // In particular, a new cache-write counter must not be silently
            // dropped by Responses or interpreted as ordinary input tokens.
            return Err("kimi_usage_details_unknown_field");
        }
        if value.is_null() {
            continue;
        }
        let count = value
            .as_i64()
            .filter(|value| *value >= 0)
            .ok_or("kimi_usage_details_field_type")?;
        if count > total {
            return Err("usage_detail_exceeds_total");
        }
    }
    Ok(())
}

#[cfg(test)]
pub(in crate::api) fn invalid_examples() -> Vec<Value> {
    use serde_json::json;
    let valid = json!({"prompt_tokens":19,"completion_tokens":13,"total_tokens":32});
    let mut values = Vec::new();
    for (key, value) in [
        ("provider_secret", json!(1)),
        ("total_tokens", json!("32")),
        ("total_tokens", Value::Null),
        ("total_tokens", json!(31)),
        ("prompt_tokens", json!(-1)),
        ("prompt_tokens_details", json!({"cached_tokens":"12"})),
        ("completion_tokens_details", json!({"reasoning_tokens":"2"})),
        ("prompt_tokens_details", json!({"cached_tokens":20})),
        ("completion_tokens_details", json!({"reasoning_tokens":14})),
        ("prompt_tokens_details", json!({"cache_write_tokens":1})),
        ("completion_tokens_details", json!({"provider_secret":1})),
    ] {
        let mut malformed = valid.clone();
        malformed[key] = value;
        values.push(malformed);
    }
    let mut missing = valid;
    missing.as_object_mut().unwrap().remove("total_tokens");
    values.push(missing);
    values
}
