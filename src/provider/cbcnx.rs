use serde_json::{Value, json};

use super::catalog::ProviderType;

/// Built-in driver for 广电's CBCNX OpenAI-compatible service.
///
/// The driver deliberately describes only the wire contracts we can exercise
/// through the normal OpenAI proxy and Images paths.  In particular, a model
/// name alone never makes an asynchronous video API available: that requires
/// a separately reviewed submit/poll/result-archive contract.
pub const CBCNX_PROVIDER_DRIVER: &str = "cbcnx";

/// CBCNX shares the bounded, credentialed HTTP transport with the generic
/// OpenAI-compatible driver.  Keep this classification narrow so driver
/// additions cannot silently inherit an outbound protocol contract.
pub fn is_openai_compatible_http_driver(driver: &str) -> bool {
    matches!(driver, "http-json" | CBCNX_PROVIDER_DRIVER)
}

pub(super) fn provider_type(credential_schema: Value) -> ProviderType {
    ProviderType {
        id: CBCNX_PROVIDER_DRIVER.to_owned(),
        display_name: "广电（CBCNX）".to_owned(),
        protocols: vec!["openai".to_owned(), "generation".to_owned()],
        // Do not advertise video from a model-name list.  Text, embeddings,
        // and standard OpenAI Images each use already bounded core paths.
        modalities: vec![
            "text".to_owned(),
            "embedding".to_owned(),
            "image".to_owned(),
        ],
        config_schema: json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "additionalProperties": false,
            "required": ["base_url", "stream_usage_contract"],
            "properties": {
                "base_url": {
                    "type": "string",
                    "format": "uri",
                    "title": "广电 API 地址",
                    "description": "Administrator-provided CBCNX OpenAI-compatible API base. Credentials must not be embedded in the URL."
                },
                "network_scope": {
                    "title": "Network scope",
                    "type": "string",
                    "enum": ["public", "private"],
                    "default": "public",
                    "description": "Private destinations require a global operator credential."
                },
                "timeout_seconds": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 600,
                    "default": 120
                },
                "stream_usage_contract": {
                    "title": "Streaming usage contract",
                    "const": "openai-chat-usage-only",
                    "default": "openai-chat-usage-only",
                    "description": "CBCNX Chat streams must use the verified OpenAI include_usage terminal usage shape when usage is requested."
                },
                "input_token_overhead_ceiling": {
                    "title": "Input token overhead ceiling",
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 1000000,
                    "default": 0,
                    "description": "Trusted reservation allowance for input tokens added by the compatible upstream outside the forwarded request body."
                },
                "image_api_mode": {
                    "title": "Image generation API",
                    "const": "images",
                    "default": "images",
                    "readOnly": true,
                    "description": "CBCNX image routes use the standard OpenAI Images endpoint."
                },
                "result_origins": {
                    "title": "Generated asset origins",
                    "type": "array",
                    "uniqueItems": true,
                    "items": {"type": "string", "format": "uri"},
                    "description": "Exact origins allowed for generated image asset archival. Base64 image responses do not require an origin."
                }
            }
        }),
        credential_schema,
        oauth_adapter: None,
        managed_oauth_adapter: None,
        component_adapter: None,
        source: "builtin".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_reviewed_http_drivers_share_the_generic_transport() {
        assert!(is_openai_compatible_http_driver("http-json"));
        assert!(is_openai_compatible_http_driver(CBCNX_PROVIDER_DRIVER));
        assert!(!is_openai_compatible_http_driver("openai-codex"));
        assert!(!is_openai_compatible_http_driver("volcengine-seedance"));
    }
}
