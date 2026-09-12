mod accounts;
mod availability;
mod health;
mod models;
mod native_codex_upgrade;
mod native_oauth_import;
mod oauth;
mod oauth_claude;
mod oauth_copilot;
mod quota;

fn restrict_transport_proxy_capability(
    service: &crate::model::AuthenticatedService,
    account: &mut crate::provider::UpstreamAccountView,
) {
    account.can_update_transport_proxy &= service.tenant_external_id.is_none();
}

pub(in crate::api) use accounts::{
    create_upstream, delete_upstream, get_upstream_deletion_readiness, list_upstreams,
    rotate_codex_transport_proxy, rotate_upstream_credential, set_upstream_status, update_upstream,
};
pub(in crate::api) use availability::upstream_account_availability;
pub(in crate::api) use health::probe_upstream_health;
pub(crate) use models::trigger_upstream_model_sync;
pub(in crate::api) use models::{
    aggregate_upstream_models, list_upstream_models, sync_upstream_models,
};
pub(in crate::api) use native_codex_upgrade::{
    apply_native_codex_upgrade, prepare_native_codex_upgrade,
};
pub(in crate::api) use native_oauth_import::{
    MAX_NATIVE_KIMI_COHORT_REQUEST, import_native_kimi_oauth_cohort,
    native_oauth_import_capabilities,
};
pub(in crate::api) use quota::upstream_quota;
pub(in crate::api) use quota::{
    confirm_quota_reset, get_quota_reset, prepare_quota_reset, reconcile_quota_reset,
};

pub(in crate::api) use oauth::{
    disconnect_upstream_oauth, poll_codex_oauth, poll_cursor_oauth, refresh_upstream_oauth,
    start_codex_oauth, start_cursor_oauth, start_provider_adapter_oauth,
};
pub(crate) use oauth::{refresh_managed_upstream_oauth, refresh_managed_upstream_oauth_for_worker};
pub(in crate::api) use oauth_claude::{complete_claude_oauth, start_claude_oauth};
pub(crate) use oauth_copilot::trigger_copilot_remint_on_auth_failure;
pub(in crate::api) use oauth_copilot::{poll_copilot_oauth, start_copilot_oauth};
