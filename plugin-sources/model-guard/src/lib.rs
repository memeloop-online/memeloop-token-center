//! Opt-in model admission policy. No request parsing, rewriting, host calls,
//! credential access, provider contribution or externally visible side effects.
wit_bindgen::generate!({ path: "../../wit/token-center.wit", world: "plugin" });

use memeloop::token_center::types::{Decision, Metering, RequestContext};

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Configuration {
    blocked_models: Vec<String>,
}

fn decide(model: &str, configuration: &str) -> Result<Decision, String> {
    let config: Configuration = serde_json::from_str(configuration)
        .map_err(|_| "invalid model guard configuration".to_owned())?;
    if config.blocked_models.len() > 128
        || config
            .blocked_models
            .iter()
            .any(|model| model.is_empty() || model.chars().count() > 256)
    {
        return Err("invalid model guard configuration".into());
    }
    let blocked = config.blocked_models.iter().any(|entry| entry == model);
    Ok(Decision {
        allow: !blocked,
        reason: blocked.then(|| "model blocked by configured policy".into()),
        model: None,
        upstream_account_id: None,
        request_json: None,
    })
}

struct ModelGuard;

impl exports::memeloop::token_center::traffic_policy::Guest for ModelGuard {
    fn post_auth(context: RequestContext, _request_json: String) -> Result<Decision, String> {
        decide(&context.model, &context.config_json)
    }
}

// WIT 0.2's existing world requires these exports. The manifest declares no
// provider, so the host never calls them; fail explicitly if invoked directly.
impl exports::memeloop::token_center::upstream_provider::Guest for ModelGuard {
    fn list_models(_: String) -> Result<String, String> {
        Err("not a provider".into())
    }
    fn quote(_: RequestContext, _: String) -> Result<Metering, String> {
        Err("not a provider".into())
    }
    fn prepare(_: RequestContext, _: String, _: String) -> Result<String, String> {
        Err("not a provider".into())
    }
    fn normalize(_: RequestContext, _: String) -> Result<String, String> {
        Err("not a provider".into())
    }
}

// Component ABI symbols are valid Wasm exports, not native ELF symbol names.
// Keep native algorithm tests linkable while exporting the actual component.
#[cfg(target_arch = "wasm32")]
export!(ModelGuard);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_leave_every_request_unchanged() {
        for model in ["kimi-k2", "gpt-6", "", "MODEL"] {
            let decision = decide(model, r#"{"blocked_models":[]}"#).unwrap();
            assert!(decision.allow);
            assert!(decision.reason.is_none());
            assert!(decision.model.is_none());
            assert!(decision.request_json.is_none());
            assert!(decision.upstream_account_id.is_none());
        }
    }

    #[test]
    fn only_explicit_exact_ids_are_blocked() {
        let config = r#"{"blocked_models":["retired-model"]}"#;
        assert!(!decide("retired-model", config).unwrap().allow);
        assert!(decide("Retired-model", config).unwrap().allow);
        assert!(decide("retired-model-v2", config).unwrap().allow);
        assert!(decide("other", config).unwrap().allow);
        assert!(decide("other", "{}").is_err());
        assert!(decide("other", r#"{"blocked_models":[],"extra":true}"#).is_err());
    }
}
