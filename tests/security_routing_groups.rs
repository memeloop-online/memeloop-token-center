use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use memeloop_token_center::{
    AppState, api,
    config::Config,
    db::{
        CreateGroupInput, CreateKeyInput, CreateRoutedModelRouteInput, CreateUpstreamAccountInput,
        DiscoveredUpstreamModel, GroupKind, NewRequest, ReplaceCredentialRoutingInput,
        ReplaceGroupMembersInput, ReplaceModelCatalogResult, RouteSelectionOptions,
    },
    error::AppError,
    model::KeyPolicy,
    provider::UpstreamCredential,
};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use sqlx::{Row, any::AnyPoolOptions};
use tower::ServiceExt;
use uuid::Uuid;

async fn api_json(state: &AppState, method: &str, path: String, body: Value) -> StatusCode {
    api::router(state.clone())
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header(header::AUTHORIZATION, "Bearer test-service-token")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .expect("routing security response")
        .status()
}

async fn public_models(state: &AppState, key: &str) -> Value {
    let response = api::router(state.clone())
        .oneshot(
            Request::get("/v1/models")
                .header(header::AUTHORIZATION, format!("Bearer {key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("public models response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("bounded public models body");
    serde_json::from_slice(&body).expect("public models JSON")
}

fn key_input(tenant: &str, principal: &str) -> CreateKeyInput {
    CreateKeyInput {
        tenant_external_id: tenant.to_owned(),
        principal_external_id: principal.to_owned(),
        alias: principal.to_owned(),
        currency: "USD".to_owned(),
        policy: KeyPolicy {
            allowed_models: vec![
                "secure-route-model".to_owned(),
                "secure-generation-model".to_owned(),
                "secure-bridge-fallback-model".to_owned(),
                "secure-filtered-model".to_owned(),
            ],
            ..KeyPolicy::default()
        },
        initial_balance: Decimal::TEN,
        idempotency_key: None,
    }
}

#[test]
fn routing_contract_uses_only_provider_route_and_credential_group_terminology() {
    let contract = [
        include_str!("../openapi/openapi.yaml"),
        include_str!("../docs/api-contract.md"),
        include_str!("../docs/security-audit.md"),
        include_str!("../web/src/i18n.tsx"),
    ]
    .join("\n")
    .to_lowercase();
    for legacy in [
        "provider tag",
        "route tag",
        "credential tag",
        "provider pool",
        "route pool",
        "credential pool",
        "provider rule group",
        "route rule group",
        "credential rule group",
        "提供商标签",
        "路由标签",
        "凭据标签",
        "提供商池",
        "路由池",
        "凭据池",
        "规则组",
    ] {
        assert!(
            !contract.contains(legacy),
            "routing contract contains legacy terminology: {legacy}"
        );
    }
}

async fn exercise_group_routing_security(database_url: String, backend: &str) {
    let state = AppState::initialize(Config::for_test(database_url.clone()))
        .await
        .unwrap_or_else(|error| panic!("initialize {backend} routing security state: {error}"));
    let pepper = state.config.key_pepper.as_bytes();
    let exact = state
        .db
        .create_key(key_input("routing-a", "exact-credential"), pepper)
        .await
        .expect("exact credential");
    let classified_only = state
        .db
        .create_key(key_input("routing-a", "classified-only"), pepper)
        .await
        .expect("classified-only credential");
    let group_granted = state
        .db
        .create_key(key_input("routing-a", "route-group-granted"), pepper)
        .await
        .expect("route-group-granted credential");
    let other_tenant = state
        .db
        .create_key(key_input("routing-b", "other-tenant"), pepper)
        .await
        .expect("other-tenant credential");
    let account_a = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: "routing-a".to_owned(),
                name: "routing-a-upstream".to_owned(),
                driver: "http-json".to_owned(),
                config: json!({"base_url": "http://127.0.0.1:18081", "network_scope": "private"}),
                credential: UpstreamCredential::None,
                oauth_session_id: None,
                oauth_driver: None,
                oauth_refresh_url: None,
            },
            pepper,
        )
        .await
        .expect("tenant A upstream");
    let account_b = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: "routing-b".to_owned(),
                name: "routing-b-upstream".to_owned(),
                driver: "http-json".to_owned(),
                config: json!({"base_url": "http://127.0.0.1:18082", "network_scope": "private"}),
                credential: UpstreamCredential::None,
                oauth_session_id: None,
                oauth_driver: None,
                oauth_refresh_url: None,
            },
            pepper,
        )
        .await
        .expect("tenant B upstream");

    let account_a_secondary = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: "routing-a".to_owned(),
                name: "routing-a-secondary-upstream".to_owned(),
                driver: "http-json".to_owned(),
                config: json!({"base_url": "http://127.0.0.1:18084", "network_scope": "private"}),
                credential: UpstreamCredential::None,
                oauth_session_id: None,
                oauth_driver: None,
                oauth_refresh_url: None,
            },
            pepper,
        )
        .await
        .expect("tenant A secondary upstream");

    for (account, tenant) in [
        (&account_a, "routing-a"),
        (&account_a_secondary, "routing-a"),
        (&account_b, "routing-b"),
    ] {
        let lease = Uuid::now_v7();
        assert!(
            state
                .db
                .claim_upstream_model_catalog_sync(
                    account.id,
                    tenant,
                    account.credential_generation,
                    lease,
                )
                .await
                .expect("claim trusted model catalog")
        );
        assert_eq!(
            state
                .db
                .replace_upstream_model_catalog(
                    account.id,
                    tenant,
                    account.credential_generation,
                    lease,
                    "openai_v1",
                    &[DiscoveredUpstreamModel {
                        model_id: "secure-upstream-model".to_owned(),
                        protocol: "any".to_owned(),
                        context_window: None,
                        reservation_token_bound: None,
                        reservation_bound_source: None,
                    }],
                )
                .await
                .expect("replace trusted model catalog"),
            ReplaceModelCatalogResult::Replaced
        );
    }

    let provider_group = state
        .db
        .create_group(
            GroupKind::Provider,
            CreateGroupInput {
                tenant_external_id: "routing-a".to_owned(),
                name: "Provider group A".to_owned(),
            },
        )
        .await
        .expect("provider group");
    let provider_group = state
        .db
        .replace_group_members(
            GroupKind::Provider,
            provider_group.id,
            ReplaceGroupMembersInput {
                tenant_external_id: "routing-a".to_owned(),
                member_ids: vec![account_a.id],
                expected_updated_at: provider_group.updated_at,
            },
        )
        .await
        .expect("provider group active member");
    let candidate_provider_group = state
        .db
        .create_group(
            GroupKind::Provider,
            CreateGroupInput {
                tenant_external_id: "routing-a".to_owned(),
                name: "All candidate providers".to_owned(),
            },
        )
        .await
        .expect("candidate provider group");
    let candidate_provider_group = state
        .db
        .replace_group_members(
            GroupKind::Provider,
            candidate_provider_group.id,
            ReplaceGroupMembersInput {
                tenant_external_id: "routing-a".to_owned(),
                member_ids: vec![account_a.id, account_a_secondary.id],
                expected_updated_at: candidate_provider_group.updated_at,
            },
        )
        .await
        .expect("candidate provider group members");
    let route_group = state
        .db
        .create_group(
            GroupKind::Route,
            CreateGroupInput {
                tenant_external_id: "routing-a".to_owned(),
                name: "Route group A".to_owned(),
            },
        )
        .await
        .expect("route group");
    let secondary_route_group = state
        .db
        .create_group(
            GroupKind::Route,
            CreateGroupInput {
                tenant_external_id: "routing-a".to_owned(),
                name: "Route group B".to_owned(),
            },
        )
        .await
        .expect("secondary route group");
    let credential_group = state
        .db
        .create_group(
            GroupKind::Credential,
            CreateGroupInput {
                tenant_external_id: "routing-a".to_owned(),
                name: "Credential group A".to_owned(),
            },
        )
        .await
        .expect("credential group");
    let secondary_credential_group = state
        .db
        .create_group(
            GroupKind::Credential,
            CreateGroupInput {
                tenant_external_id: "routing-a".to_owned(),
                name: "Credential group B".to_owned(),
            },
        )
        .await
        .expect("secondary credential group");
    let (route, _) = state
        .db
        .create_routed_model_route(CreateRoutedModelRouteInput {
            tenant_external_id: "routing-a".to_owned(),
            public_model: "secure-route-model".to_owned(),
            upstream_model: "secure-upstream-model".to_owned(),
            protocol: "openai".to_owned(),
            priority: 0,
            upstream_account_ids: vec![account_a.id],
            included_provider_group_ids: Vec::new(),
            excluded_provider_group_ids: Vec::new(),
            route_group_ids: vec![route_group.id],
            route_group_names: Vec::new(),
            granted_credential_ids: vec![exact.key_id],
            custom_model_confirmed: false,
        })
        .await
        .expect("exact-credential route");
    let (generation_route, _) = state
        .db
        .create_routed_model_route(CreateRoutedModelRouteInput {
            tenant_external_id: "routing-a".to_owned(),
            public_model: "secure-generation-model".to_owned(),
            upstream_model: "secure-upstream-model".to_owned(),
            protocol: "generation".to_owned(),
            priority: 0,
            upstream_account_ids: vec![account_a.id],
            included_provider_group_ids: Vec::new(),
            excluded_provider_group_ids: Vec::new(),
            route_group_ids: vec![route_group.id],
            route_group_names: Vec::new(),
            granted_credential_ids: vec![exact.key_id],
            custom_model_confirmed: false,
        })
        .await
        .expect("exact-credential generation route");
    let (filtered_route, filtered_routing) = state
        .db
        .create_routed_model_route(CreateRoutedModelRouteInput {
            tenant_external_id: "routing-a".to_owned(),
            public_model: "secure-filtered-model".to_owned(),
            upstream_model: "secure-upstream-model".to_owned(),
            protocol: "openai".to_owned(),
            priority: 0,
            upstream_account_ids: vec![account_a.id],
            included_provider_group_ids: vec![candidate_provider_group.id],
            excluded_provider_group_ids: vec![provider_group.id],
            route_group_ids: vec![route_group.id, secondary_route_group.id],
            route_group_names: Vec::new(),
            granted_credential_ids: vec![exact.key_id],
            custom_model_confirmed: false,
        })
        .await
        .expect("provider-group filtered route");
    assert_eq!(
        filtered_routing.candidate_upstream_account_ids,
        vec![account_a_secondary.id]
    );
    assert_eq!(filtered_routing.route_group_ids.len(), 2);
    let bridge = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: "routing-a".to_owned(),
                name: "retired CPA bridge".to_owned(),
                driver: "cpa-subscription-bridge".to_owned(),
                config: json!({
                    "base_url": "http://127.0.0.1:18083",
                    "network_scope": "private"
                }),
                credential: UpstreamCredential::None,
                oauth_session_id: None,
                oauth_driver: None,
                oauth_refresh_url: None,
            },
            pepper,
        )
        .await
        .expect("legacy bridge fixture");
    state
        .db
        .set_upstream_account_status(bridge.id, "routing-a", "disabled", bridge.updated_at)
        .await
        .expect("disable legacy bridge fixture");
    let (bridge_fallback_route, _) = state
        .db
        .create_routed_model_route(CreateRoutedModelRouteInput {
            tenant_external_id: "routing-a".to_owned(),
            public_model: "secure-bridge-fallback-model".to_owned(),
            upstream_model: "secure-upstream-model".to_owned(),
            protocol: "openai".to_owned(),
            priority: 0,
            upstream_account_ids: vec![bridge.id],
            included_provider_group_ids: vec![provider_group.id],
            excluded_provider_group_ids: Vec::new(),
            route_group_ids: vec![route_group.id],
            route_group_names: Vec::new(),
            granted_credential_ids: vec![exact.key_id],
            custom_model_confirmed: false,
        })
        .await
        .expect("legacy bridge fallback route");

    let initial_group_routing = state
        .db
        .credential_routing(group_granted.key_id, "routing-a")
        .await
        .expect("initial route-group credential routing");
    let group_routing_before = state
        .db
        .replace_credential_routing(
            group_granted.key_id,
            ReplaceCredentialRoutingInput {
                tenant_external_id: "routing-a".to_owned(),
                route_ids: Vec::new(),
                route_group_ids: vec![route_group.id, secondary_route_group.id],
                expected_grant_revision: initial_group_routing.grant_revision,
            },
        )
        .await
        .expect("grant route group to credential");
    assert!(group_routing_before.route_ids.is_empty());
    assert_eq!(group_routing_before.route_group_ids.len(), 2);
    assert!(
        group_routing_before
            .route_group_ids
            .contains(&route_group.id)
    );
    assert!(
        group_routing_before
            .route_group_ids
            .contains(&secondary_route_group.id)
    );
    assert_eq!(group_routing_before.effective_route_ids.len(), 4);
    assert_eq!(
        group_routing_before
            .effective_route_ids
            .iter()
            .filter(|id| **id == filtered_route.id)
            .count(),
        1
    );

    let exact_key = state
        .db
        .authenticate_key(&exact.key, pepper)
        .await
        .expect("authenticate exact credential");
    let classified_key = state
        .db
        .authenticate_key(&classified_only.key, pepper)
        .await
        .expect("authenticate classified-only credential");
    let group_key = state
        .db
        .authenticate_key(&group_granted.key, pepper)
        .await
        .expect("authenticate route-group credential");
    let seed = Uuid::from_u128(0x1234);
    let exact_before = state
        .db
        .resolve_authorized_upstream_with_hint(
            exact_key.key_id,
            exact_key.tenant_id,
            "secure-route-model",
            "openai",
            RouteSelectionOptions {
                upstream_account_hint: None,
                selection_seed: seed,
            },
            pepper,
        )
        .await
        .expect("resolve exact credential")
        .expect("exact credential is authorized");
    assert_eq!(exact_before.route_id, route.id);
    let generation_before = state
        .db
        .resolve_authorized_upstream_with_hint(
            exact_key.key_id,
            exact_key.tenant_id,
            "secure-generation-model",
            "generation",
            RouteSelectionOptions {
                upstream_account_hint: None,
                selection_seed: seed,
            },
            pepper,
        )
        .await
        .expect("resolve exact generation credential")
        .expect("exact credential is authorized for generation");
    assert_eq!(generation_before.route_id, generation_route.id);
    let bridge_fallback_before = state
        .db
        .resolve_authorized_upstream_with_hint(
            exact_key.key_id,
            exact_key.tenant_id,
            "secure-bridge-fallback-model",
            "openai",
            RouteSelectionOptions {
                upstream_account_hint: None,
                selection_seed: seed,
            },
            pepper,
        )
        .await
        .expect("resolve legacy bridge fallback")
        .expect("active provider-group candidate wins");
    assert_eq!(bridge_fallback_before.route_id, bridge_fallback_route.id);
    let filtered_before = state
        .db
        .resolve_authorized_upstream_with_hint(
            exact_key.key_id,
            exact_key.tenant_id,
            "secure-filtered-model",
            "openai",
            RouteSelectionOptions {
                upstream_account_hint: None,
                selection_seed: seed,
            },
            pepper,
        )
        .await
        .expect("resolve provider-group filtered route")
        .expect("non-excluded candidate remains available");
    assert_eq!(filtered_before.route_id, filtered_route.id);
    assert_eq!(filtered_before.account_id, account_a_secondary.id);
    assert_eq!(bridge_fallback_before.account_id, account_a.id);
    let bridge_history_request_id = Uuid::now_v7();
    state
        .db
        .record_request_started(NewRequest {
            request_id: bridge_history_request_id,
            key_id: exact_key.key_id,
            tenant_id: exact_key.tenant_id,
            protocol: "openai".to_owned(),
            model: "secure-bridge-fallback-model".to_owned(),
            request_object: format!("memory://routing-security/{bridge_history_request_id}"),
            reservation_id: Uuid::now_v7(),
            upstream_account_id: Some(account_a.id),
            model_route_id: Some(bridge_fallback_route.id),
        })
        .await
        .expect("record normalized bridge route history");
    let persisted_generation_before = state
        .db
        .reload_persisted_generation_upstream(
            exact_key.tenant_id,
            "secure-generation-model",
            account_a.id,
            pepper,
        )
        .await
        .expect("reload persisted generation before classification")
        .expect("persisted generation route exists");
    assert_eq!(persisted_generation_before.route_id, generation_route.id);
    assert!(
        state
            .db
            .resolve_authorized_upstream_with_hint(
                classified_key.key_id,
                classified_key.tenant_id,
                "secure-route-model",
                "openai",
                RouteSelectionOptions {
                    upstream_account_hint: None,
                    selection_seed: seed
                },
                pepper,
            )
            .await
            .expect("resolve classified-only credential before membership")
            .is_none()
    );
    let exact_models_before = state
        .db
        .granted_available_models(exact_key.key_id, exact_key.tenant_id)
        .await
        .expect("exact models before classification");
    let classified_models_before = state
        .db
        .granted_available_models(classified_key.key_id, classified_key.tenant_id)
        .await
        .expect("classified models before membership");
    let group_models_before = state
        .db
        .granted_available_models(group_key.key_id, group_key.tenant_id)
        .await
        .expect("route-group models before classification");
    assert_eq!(exact_models_before.len(), 4);
    assert!(exact_models_before.contains(&"secure-route-model".to_owned()));
    assert!(exact_models_before.contains(&"secure-generation-model".to_owned()));
    assert!(exact_models_before.contains(&"secure-bridge-fallback-model".to_owned()));
    assert!(exact_models_before.contains(&"secure-filtered-model".to_owned()));
    assert!(classified_models_before.is_empty());
    assert_eq!(group_models_before, exact_models_before);
    let exact_public_models_before = public_models(&state, &exact.key).await;
    let classified_public_models_before = public_models(&state, &classified_only.key).await;
    let group_public_models_before = public_models(&state, &group_granted.key).await;
    let exact_public_model_ids = exact_public_models_before["data"]
        .as_array()
        .expect("exact public model list")
        .iter()
        .filter_map(|model| model["id"].as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        exact_public_model_ids,
        [
            "secure-bridge-fallback-model",
            "secure-filtered-model",
            "secure-generation-model",
            "secure-route-model",
        ]
        .into_iter()
        .collect()
    );
    assert_eq!(
        classified_public_models_before["data"]
            .as_array()
            .map(Vec::len),
        Some(0)
    );
    assert_eq!(group_public_models_before, exact_public_models_before);
    let exact_routing_before = state
        .db
        .credential_routing(exact.key_id, "routing-a")
        .await
        .expect("exact routing before classification");
    assert_eq!(exact_routing_before.route_ids.len(), 4);
    assert!(exact_routing_before.route_ids.contains(&route.id));
    assert!(
        exact_routing_before
            .route_ids
            .contains(&generation_route.id)
    );
    assert!(
        exact_routing_before
            .route_ids
            .contains(&bridge_fallback_route.id)
    );
    assert!(exact_routing_before.route_ids.contains(&filtered_route.id));

    let credential_group = state
        .db
        .replace_group_members(
            GroupKind::Credential,
            credential_group.id,
            ReplaceGroupMembersInput {
                tenant_external_id: "routing-a".to_owned(),
                member_ids: vec![exact.key_id, classified_only.key_id, group_granted.key_id],
                expected_updated_at: credential_group.updated_at,
            },
        )
        .await
        .expect("classify credentials");
    let exact_after = state
        .db
        .resolve_authorized_upstream_with_hint(
            exact_key.key_id,
            exact_key.tenant_id,
            "secure-route-model",
            "openai",
            RouteSelectionOptions {
                upstream_account_hint: None,
                selection_seed: seed,
            },
            pepper,
        )
        .await
        .expect("resolve exact credential after membership")
        .expect("exact grant remains effective");
    assert_eq!(exact_after.route_id, exact_before.route_id);
    assert_eq!(exact_after.account_id, exact_before.account_id);
    let generation_after = state
        .db
        .resolve_authorized_upstream_with_hint(
            exact_key.key_id,
            exact_key.tenant_id,
            "secure-generation-model",
            "generation",
            RouteSelectionOptions {
                upstream_account_hint: None,
                selection_seed: seed,
            },
            pepper,
        )
        .await
        .expect("resolve generation after membership")
        .expect("generation grant remains effective");
    assert_eq!(generation_after.route_id, generation_before.route_id);
    assert_eq!(generation_after.account_id, generation_before.account_id);
    let bridge_fallback_after = state
        .db
        .resolve_authorized_upstream_with_hint(
            exact_key.key_id,
            exact_key.tenant_id,
            "secure-bridge-fallback-model",
            "openai",
            RouteSelectionOptions {
                upstream_account_hint: None,
                selection_seed: seed,
            },
            pepper,
        )
        .await
        .expect("resolve bridge fallback after membership")
        .expect("active provider remains available after membership");
    assert_eq!(
        bridge_fallback_after.route_id,
        bridge_fallback_before.route_id
    );
    assert_eq!(bridge_fallback_after.account_id, account_a.id);
    assert!(
        state
            .db
            .resolve_authorized_upstream_with_hint(
                classified_key.key_id,
                classified_key.tenant_id,
                "secure-route-model",
                "openai",
                RouteSelectionOptions {
                    upstream_account_hint: None,
                    selection_seed: seed
                },
                pepper,
            )
            .await
            .expect("resolve classified-only credential after membership")
            .is_none(),
        "credential-group membership must not grant a route"
    );
    assert!(
        state
            .db
            .resolve_authorized_upstream_with_hint(
                classified_key.key_id,
                classified_key.tenant_id,
                "secure-generation-model",
                "generation",
                RouteSelectionOptions {
                    upstream_account_hint: None,
                    selection_seed: seed
                },
                pepper,
            )
            .await
            .expect("resolve classified-only generation after membership")
            .is_none(),
        "credential-group membership must not grant a generation route"
    );
    assert_eq!(
        state
            .db
            .granted_available_models(exact_key.key_id, exact_key.tenant_id)
            .await
            .expect("exact models after classification"),
        exact_models_before
    );
    assert_eq!(
        state
            .db
            .granted_available_models(classified_key.key_id, classified_key.tenant_id)
            .await
            .expect("classified models after membership"),
        classified_models_before
    );
    assert_eq!(
        public_models(&state, &exact.key).await,
        exact_public_models_before
    );
    assert_eq!(
        public_models(&state, &classified_only.key).await,
        classified_public_models_before
    );
    assert_eq!(
        public_models(&state, &group_granted.key).await,
        group_public_models_before
    );
    assert_eq!(
        state
            .db
            .credential_routing(group_granted.key_id, "routing-a")
            .await
            .expect("route-group routing after classification")
            .route_group_ids,
        group_routing_before.route_group_ids
    );
    let exact_routing_after_membership = state
        .db
        .credential_routing(exact.key_id, "routing-a")
        .await
        .expect("exact routing after classification");
    assert_eq!(
        exact_routing_after_membership.route_ids,
        exact_routing_before.route_ids
    );
    let credential_group = state
        .db
        .replace_group_members(
            GroupKind::Credential,
            credential_group.id,
            ReplaceGroupMembersInput {
                tenant_external_id: "routing-a".to_owned(),
                member_ids: Vec::new(),
                expected_updated_at: credential_group.updated_at,
            },
        )
        .await
        .expect("remove credential classifications");
    let secondary_credential_group = state
        .db
        .replace_group_members(
            GroupKind::Credential,
            secondary_credential_group.id,
            ReplaceGroupMembersInput {
                tenant_external_id: "routing-a".to_owned(),
                member_ids: vec![exact.key_id, classified_only.key_id, group_granted.key_id],
                expected_updated_at: secondary_credential_group.updated_at,
            },
        )
        .await
        .expect("move credentials to another classification group");
    let exact_after_cross_group = state
        .db
        .resolve_authorized_upstream_with_hint(
            exact_key.key_id,
            exact_key.tenant_id,
            "secure-route-model",
            "openai",
            RouteSelectionOptions {
                upstream_account_hint: None,
                selection_seed: seed,
            },
            pepper,
        )
        .await
        .expect("resolve exact credential after cross-group move")
        .expect("exact route remains granted after cross-group move");
    assert_eq!(exact_after_cross_group.route_id, exact_before.route_id);
    assert_eq!(exact_after_cross_group.account_id, exact_before.account_id);
    let generation_after_cross_group = state
        .db
        .resolve_authorized_upstream_with_hint(
            exact_key.key_id,
            exact_key.tenant_id,
            "secure-generation-model",
            "generation",
            RouteSelectionOptions {
                upstream_account_hint: None,
                selection_seed: seed,
            },
            pepper,
        )
        .await
        .expect("resolve generation after cross-group move")
        .expect("generation grant remains effective after cross-group move");
    assert_eq!(
        generation_after_cross_group.route_id,
        generation_before.route_id
    );
    assert_eq!(
        generation_after_cross_group.account_id,
        generation_before.account_id
    );
    let bridge_fallback_after_cross_group = state
        .db
        .resolve_authorized_upstream_with_hint(
            exact_key.key_id,
            exact_key.tenant_id,
            "secure-bridge-fallback-model",
            "openai",
            RouteSelectionOptions {
                upstream_account_hint: None,
                selection_seed: seed,
            },
            pepper,
        )
        .await
        .expect("resolve bridge fallback after cross-group move")
        .expect("active provider remains available after cross-group move");
    assert_eq!(
        bridge_fallback_after_cross_group.route_id,
        bridge_fallback_before.route_id
    );
    assert_eq!(bridge_fallback_after_cross_group.account_id, account_a.id);
    let persisted_generation_after_cross_group = state
        .db
        .reload_persisted_generation_upstream(
            exact_key.tenant_id,
            "secure-generation-model",
            account_a.id,
            pepper,
        )
        .await
        .expect("reload persisted generation after cross-group move")
        .expect("persisted generation route remains available");
    assert_eq!(
        persisted_generation_after_cross_group.route_id,
        persisted_generation_before.route_id
    );
    assert_eq!(
        persisted_generation_after_cross_group.account_id,
        persisted_generation_before.account_id
    );
    assert_eq!(
        public_models(&state, &exact.key).await,
        exact_public_models_before
    );
    assert_eq!(
        public_models(&state, &classified_only.key).await,
        classified_public_models_before
    );
    assert_eq!(
        public_models(&state, &group_granted.key).await,
        group_public_models_before
    );
    assert_eq!(
        state
            .db
            .credential_routing(group_granted.key_id, "routing-a")
            .await
            .expect("route-group routing after cross-group move")
            .route_group_ids,
        group_routing_before.route_group_ids
    );
    assert_eq!(
        state
            .db
            .credential_routing(exact.key_id, "routing-a")
            .await
            .expect("exact routing after cross-group move")
            .route_ids,
        exact_routing_before.route_ids
    );
    state
        .db
        .replace_group_members(
            GroupKind::Credential,
            secondary_credential_group.id,
            ReplaceGroupMembersInput {
                tenant_external_id: "routing-a".to_owned(),
                member_ids: Vec::new(),
                expected_updated_at: secondary_credential_group.updated_at,
            },
        )
        .await
        .expect("remove secondary credential classifications");
    assert!(
        state
            .db
            .resolve_authorized_upstream_with_hint(
                classified_key.key_id,
                classified_key.tenant_id,
                "secure-route-model",
                "openai",
                RouteSelectionOptions {
                    upstream_account_hint: None,
                    selection_seed: seed
                },
                pepper,
            )
            .await
            .expect("resolve after classification removal")
            .is_none()
    );
    let exact_after_removal = state
        .db
        .resolve_authorized_upstream_with_hint(
            exact_key.key_id,
            exact_key.tenant_id,
            "secure-route-model",
            "openai",
            RouteSelectionOptions {
                upstream_account_hint: None,
                selection_seed: seed,
            },
            pepper,
        )
        .await
        .expect("resolve exact credential after classification removal")
        .expect("exact route remains granted after classification removal");
    assert_eq!(exact_after_removal.route_id, exact_before.route_id);
    assert_eq!(exact_after_removal.account_id, exact_before.account_id);
    let generation_after_removal = state
        .db
        .resolve_authorized_upstream_with_hint(
            exact_key.key_id,
            exact_key.tenant_id,
            "secure-generation-model",
            "generation",
            RouteSelectionOptions {
                upstream_account_hint: None,
                selection_seed: seed,
            },
            pepper,
        )
        .await
        .expect("resolve generation after classification removal")
        .expect("generation grant remains effective after classification removal");
    assert_eq!(
        generation_after_removal.route_id,
        generation_before.route_id
    );
    assert_eq!(
        generation_after_removal.account_id,
        generation_before.account_id
    );
    let bridge_fallback_after_removal = state
        .db
        .resolve_authorized_upstream_with_hint(
            exact_key.key_id,
            exact_key.tenant_id,
            "secure-bridge-fallback-model",
            "openai",
            RouteSelectionOptions {
                upstream_account_hint: None,
                selection_seed: seed,
            },
            pepper,
        )
        .await
        .expect("resolve bridge fallback after classification removal")
        .expect("active provider remains available after classification removal");
    assert_eq!(
        bridge_fallback_after_removal.route_id,
        bridge_fallback_before.route_id
    );
    assert_eq!(bridge_fallback_after_removal.account_id, account_a.id);
    let persisted_generation_after_removal = state
        .db
        .reload_persisted_generation_upstream(
            exact_key.tenant_id,
            "secure-generation-model",
            account_a.id,
            pepper,
        )
        .await
        .expect("reload persisted generation after classification removal")
        .expect("persisted generation route remains available after removal");
    assert_eq!(
        persisted_generation_after_removal.route_id,
        persisted_generation_before.route_id
    );
    assert_eq!(
        persisted_generation_after_removal.account_id,
        persisted_generation_before.account_id
    );
    assert_eq!(
        public_models(&state, &exact.key).await,
        exact_public_models_before
    );
    assert_eq!(
        public_models(&state, &classified_only.key).await,
        classified_public_models_before
    );
    assert_eq!(
        public_models(&state, &group_granted.key).await,
        group_public_models_before
    );
    assert_eq!(
        state
            .db
            .credential_routing(group_granted.key_id, "routing-a")
            .await
            .expect("route-group routing after classification removal")
            .route_group_ids,
        group_routing_before.route_group_ids
    );
    assert_eq!(
        state
            .db
            .credential_routing(exact.key_id, "routing-a")
            .await
            .expect("exact routing after classification removal")
            .route_ids,
        exact_routing_before.route_ids
    );

    let exact_before_rotation = state
        .db
        .credential_routing(exact.key_id, "routing-a")
        .await
        .expect("exact routing before rotation");
    let rotated_exact = state
        .db
        .rotate_key(exact.key_id, "routing-exact-rotation", pepper)
        .await
        .expect("rotate exact-route credential");
    assert_eq!(rotated_exact.key_id, exact.key_id);
    assert!(matches!(
        state.db.authenticate_key(&exact.key, pepper).await,
        Err(AppError::Unauthorized)
    ));
    let rotated_exact_auth = state
        .db
        .authenticate_key(&rotated_exact.key, pepper)
        .await
        .expect("authenticate rotated exact-route credential");
    let exact_after_rotation = state
        .db
        .credential_routing(rotated_exact.key_id, "routing-a")
        .await
        .expect("exact routing after rotation");
    assert_eq!(
        exact_after_rotation.route_ids,
        exact_before_rotation.route_ids
    );
    assert_eq!(
        exact_after_rotation.route_group_ids,
        exact_before_rotation.route_group_ids
    );
    assert_eq!(
        exact_after_rotation.effective_route_ids,
        exact_before_rotation.effective_route_ids
    );
    assert_eq!(
        exact_after_rotation.grant_revision,
        exact_before_rotation.grant_revision
    );
    assert_eq!(
        public_models(&state, &rotated_exact.key).await,
        exact_public_models_before
    );
    let filtered_after_rotation = state
        .db
        .resolve_authorized_upstream_with_hint(
            rotated_exact_auth.key_id,
            rotated_exact_auth.tenant_id,
            "secure-filtered-model",
            "openai",
            RouteSelectionOptions {
                upstream_account_hint: None,
                selection_seed: seed,
            },
            pepper,
        )
        .await
        .expect("resolve filtered route after credential rotation")
        .expect("rotated credential keeps the filtered route grant");
    assert_eq!(filtered_after_rotation.route_id, filtered_before.route_id);
    assert_eq!(
        filtered_after_rotation.account_id,
        filtered_before.account_id
    );

    let group_before_rotation = state
        .db
        .credential_routing(group_granted.key_id, "routing-a")
        .await
        .expect("route-group routing before rotation");
    let rotated_group = state
        .db
        .rotate_key(group_granted.key_id, "routing-group-rotation", pepper)
        .await
        .expect("rotate route-group credential");
    assert_eq!(rotated_group.key_id, group_granted.key_id);
    assert!(matches!(
        state.db.authenticate_key(&group_granted.key, pepper).await,
        Err(AppError::Unauthorized)
    ));
    state
        .db
        .authenticate_key(&rotated_group.key, pepper)
        .await
        .expect("authenticate rotated route-group credential");
    let group_after_rotation = state
        .db
        .credential_routing(rotated_group.key_id, "routing-a")
        .await
        .expect("route-group routing after rotation");
    assert_eq!(
        group_after_rotation.route_group_ids,
        group_before_rotation.route_group_ids
    );
    assert_eq!(
        group_after_rotation.effective_route_ids,
        group_before_rotation.effective_route_ids
    );
    assert_eq!(
        group_after_rotation.grant_revision,
        group_before_rotation.grant_revision
    );
    assert_eq!(
        public_models(&state, &rotated_group.key).await,
        group_public_models_before
    );

    let other_routing = state
        .db
        .credential_routing(other_tenant.key_id, "routing-b")
        .await
        .expect("other tenant routing view");
    assert!(matches!(
        state
            .db
            .replace_credential_routing(
                other_tenant.key_id,
                ReplaceCredentialRoutingInput {
                    tenant_external_id: "routing-b".to_owned(),
                    route_ids: Vec::new(),
                    route_group_ids: vec![route_group.id],
                    expected_grant_revision: other_routing.grant_revision,
                }
            )
            .await,
        Err(AppError::NotFound)
    ));
    assert!(matches!(
        state
            .db
            .create_routed_model_route(CreateRoutedModelRouteInput {
                tenant_external_id: "routing-b".to_owned(),
                public_model: "cross-provider-group".to_owned(),
                upstream_model: "cross-provider-group".to_owned(),
                protocol: "openai".to_owned(),
                priority: 0,
                upstream_account_ids: vec![account_b.id],
                included_provider_group_ids: vec![provider_group.id],
                excluded_provider_group_ids: Vec::new(),
                route_group_ids: Vec::new(),
                route_group_names: Vec::new(),
                granted_credential_ids: vec![other_tenant.key_id],
                custom_model_confirmed: false,
            })
            .await,
        Err(AppError::NotFound)
    ));
    assert!(matches!(
        state
            .db
            .replace_group_members(
                GroupKind::Credential,
                credential_group.id,
                ReplaceGroupMembersInput {
                    tenant_external_id: "routing-a".to_owned(),
                    member_ids: vec![other_tenant.key_id],
                    expected_updated_at: credential_group.updated_at,
                }
            )
            .await,
        Err(AppError::NotFound)
    ));

    for status in [
        api_json(
            &state,
            "PUT",
            format!("/internal/v1/keys/{}/routing", exact.key_id),
            json!({
                "tenant_external_id": "routing-a",
                "route_ids": [],
                "route_group_ids": [],
                "credential_group_ids": [credential_group.id],
                "expected_grant_revision": 0
            }),
        )
        .await,
        api_json(
            &state,
            "POST",
            "/internal/v1/model-routes".to_owned(),
            json!({
                "tenant_external_id": "routing-a",
                "public_model": "forbidden-credential-group-route",
                "upstream_account_ids": [account_a.id],
                "upstream_model": "forbidden-credential-group-route",
                "protocol": "openai",
                "granted_credential_group_ids": [credential_group.id]
            }),
        )
        .await,
    ] {
        assert!(
            matches!(
                status,
                StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY
            ),
            "credential groups must be rejected by routing request schemas"
        );
    }
    let route_group_cross_status = api_json(
        &state,
        "PUT",
        format!("/internal/v1/keys/{}/routing", other_tenant.key_id),
        json!({
            "tenant_external_id": "routing-b",
            "route_ids": [],
            "route_group_ids": [route_group.id],
            "expected_grant_revision": other_routing.grant_revision
        }),
    )
    .await;
    assert_eq!(route_group_cross_status, StatusCode::NOT_FOUND);
    let provider_group_cross_status = api_json(
        &state,
        "POST",
        "/internal/v1/model-routes".to_owned(),
        json!({
            "tenant_external_id": "routing-b",
            "public_model": "api-cross-provider-group",
            "upstream_account_ids": [account_b.id],
            "upstream_model": "api-cross-provider-group",
            "protocol": "openai",
            "included_provider_group_ids": [provider_group.id],
            "granted_credential_ids": [other_tenant.key_id]
        }),
    )
    .await;
    assert_eq!(provider_group_cross_status, StatusCode::NOT_FOUND);
    let credential_group_cross_status = api_json(
        &state,
        "PUT",
        format!(
            "/internal/v1/credential-groups/{}/members",
            credential_group.id
        ),
        json!({
            "tenant_external_id": "routing-a",
            "member_ids": [other_tenant.key_id],
            "expected_updated_at": credential_group.updated_at
        }),
    )
    .await;
    assert_eq!(credential_group_cross_status, StatusCode::NOT_FOUND);

    sqlx::any::install_default_drivers();
    let inspection = AnyPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .expect("routing security inspection pool");
    if backend == "sqlite" {
        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&inspection)
            .await
            .expect("enable inspection foreign keys");
    }
    let columns = if backend == "postgres" {
        sqlx::query("SELECT CAST(column_name AS TEXT) AS name FROM information_schema.columns WHERE table_schema = current_schema() AND table_name = 'routing_grants'")
            .fetch_all(&inspection)
            .await
            .expect("PostgreSQL routing grant columns")
    } else {
        sqlx::query("PRAGMA table_info(routing_grants)")
            .fetch_all(&inspection)
            .await
            .expect("SQLite routing grant columns")
    };
    assert!(columns.iter().all(|row| {
        row.try_get::<String, _>("name")
            .expect("routing grant column")
            != "credential_group_id"
    }));
    let tenant_id: String = sqlx::query_scalar("SELECT id FROM tenants WHERE external_id = $1")
        .bind("routing-a")
        .fetch_one(&inspection)
        .await
        .expect("routing A tenant id");
    let legacy_bridge_account: String =
        sqlx::query_scalar("SELECT upstream_account_id FROM model_routes WHERE id = $1")
            .bind(bridge_fallback_route.id.to_string())
            .fetch_one(&inspection)
            .await
            .expect("legacy bridge compatibility account");
    assert_eq!(legacy_bridge_account, bridge.id.to_string());
    let history = sqlx::query(
        "SELECT model_route_id, upstream_account_id FROM request_records WHERE id = $1",
    )
    .bind(bridge_history_request_id.to_string())
    .fetch_one(&inspection)
    .await
    .expect("normalized bridge route history");
    assert_eq!(
        history
            .try_get::<Option<String>, _>("model_route_id")
            .expect("history route id"),
        Some(bridge_fallback_route.id.to_string())
    );
    assert_eq!(
        history
            .try_get::<Option<String>, _>("upstream_account_id")
            .expect("history upstream account id"),
        Some(account_a.id.to_string())
    );
    assert!(
        sqlx::query("INSERT INTO routing_grants (tenant_id, key_id, model_route_id, route_group_id, created_at) VALUES ($1, $2, $3, NULL, 1)")
            .bind(&tenant_id)
            .bind(credential_group.id.to_string())
            .bind(route.id.to_string())
            .execute(&inspection)
            .await
            .is_err(),
        "a credential group cannot be the routing-grant subject"
    );
    assert!(
        sqlx::query("INSERT INTO routing_grants (tenant_id, key_id, model_route_id, route_group_id, created_at) VALUES ($1, $2, NULL, $3, 1)")
            .bind(&tenant_id)
            .bind(classified_only.key_id.to_string())
            .bind(credential_group.id.to_string())
            .execute(&inspection)
            .await
            .is_err(),
        "a credential group cannot be the routing-grant target"
    );
}

#[tokio::test]
async fn credential_groups_are_classification_only_and_cross_tenant_group_ids_fail_closed() {
    let directory = tempfile::tempdir().expect("routing security directory");
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("routing-security.db").display()
    );
    exercise_group_routing_security(database_url, "sqlite").await;
}

#[tokio::test]
async fn postgres_group_routing_security_is_enforced_or_explicitly_skipped() {
    let database_url = match std::env::var("MTC_TEST_POSTGRES_URL") {
        Ok(value) => value,
        Err(_)
            if std::env::var_os("MTC_REQUIRE_POSTGRES_SECURITY").is_some()
                || std::env::var_os("CI").is_some() =>
        {
            panic!(
                "PostgreSQL routing security gate required but MTC_TEST_POSTGRES_URL is unset; this is not a passing PostgreSQL result"
            )
        }
        Err(_) => {
            eprintln!(
                "SECURITY_GATE_SKIPPED backend=postgres gate=routing_groups reason=MTC_TEST_POSTGRES_URL_unset (set MTC_REQUIRE_POSTGRES_SECURITY=1 in release CI)"
            );
            return;
        }
    };
    exercise_group_routing_security(database_url, "postgres").await;
}
