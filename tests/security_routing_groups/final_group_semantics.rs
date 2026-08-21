use super::*;

fn filtered_key_input(principal: &str) -> CreateKeyInput {
    let mut input = key_input("final-routing-a", principal);
    input.policy.allowed_models = vec!["secure-filtered-model".to_owned()];
    input
}

async fn publish_catalog(state: &AppState, account_id: Uuid, generation: i64) {
    let lease = Uuid::now_v7();
    assert!(
        state
            .db
            .claim_upstream_model_catalog_sync(account_id, "final-routing-a", generation, lease,)
            .await
            .expect("claim final routing catalog")
    );
    assert_eq!(
        state
            .db
            .replace_upstream_model_catalog(
                account_id,
                "final-routing-a",
                generation,
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
            .expect("publish final routing catalog"),
        ReplaceModelCatalogResult::Replaced
    );
}

async fn assert_rotation_preserves_route(
    state: &AppState,
    key_id: Uuid,
    old_key: &str,
    expected_route_id: Uuid,
    expected_account_id: Uuid,
) {
    let pepper = state.config.key_pepper.as_bytes();
    let before = state
        .db
        .credential_routing(key_id, "final-routing-a")
        .await
        .expect("routing before rotation");
    let models_before = public_models(state, old_key).await;
    let rotated = state
        .db
        .rotate_key(key_id, format!("rotation-{key_id}"), pepper)
        .await
        .expect("rotate credential without changing its identity");

    assert_eq!(rotated.key_id, key_id);
    assert!(matches!(
        state.db.authenticate_key(old_key, pepper).await,
        Err(AppError::Unauthorized)
    ));
    let authenticated = state
        .db
        .authenticate_key(&rotated.key, pepper)
        .await
        .expect("authenticate rotated credential");
    let after = state
        .db
        .credential_routing(key_id, "final-routing-a")
        .await
        .expect("routing after rotation");
    assert_eq!(after.route_ids, before.route_ids);
    assert_eq!(after.route_group_ids, before.route_group_ids);
    assert_eq!(after.effective_route_ids, before.effective_route_ids);
    assert_eq!(after.grant_revision, before.grant_revision);
    assert_eq!(public_models(state, &rotated.key).await, models_before);

    let selected = state
        .db
        .resolve_authorized_upstream_with_hint(
            authenticated.key_id,
            authenticated.tenant_id,
            "secure-filtered-model",
            "openai",
            RouteSelectionOptions {
                upstream_account_hint: None,
                selection_seed: Uuid::from_u128(0x9876),
            },
            pepper,
        )
        .await
        .expect("resolve route after rotation")
        .expect("rotated credential retains its route authorization");
    assert_eq!(selected.route_id, expected_route_id);
    assert_eq!(selected.account_id, expected_account_id);
}

async fn exercise_final_group_semantics(database_url: String, backend: &str) {
    let state = AppState::initialize(Config::for_test(database_url))
        .await
        .unwrap_or_else(|error| panic!("initialize {backend} final routing state: {error}"));
    let pepper = state.config.key_pepper.as_bytes();
    let exact = state
        .db
        .create_key(filtered_key_input("final-exact"), pepper)
        .await
        .expect("directly granted credential");
    let grouped = state
        .db
        .create_key(filtered_key_input("final-grouped"), pepper)
        .await
        .expect("route-group credential");

    let primary = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: "final-routing-a".to_owned(),
                name: "final-primary".to_owned(),
                driver: "http-json".to_owned(),
                config: json!({"base_url": "http://127.0.0.1:18101", "network_scope": "private"}),
                credential: UpstreamCredential::None,
                oauth_session_id: None,
                oauth_driver: None,
                oauth_refresh_url: None,
            },
            pepper,
        )
        .await
        .expect("primary provider");
    let secondary = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: "final-routing-a".to_owned(),
                name: "final-secondary".to_owned(),
                driver: "http-json".to_owned(),
                config: json!({"base_url": "http://127.0.0.1:18102", "network_scope": "private"}),
                credential: UpstreamCredential::None,
                oauth_session_id: None,
                oauth_driver: None,
                oauth_refresh_url: None,
            },
            pepper,
        )
        .await
        .expect("secondary provider");
    publish_catalog(&state, primary.id, primary.credential_generation).await;
    publish_catalog(&state, secondary.id, secondary.credential_generation).await;

    let included = state
        .db
        .create_group(
            GroupKind::Provider,
            CreateGroupInput {
                tenant_external_id: "final-routing-a".to_owned(),
                name: "candidate providers".to_owned(),
            },
        )
        .await
        .expect("candidate provider group");
    let included = state
        .db
        .replace_group_members(
            GroupKind::Provider,
            included.id,
            ReplaceGroupMembersInput {
                tenant_external_id: "final-routing-a".to_owned(),
                member_ids: vec![primary.id, secondary.id],
                expected_updated_at: included.updated_at,
            },
        )
        .await
        .expect("candidate provider members");
    let excluded = state
        .db
        .create_group(
            GroupKind::Provider,
            CreateGroupInput {
                tenant_external_id: "final-routing-a".to_owned(),
                name: "excluded providers".to_owned(),
            },
        )
        .await
        .expect("excluded provider group");
    let excluded = state
        .db
        .replace_group_members(
            GroupKind::Provider,
            excluded.id,
            ReplaceGroupMembersInput {
                tenant_external_id: "final-routing-a".to_owned(),
                member_ids: vec![primary.id],
                expected_updated_at: excluded.updated_at,
            },
        )
        .await
        .expect("excluded provider members");

    let route_group_a = state
        .db
        .create_group(
            GroupKind::Route,
            CreateGroupInput {
                tenant_external_id: "final-routing-a".to_owned(),
                name: "route group A".to_owned(),
            },
        )
        .await
        .expect("route group A");
    let route_group_b = state
        .db
        .create_group(
            GroupKind::Route,
            CreateGroupInput {
                tenant_external_id: "final-routing-a".to_owned(),
                name: "route group B".to_owned(),
            },
        )
        .await
        .expect("route group B");
    let (route, routing) = state
        .db
        .create_routed_model_route(CreateRoutedModelRouteInput {
            tenant_external_id: "final-routing-a".to_owned(),
            public_model: "secure-filtered-model".to_owned(),
            upstream_model: "secure-upstream-model".to_owned(),
            protocol: "openai".to_owned(),
            priority: 0,
            upstream_account_ids: vec![primary.id],
            included_provider_group_ids: vec![included.id],
            excluded_provider_group_ids: vec![excluded.id],
            route_group_ids: vec![route_group_a.id, route_group_b.id],
            route_group_names: Vec::new(),
            granted_credential_ids: vec![exact.key_id],
            custom_model_confirmed: false,
        })
        .await
        .expect("filtered route");
    assert_eq!(routing.candidate_upstream_account_ids, vec![secondary.id]);
    assert_eq!(routing.route_group_ids.len(), 2);

    let initial = state
        .db
        .credential_routing(grouped.key_id, "final-routing-a")
        .await
        .expect("initial group routing");
    let grouped_routing = state
        .db
        .replace_credential_routing(
            grouped.key_id,
            ReplaceCredentialRoutingInput {
                tenant_external_id: "final-routing-a".to_owned(),
                route_ids: Vec::new(),
                route_group_ids: vec![route_group_a.id, route_group_b.id],
                expected_grant_revision: initial.grant_revision,
            },
        )
        .await
        .expect("grant both route groups");
    assert_eq!(grouped_routing.effective_route_ids, vec![route.id]);

    for credential in [&exact, &grouped] {
        let authenticated = state
            .db
            .authenticate_key(&credential.key, pepper)
            .await
            .expect("authenticate routed credential");
        let selected = state
            .db
            .resolve_authorized_upstream_with_hint(
                authenticated.key_id,
                authenticated.tenant_id,
                "secure-filtered-model",
                "openai",
                RouteSelectionOptions {
                    upstream_account_hint: None,
                    selection_seed: Uuid::from_u128(0x9876),
                },
                pepper,
            )
            .await
            .expect("resolve provider-group filtered route")
            .expect("one non-excluded provider remains");
        assert_eq!(selected.route_id, route.id);
        assert_eq!(selected.account_id, secondary.id);
    }

    assert_rotation_preserves_route(&state, exact.key_id, &exact.key, route.id, secondary.id).await;
    assert_rotation_preserves_route(&state, grouped.key_id, &grouped.key, route.id, secondary.id)
        .await;
}

#[tokio::test]
async fn provider_and_route_groups_compose_and_survive_rotation_on_sqlite() {
    let directory = tempfile::tempdir().expect("final routing directory");
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("final-routing.db").display()
    );
    exercise_final_group_semantics(database_url, "sqlite").await;
}

#[tokio::test]
async fn provider_and_route_groups_compose_and_survive_rotation_on_postgres_or_skip() {
    let database_url = match std::env::var("MTC_TEST_POSTGRES_URL") {
        Ok(value) => value,
        Err(_)
            if std::env::var_os("MTC_REQUIRE_POSTGRES_SECURITY").is_some()
                || std::env::var_os("CI").is_some() =>
        {
            panic!("PostgreSQL final routing gate requires MTC_TEST_POSTGRES_URL")
        }
        Err(_) => {
            eprintln!(
                "SECURITY_GATE_SKIPPED backend=postgres gate=final_group_semantics reason=MTC_TEST_POSTGRES_URL_unset"
            );
            return;
        }
    };
    exercise_final_group_semantics(database_url, "postgres").await;
}
