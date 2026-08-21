mod accounts;
mod imports;
mod model_catalog;
mod oauth;
mod routes;

pub use accounts::{CreateUpstreamAccountInput, UpdateUpstreamAccountInput};
pub use imports::{
    ImportManagedOAuthAccountInput, ManagedOAuthImportResult, ManagedOAuthImportStatus,
};
pub use model_catalog::{
    AggregatedUpstreamModelCatalogView, AggregatedUpstreamModelView, DiscoveredUpstreamModel,
    ReplaceModelCatalogResult, UpstreamModelCatalogView, UpstreamModelView,
};
pub use oauth::ReauthorizeUpstreamAccountInput;
pub use routes::{CreateModelRouteInput, UpdateModelRouteInput};

const UPSTREAM_CREDENTIAL_ROTATION_RESOURCE: &str = "upstream_credential";
const UPSTREAM_OAUTH_REFRESH_RESOURCE: &str = "upstream_oauth_refresh";
const UPSTREAM_OAUTH_REFRESH_LEASE_MILLIS: i64 = 2 * 60 * 1_000;

pub(super) fn upstream_connection_method(driver: &str, auth_kind: &str) -> String {
    if driver == "cpa-subscription-bridge" {
        "subscription_bridge".to_owned()
    } else {
        auth_kind.to_owned()
    }
}

pub(super) fn upstream_can_reauthorize(
    driver: &str,
    auth_kind: &str,
    oauth_session_id: Option<&str>,
    oauth_driver: Option<&str>,
) -> bool {
    auth_kind == "oauth"
        && oauth_session_id.is_some()
        && (driver == "cpa-subscription-bridge"
            || matches!(oauth_driver, Some("cursor" | "provider_adapter")))
}
