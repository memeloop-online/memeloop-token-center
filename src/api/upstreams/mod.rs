mod accounts;
mod health;
mod import;
mod managed_import;
mod oauth;

pub(in crate::api) use accounts::{
    create_upstream, delete_upstream, list_upstreams, rotate_upstream_credential,
    set_upstream_status, update_upstream,
};
pub(in crate::api) use health::probe_upstream_health;
pub(in crate::api) use import::import_cpa_subscription_accounts;
pub(in crate::api) use managed_import::{
    MAX_MANAGED_OAUTH_IMPORT_REQUEST, cpa_managed_oauth_capabilities, import_cpa_managed_oauth,
};

#[cfg(test)]
pub(in crate::api) use import::{cpa_subscription_account, validate_cpa_auth_filename};
pub(crate) use oauth::refresh_managed_upstream_oauth;
pub(in crate::api) use oauth::{
    poll_cursor_oauth, poll_subscription_bridge_oauth, refresh_upstream_oauth, start_cursor_oauth,
    start_provider_adapter_oauth, start_subscription_bridge_oauth,
};
