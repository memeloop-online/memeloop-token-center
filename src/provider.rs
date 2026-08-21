mod catalog;
mod credential;
mod types;

pub(crate) use catalog::validate_managed_oauth_adapter_contribution;
pub use catalog::{
    ComponentAdapterContribution, MANAGED_OAUTH_ADAPTER_API_VERSION, ManagedOAuthAdapterBackend,
    ManagedOAuthAdapterContribution, OAuthAdapterContribution, OAuthFlowKind, ProviderCatalog,
    ProviderType, ResolvedManagedOAuthAdapter,
};
pub use credential::{
    UpstreamCredential, open_credential, seal_credential, validate_adapter_state, validate_config,
};
pub(crate) use credential::{open_private_json, seal_private_json};
pub use types::{ModelRouteView, ResolvedUpstream, UpstreamAccountView};

#[cfg(test)]
mod tests;
