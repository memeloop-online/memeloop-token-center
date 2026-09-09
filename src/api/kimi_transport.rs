//! Native Kimi Code transport. Model aliases/catalog are pinned to
//! linonetwo/CLIProxyAPI v7.2.128-onetwo.1, not a runtime dependency on that service.
use super::{AppError, Protocol};
use serde_json::{Value, json};

pub(super) fn supports(protocol: Protocol) -> bool {
    matches!(
        protocol,
        Protocol::OpenAiChat | Protocol::AnthropicMessages | Protocol::AnthropicCountTokens
    )
}

pub(super) fn normalize_model(model: &str) -> String {
    let model = model.trim();
    let (base, suffix) = match model.rfind('(') {
        Some(index) if model.ends_with(')') => (&model[..index], &model[index..]),
        _ => (model, ""),
    };
    let base = base.trim().to_ascii_lowercase();
    let base = base.strip_suffix("[1m]").unwrap_or(&base);
    let normalized = match base {
        "kimi-k2.7-code" | "k2.7-code" | "kimi-for-coding" | "for-coding" => "kimi-for-coding",
        "kimi-k2.7-code-highspeed"
        | "k2.7-code-highspeed"
        | "kimi-for-coding-highspeed"
        | "for-coding-highspeed" => "kimi-for-coding-highspeed",
        _ => base.strip_prefix("kimi-").unwrap_or(base),
    };
    format!("{normalized}{suffix}")
}

pub(super) fn prepare(
    protocol: Protocol,
    model: &str,
    request: &mut Value,
) -> Result<(), AppError> {
    if !supports(protocol) {
        return Err(AppError::BadRequest(
            "native Kimi OAuth supports Chat Completions and Anthropic Messages/count_tokens; this protocol is unavailable".into(),
        ));
    }
    let object = request
        .as_object_mut()
        .ok_or_else(|| AppError::BadRequest("request body must be an object".into()))?;
    object.insert("model".into(), Value::String(normalize_model(model)));
    if matches!(protocol, Protocol::OpenAiChat)
        && object.get("stream").and_then(Value::as_bool) == Some(true)
    {
        let options = object.entry("stream_options").or_insert_with(|| json!({}));
        let options = options
            .as_object_mut()
            .ok_or_else(|| AppError::BadRequest("stream_options must be an object".into()))?;
        // Billing must see the terminal usage chunk even when the client omitted
        // this option; retain every other client stream option.
        options.insert("include_usage".into(), Value::Bool(true));
    }
    Ok(())
}

pub(super) fn catalog() -> Vec<crate::db::DiscoveredUpstreamModel> {
    [
        ("kimi-k2", 131_072, 32_768),
        ("kimi-k2-thinking", 131_072, 32_768),
        ("kimi-k2.5", 262_144, 32_768),
        ("kimi-k2.6", 262_144, 65_536),
        ("kimi-k2.7-code", 262_144, 65_536),
        ("kimi-k2.7-code-highspeed", 262_144, 65_536),
        ("kimi-k3", 1_048_576, 65_536),
        ("kimi-k3-256k", 262_144, 65_536),
    ]
    .into_iter()
    .flat_map(|(id, context, output)| {
        ["openai", "anthropic"].map(|protocol| crate::db::DiscoveredUpstreamModel {
            model_id: id.into(),
            protocol: protocol.into(),
            context_window: Some(context),
            reservation_token_bound: Some(output),
            reservation_bound_source: Some("provider_max_output_tokens".into()),
        })
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_source_aliases_are_idempotent_and_preserve_thinking_suffix() {
        for (input, expected) in [
            ("kimi-k3[1m](1024)", "k3(1024)"),
            ("KIMI-K2.7-CODE", "kimi-for-coding"),
            ("kimi-for-coding", "kimi-for-coding"),
            ("k2.7-code-highspeed", "kimi-for-coding-highspeed"),
            ("kimi-k3-256k", "k3-256k"),
        ] {
            assert_eq!(normalize_model(input), expected);
        }
    }

    #[test]
    fn streaming_requires_usage_without_erasing_options_or_messages() {
        let mut body = json!({"stream":true,"stream_options":{"include_usage":false,"x":1},
            "messages":[{"role":"assistant","reasoning_content":"test","tool_calls":[]}]});
        let messages = body["messages"].clone();
        prepare(Protocol::OpenAiChat, "kimi-k3", &mut body).unwrap();
        assert_eq!(body["stream_options"], json!({"include_usage":true,"x":1}));
        assert_eq!(body["messages"], messages);
        assert_eq!(body["model"], "k3");
    }

    #[test]
    fn anthropic_is_native_and_responses_never_silently_translated() {
        let mut body = json!({"thinking":{"type":"enabled","budget_tokens":1024}});
        prepare(Protocol::AnthropicMessages, "kimi-k3", &mut body).unwrap();
        assert_eq!(body["thinking"]["budget_tokens"], 1024);
        assert!(prepare(Protocol::OpenAiResponses, "kimi-k3", &mut body).is_err());
        assert!(prepare(Protocol::OpenAiEmbeddings, "kimi-k3", &mut body).is_err());
        assert!(supports(Protocol::AnthropicCountTokens));
        assert_eq!(catalog().len(), 16);
    }
}
