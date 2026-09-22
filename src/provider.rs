pub mod antigravity;
mod catalog;
mod codex_agent_profile;
mod credential;
mod transport_policy;
mod types;

pub(crate) use catalog::{
    CODEX_AGENT_INSTRUCTIONS_TEMPLATE_V1, CODEX_MODEL_CAPABILITIES_VERSION,
    is_bundled_codex_model_slug,
};
pub use catalog::{
    CodexModelCapabilities, CodexReasoningLevel, ComponentAdapterContribution,
    GenerationAdapterContribution, OAuthAdapterContribution, OAuthFlowKind, ProviderCatalog,
    ProviderType, RESPONSES_VIA_CHAT_COMPACTION_CONFIG, RequestCompatibility,
    ResponsesViaChatDialect, canonicalize_provider_driver, is_openai_compatible_http_driver,
};
pub(crate) use catalog::{ManagedOAuthAdapterBackend, ResolvedManagedOAuthAdapter};
pub(crate) use codex_agent_profile::CODEX_GENERIC_AGENT_INSTRUCTIONS_V1;
pub use credential::{
    UpstreamCredential, open_credential, seal_credential, validate_adapter_state, validate_config,
};
pub(crate) use credential::{
    open_private_json, seal_private_json, seal_private_json_with_nonce, validate_codex_proxy_url,
    validate_oauth_remote_dns_proxy_url, validate_provider_adapter_secret_patch,
    validate_proxy_url,
};
pub(crate) use transport_policy::{CodexChatControlPolicy, CodexTransportPolicy, SseFramingLimits};
pub use types::{
    AuthorizedUpstreamCandidate, ModelRouteView, ResolvedUpstream, UpstreamAccountView,
    UpstreamDeletionReadiness,
};
pub(crate) use types::{PROXY_ROUTING_POLICY, UpstreamTransportSnapshot};

#[cfg(test)]
mod tests;
