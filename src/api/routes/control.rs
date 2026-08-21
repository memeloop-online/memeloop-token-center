use super::super::*;

pub(in crate::api) fn control_router(state: AppState) -> Router<AppState> {
    let authenticated = Router::new()
        .route("/internal/v1/keys", get(list_keys).post(create_key))
        .route("/internal/v1/keys/{key_id}/rotate", post(rotate_key))
        .route("/internal/v1/keys/{key_id}/alias", patch(rename_key))
        .route("/internal/v1/keys/{key_id}/limits", get(key_limits))
        .route("/internal/v1/keys/{key_id}/policy", put(update_key_policy))
        .route(
            "/internal/v1/keys/{key_id}/routing",
            get(get_credential_routing).put(replace_credential_routing),
        )
        .route("/internal/v1/keys/{key_id}/status", patch(set_key_status))
        .route(
            "/internal/v1/keys/{key_id}/legacy-credentials",
            post(register_legacy_key_credential),
        )
        .route(
            "/internal/v1/service-tokens",
            get(list_service_tokens).post(create_service_token),
        )
        .route(
            "/internal/v1/service-tokens/{service_id}/rotate",
            post(rotate_service_token),
        )
        .route(
            "/internal/v1/service-tokens/{service_id}/status",
            patch(set_service_token_status),
        )
        .route("/internal/v1/provider-types", get(provider_types))
        .route("/internal/v1/tenants", get(list_tenants))
        .route("/internal/v1/plugins", get(plugin_manifests))
        .route(
            "/internal/v1/plugins/{plugin_id}/configuration",
            get(get_plugin_configuration).put(put_plugin_configuration),
        )
        .route("/internal/v1/schemas", get(configuration_schemas))
        .route("/internal/v1/oauth/cursor/start", post(start_cursor_oauth))
        .route("/internal/v1/oauth/cursor/poll", post(poll_cursor_oauth))
        .route("/internal/v1/oauth/codex/start", post(start_codex_oauth))
        .route("/internal/v1/oauth/codex/poll", post(poll_codex_oauth))
        .route(
            "/internal/v1/oauth/provider-adapter/start",
            post(start_provider_adapter_oauth),
        )
        .route(
            "/internal/v1/oauth/provider-adapter/poll",
            post(poll_cursor_oauth),
        )
        .route(
            "/internal/v1/upstreams",
            get(list_upstreams).post(create_upstream),
        )
        .route(
            "/internal/v1/imports/cpa/managed-oauth/capabilities",
            get(cpa_managed_oauth_capabilities),
        )
        .route(
            "/internal/v1/imports/cpa/managed-oauth",
            post(import_cpa_managed_oauth)
                .layer(DefaultBodyLimit::max(MAX_MANAGED_OAUTH_IMPORT_REQUEST)),
        )
        .route(
            "/internal/v1/imports/session-archive/quarantine",
            get(list_archive_quarantine),
        )
        .route(
            "/internal/v1/imports/session-archive/quarantine/{quarantine_id}",
            get(get_archive_quarantine),
        )
        .route(
            "/internal/v1/imports/session-archive/quarantine/{quarantine_id}/resolutions",
            post(resolve_archive_quarantine),
        )
        .route("/internal/v1/requests", get(internal_requests))
        .route(
            "/internal/v1/requests/{request_id}",
            get(internal_request_detail),
        )
        .route(
            "/internal/v1/requests/{request_id}/assets/{asset_id}",
            get(internal_request_asset),
        )
        .route(
            "/internal/v1/generations/{job_id}/assets/{asset_id}",
            get(internal_generation_asset),
        )
        .route("/internal/v1/stats", get(internal_stats))
        .route(
            "/internal/v1/usage-analysis",
            get(usage_analysis::internal_usage_analysis),
        )
        .route("/internal/v1/request-events", get(internal_request_events))
        .route("/internal/v1/sessions", get(internal_sessions))
        .route(
            "/internal/v1/sessions/{session_id}",
            get(internal_session_detail),
        )
        .route(
            "/internal/v1/upstreams/{account_id}/credential",
            put(rotate_upstream_credential),
        )
        .route(
            "/internal/v1/upstreams/{account_id}",
            put(update_upstream)
                .patch(set_upstream_status)
                .delete(delete_upstream),
        )
        .route(
            "/internal/v1/upstreams/{account_id}/health",
            post(probe_upstream_health),
        )
        .route(
            "/internal/v1/upstreams/{account_id}/models",
            get(list_upstream_models),
        )
        .route(
            "/internal/v1/upstreams/{account_id}/models/sync",
            post(sync_upstream_models),
        )
        .route(
            "/internal/v1/upstream-models",
            get(aggregate_upstream_models),
        )
        .route(
            "/internal/v1/upstreams/{account_id}/oauth/refresh",
            post(refresh_upstream_oauth),
        )
        .route(
            "/internal/v1/model-routes",
            get(list_model_routes).post(create_model_route),
        )
        .route(
            "/internal/v1/model-routes/{route_id}",
            put(update_model_route)
                .patch(set_model_route_enabled)
                .delete(delete_model_route),
        )
        .route(
            "/internal/v1/model-routes/{route_id}/routing",
            get(get_route_routing).put(replace_route_routing),
        )
        .route(
            "/internal/v1/provider-groups",
            get(list_provider_groups).post(create_provider_group),
        )
        .route(
            "/internal/v1/provider-groups/{group_id}",
            put(update_provider_group).delete(delete_provider_group),
        )
        .route(
            "/internal/v1/provider-groups/{group_id}/members",
            put(replace_provider_group_members),
        )
        .route(
            "/internal/v1/route-groups",
            get(list_route_groups).post(create_route_group),
        )
        .route(
            "/internal/v1/route-groups/{group_id}",
            put(update_route_group).delete(delete_route_group),
        )
        .route(
            "/internal/v1/route-groups/{group_id}/members",
            put(replace_route_group_members),
        )
        .route(
            "/internal/v1/credential-groups",
            get(list_credential_groups).post(create_credential_group),
        )
        .route(
            "/internal/v1/credential-groups/{group_id}",
            put(update_credential_group).delete(delete_credential_group),
        )
        .route(
            "/internal/v1/credential-groups/{group_id}/members",
            put(replace_credential_group_members),
        )
        .route("/internal/v1/model-prices", get(list_model_prices))
        .route(
            "/internal/v1/model-prices/usage-summary",
            get(model_price_usage_summary),
        )
        .route("/internal/v1/model-prices/sync", post(sync_model_prices))
        .route("/internal/v1/prices/{currency}/{model}", post(upsert_price))
        .route(
            "/internal/v1/generation-prices/{currency}/{model}",
            post(upsert_generation_price),
        )
        .route(
            "/internal/v1/generation-prices",
            get(list_generation_prices),
        )
        .route(
            "/internal/v1/accounts/{account_id}/grants",
            post(grant_balance),
        )
        .route(
            "/internal/v1/accounts/{account_id}/grant-reversals",
            post(reverse_grant_balance),
        )
        .route(
            "/internal/v1/accounts/{account_id}/ledger",
            get(list_account_ledger),
        )
        .route(
            "/internal/v1/entitlements",
            get(list_entitlements).put(reconcile_entitlement),
        )
        .route("/internal/v1/entitlements/cancel", post(cancel_entitlement))
        .route(
            "/internal/v1/entitlements/replace",
            post(replace_entitlement),
        )
        .route(
            "/internal/v1/integrations/memeloop-cloud/events",
            get(list_memeloop_cloud_subscription_events),
        )
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            authenticate_control_before_body,
        ))
        .layer(ConcurrencyLimitLayer::new(CONTROL_IN_FLIGHT_REQUESTS));
    Router::new()
        .route("/operator", get(operator_index))
        .route(
            "/internal/v1/integrations/memeloop-cloud/subscription",
            put(sync_memeloop_cloud_subscription)
                .layer(DefaultBodyLimit::max(MAX_CLOUD_WEBHOOK_BODY))
                .layer(middleware::from_fn_with_state(
                    state,
                    admit_cloud_webhook_before_body,
                )),
        )
        .merge(authenticated)
}
