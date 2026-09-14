use serde_json::Value;

/// Kimi documents cached_tokens directly under usage. Preserve the OpenAI
/// nested spelling too, but never choose silently between conflicting totals.
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
    Ok(value)
}
