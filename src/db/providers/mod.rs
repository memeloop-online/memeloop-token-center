mod accounts;
mod imports;
mod oauth;
mod routes;

pub use accounts::{CreateUpstreamAccountInput, UpdateUpstreamAccountInput};
pub use imports::{
    ImportManagedOAuthAccountInput, ManagedOAuthImportResult, ManagedOAuthImportStatus,
};
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
