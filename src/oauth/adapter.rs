use crate::{
    error::AppError,
    provider::{
        ManagedOAuthAdapterBackend, ProviderCatalog, ResolvedManagedOAuthAdapter,
        UpstreamCredential,
    },
};

use super::managed;

pub async fn refresh_managed_oauth_credential(
    http: &reqwest::Client,
    adapter: &ResolvedManagedOAuthAdapter,
    credential: &UpstreamCredential,
    allow_test_loopback: bool,
) -> Result<UpstreamCredential, AppError> {
    if !matches!(credential, UpstreamCredential::OAuth { .. })
        || !credential.has_oauth_refresh_state()
    {
        return Err(AppError::BadRequest(
            "managed OAuth credential has no refresh state".into(),
        ));
    }
    match adapter.backend() {
        ManagedOAuthAdapterBackend::Kimi => {
            managed::kimi::refresh(http, credential, allow_test_loopback).await
        }
        ManagedOAuthAdapterBackend::Codex => {
            managed::codex::refresh(http, credential, allow_test_loopback).await
        }
    }
}

/// Resolve the server-owned refresh implementation from the current catalog
/// and compare the stored endpoint only as consistency evidence.
pub fn resolve_managed_oauth_refresh_adapter(
    catalog: &ProviderCatalog,
    driver: &str,
    stored_refresh_url: &str,
) -> Result<ResolvedManagedOAuthAdapter, AppError> {
    let adapter = catalog.managed_oauth_adapter_for_driver(driver)?;
    if adapter.refresh_url() != stored_refresh_url {
        return Err(AppError::Conflict(
            "managed OAuth adapter lifecycle metadata no longer matches the active catalog".into(),
        ));
    }
    Ok(adapter)
}
