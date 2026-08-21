mod accounts;
mod health;
mod managed_import;
mod models;
mod oauth;
mod oauth_claude;
mod oauth_copilot;

pub(in crate::api) use accounts::{
    create_upstream, delete_upstream, list_upstreams, rotate_upstream_credential,
    set_upstream_status, update_upstream,
};
pub(in crate::api) use health::probe_upstream_health;
pub(in crate::api) use managed_import::{
    MAX_MANAGED_OAUTH_IMPORT_REQUEST, cpa_managed_oauth_capabilities, import_cpa_managed_oauth,
};
pub(crate) use models::trigger_upstream_model_sync;
pub(in crate::api) use models::{
    aggregate_upstream_models, list_upstream_models, sync_upstream_models,
};

pub(crate) use oauth::refresh_managed_upstream_oauth;
pub(in crate::api) use oauth::{
    disconnect_upstream_oauth, poll_codex_oauth, poll_cursor_oauth, refresh_upstream_oauth,
    start_codex_oauth, start_cursor_oauth, start_provider_adapter_oauth,
};
pub(in crate::api) use oauth_claude::{complete_claude_oauth, start_claude_oauth};
pub(in crate::api) use oauth_copilot::{poll_copilot_oauth, start_copilot_oauth};
