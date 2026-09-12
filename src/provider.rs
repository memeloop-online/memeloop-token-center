mod catalog;
mod cbcnx;
mod credential;
mod transport_policy;
mod types;

pub use catalog::{
    ComponentAdapterContribution, OAuthAdapterContribution, OAuthFlowKind, ProviderCatalog,
    ProviderType,
};
pub(crate) use catalog::{ManagedOAuthAdapterBackend, ResolvedManagedOAuthAdapter};
pub use cbcnx::{CBCNX_PROVIDER_DRIVER, is_openai_compatible_http_driver};
pub use credential::{
    UpstreamCredential, open_credential, seal_credential, validate_adapter_state, validate_config,
};
pub(crate) use credential::{open_private_json, seal_private_json, validate_codex_proxy_url};
pub(crate) use transport_policy::CodexTransportPolicy;
pub use types::{
    AuthorizedUpstreamCandidate, ModelRouteView, ResolvedUpstream, UpstreamAccountView,
    UpstreamDeletionReadiness,
};
pub(crate) use types::{PROXY_ROUTING_POLICY, UpstreamTransportSnapshot};

#[cfg(test)]
mod tests;
