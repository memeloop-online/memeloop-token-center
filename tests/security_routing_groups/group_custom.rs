use super::*;
use memeloop_token_center::db::{
    RouteCreateDisposition, RouteCreateIdempotencyKey, UpdateRoutedModelRouteInput,
};

pub(super) async fn group(state: &AppState, tenant: &str, name: &str) -> Uuid {
    state
        .db
        .create_group(
            GroupKind::Provider,
            CreateGroupInput {
                tenant_external_id: tenant.to_owned(),
                name: name.to_owned(),
            },
        )
        .await
        .unwrap()
        .id
}

pub(super) async fn members(
    state: &AppState,
    pool: &sqlx::AnyPool,
    tenant: &str,
    group_id: Uuid,
    member_ids: Vec<Uuid>,
) {
    let expected_updated_at =
        sqlx::query_scalar("SELECT updated_at FROM provider_groups WHERE id = $1")
            .bind(group_id.to_string())
            .fetch_one(pool)
            .await
            .unwrap();
    state
        .db
        .replace_group_members(
            GroupKind::Provider,
            group_id,
            ReplaceGroupMembersInput {
                tenant_external_id: tenant.to_owned(),
                member_ids,
                expected_updated_at,
            },
        )
        .await
        .unwrap();
}

fn models(protocol: &str) -> Vec<DiscoveredUpstreamModel> {
    vec![DiscoveredUpstreamModel {
        model_id: "group-private-model".to_owned(),
        protocol: protocol.to_owned(),
        context_window: None,
        reservation_token_bound: None,
        reservation_bound_source: None,
    }]
}

async fn publish(
    state: &AppState,
    tenant: &str,
    account_id: Uuid,
    models: &[DiscoveredUpstreamModel],
) {
    let lease = Uuid::now_v7();
    assert!(
        state
            .db
            .claim_upstream_model_catalog_sync(account_id, tenant, 1, lease)
            .await
            .unwrap()
    );
    assert_eq!(
        state
            .db
            .replace_upstream_model_catalog(account_id, tenant, 1, lease, "openai_v1", models)
            .await
            .unwrap(),
        ReplaceModelCatalogResult::Replaced
    );
}

async fn candidates(state: &AppState, tenant: &str, route: Uuid) -> Vec<Uuid> {
    let mut accounts = state
        .db
        .route_routing(route, tenant)
        .await
        .unwrap()
        .candidate_upstream_account_ids;
    accounts.sort_unstable();
    accounts
}

async fn evidence_count(pool: &sqlx::AnyPool, route: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM model_route_group_catalog_evidence WHERE model_route_id = $1",
    )
    .bind(route.to_string())
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn exercise(database_url: String) {
    let state = AppState::initialize(Config::for_test(database_url.clone()))
        .await
        .unwrap();
    let pool = AnyPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .unwrap();
    let tenant = format!("group-custom-{}", Uuid::now_v7());
    let pepper = state.config.key_pepper.as_bytes();
    let known = catalog_account(&state, &tenant, "discovered", "http-json").await;
    let private = catalog_account(&state, &tenant, "never-listed", "http-json").await;
    let included = group(&state, &tenant, "dynamic").await;
    let excluded = group(&state, &tenant, "excluded").await;
    members(&state, &pool, &tenant, included, vec![known, private]).await;
    let key = state
        .db
        .create_key(key_input(&tenant, "allowed"), pepper)
        .await
        .unwrap();
    let denied = state
        .db
        .create_key(key_input(&tenant, "ungranted"), pepper)
        .await
        .unwrap();
    let before_config: String =
        sqlx::query_scalar("SELECT config_json FROM upstream_accounts WHERE id = $1")
            .bind(known.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    let input = CreateRoutedModelRouteInput {
        tenant_external_id: tenant.clone(),
        public_model: "secure-route-model".to_owned(),
        upstream_model: "group-private-model".to_owned(),
        protocol: "openai".to_owned(),
        priority: 0,
        enabled: true,
        upstream_account_ids: vec![],
        included_provider_group_ids: vec![included],
        excluded_provider_group_ids: vec![excluded],
        route_group_ids: vec![],
        route_group_names: vec![],
        granted_credential_ids: vec![key.key_id],
        custom_model_confirmed: true,
    };
    let mut required = input.clone();
    required.custom_model_confirmed = false;
    assert!(matches!(
        state.db.create_routed_model_route(required).await,
        Err(AppError::BadRequest(_))
    ));
    let operation = RouteCreateIdempotencyKey::from_hmac_sha256([42; 32]);
    let created = state
        .db
        .create_routed_model_route_with_idempotency(input.clone(), Some(operation.clone()))
        .await
        .unwrap();
    let route = created.route.id;
    assert_eq!(created.disposition, RouteCreateDisposition::Created);
    assert!(created.routing.custom_model_confirmed);
    assert!(created.routing.upstream_account_ids.is_empty());
    assert_eq!(created.routing.included_provider_group_ids, vec![included]);
    let replay = state
        .db
        .create_routed_model_route_with_idempotency(input.clone(), Some(operation.clone()))
        .await
        .unwrap();
    assert_eq!(replay.disposition, RouteCreateDisposition::Reused);
    assert_eq!(replay.route.id, route);
    let equivalent = state
        .db
        .create_routed_model_route_with_idempotency(input.clone(), None)
        .await
        .unwrap();
    assert_eq!(
        equivalent.disposition,
        RouteCreateDisposition::EquivalentUnowned
    );
    assert_eq!(equivalent.route.id, route);
    let mut changed = input.clone();
    changed.custom_model_confirmed = false;
    assert!(matches!(
        state
            .db
            .create_routed_model_route_with_idempotency(changed, Some(operation))
            .await,
        Err(AppError::Conflict(_))
    ));
    let authenticated = state.db.authenticate_key(&key.key, pepper).await.unwrap();
    for (key_id, expected) in [(key.key_id, 2), (denied.key_id, 0)] {
        let resolved = state
            .db
            .resolve_authorized_upstream_candidates_with_hint(
                key_id,
                authenticated.tenant_id,
                &input.public_model,
                "openai",
                RouteSelectionOptions {
                    upstream_account_hint: None,
                    avoid_route_account: None,
                    selection_seed: Uuid::now_v7(),
                },
                pepper,
            )
            .await
            .unwrap();
        assert_eq!(resolved.len(), expected);
    }
    let listed = api::router(state.clone())
        .oneshot(
            Request::get(format!(
                "/internal/v1/model-routes?tenant_external_id={tenant}"
            ))
            .header(
                header::AUTHORIZATION,
                format!("Bearer {}", state.config.service_token),
            )
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(listed.status(), StatusCode::OK);
    let listing: Value =
        serde_json::from_slice(&to_bytes(listed.into_body(), 1024 * 1024).await.unwrap()).unwrap();
    let listed_route = listing
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == route.to_string())
        .unwrap();
    assert_eq!(listed_route["custom_model_confirmed"], true);

    let lease = Uuid::now_v7();
    assert!(
        state
            .db
            .claim_upstream_model_catalog_sync(known, &tenant, 1, lease)
            .await
            .unwrap()
    );
    assert_eq!(
        state
            .db
            .replace_upstream_model_catalog(known, &tenant, 2, lease, "openai_v1", &models("any"))
            .await
            .unwrap(),
        ReplaceModelCatalogResult::CredentialGenerationChanged
    );
    assert_eq!(
        state
            .db
            .replace_upstream_model_catalog(
                known,
                &tenant,
                1,
                Uuid::now_v7(),
                "openai_v1",
                &models("any")
            )
            .await
            .unwrap(),
        ReplaceModelCatalogResult::LeaseChanged
    );
    assert_eq!(
        state
            .db
            .record_upstream_model_catalog_failure(known, &tenant, 1, lease, "connection_failed")
            .await
            .unwrap(),
        ReplaceModelCatalogResult::Replaced
    );
    assert_eq!(evidence_count(&pool, route).await, 0);
    assert_eq!(candidates(&state, &tenant, route).await.len(), 2);
    publish(&state, &tenant, private, &models("anthropic")).await;
    assert_eq!(evidence_count(&pool, route).await, 0);
    publish(&state, &tenant, known, &models("any")).await;
    assert_eq!(evidence_count(&pool, route).await, 1);
    publish(&state, &tenant, known, &[]).await;
    publish(&state, &tenant, known, &[]).await;
    assert_eq!(candidates(&state, &tenant, route).await, vec![private]);
    assert_eq!(evidence_count(&pool, route).await, 1);
    let old_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM upstream_models WHERE upstream_account_id = $1")
            .bind(known.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(old_rows, 0);
    members(&state, &pool, &tenant, included, vec![private]).await;
    members(&state, &pool, &tenant, included, vec![known, private]).await;
    assert_eq!(candidates(&state, &tenant, route).await, vec![private]);

    let routing = state.db.route_routing(route, &tenant).await.unwrap();
    let update = UpdateRoutedModelRouteInput {
        tenant_external_id: tenant.clone(),
        public_model: input.public_model.clone(),
        upstream_model: input.upstream_model.clone(),
        protocol: input.protocol.clone(),
        priority: 1,
        upstream_account_ids: vec![],
        included_provider_group_ids: vec![included],
        excluded_provider_group_ids: vec![excluded],
        route_group_ids: vec![],
        route_group_names: vec![],
        granted_credential_ids: vec![key.key_id],
        expected_updated_at: routing.updated_at,
        expected_grant_revision: routing.grant_revision,
        custom_model_confirmed: true,
    };
    let (updated, readback) = state
        .db
        .update_routed_model_route(route, update.clone())
        .await
        .unwrap();
    assert!(readback.custom_model_confirmed);
    assert_eq!(readback.grant_revision, routing.grant_revision);
    assert_eq!(evidence_count(&pool, route).await, 1);
    let (replayed, _) = state
        .db
        .update_routed_model_route(route, update.clone())
        .await
        .unwrap();
    assert_eq!(replayed.updated_at, updated.updated_at);
    let mut stale = update;
    stale.priority = 2;
    assert!(matches!(
        state.db.update_routed_model_route(route, stale).await,
        Err(AppError::Conflict(_))
    ));
    assert_eq!(candidates(&state, &tenant, route).await, vec![private]);
    members(&state, &pool, &tenant, excluded, vec![private]).await;
    assert!(candidates(&state, &tenant, route).await.is_empty());
    members(&state, &pool, &tenant, excluded, vec![]).await;
    let added = catalog_account(&state, &tenant, "new-member", "http-json").await;
    members(&state, &pool, &tenant, included, vec![private, added]).await;
    assert_eq!(candidates(&state, &tenant, route).await.len(), 2);
    members(&state, &pool, &tenant, included, vec![known]).await;
    assert!(candidates(&state, &tenant, route).await.is_empty());
    publish(&state, &tenant, known, &models("any")).await;
    assert_eq!(candidates(&state, &tenant, route).await, vec![known]);
    let after_config: String =
        sqlx::query_scalar("SELECT config_json FROM upstream_accounts WHERE id = $1")
            .bind(known.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(before_config, after_config);
    let direct: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM model_route_upstream_accounts WHERE model_route_id = $1",
    )
    .bind(route.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(direct, 0);
    assert!(matches!(
        state.db.model_price(&input.upstream_model, "USD").await,
        Err(AppError::UnpricedModel)
    ));

    let other_tenant = format!("other-{tenant}");
    let foreign = catalog_account(&state, &other_tenant, "foreign", "http-json").await;
    let foreign_group = group(&state, &other_tenant, "foreign").await;
    members(&state, &pool, &other_tenant, foreign_group, vec![foreign]).await;
    let mut cross_tenant = input.clone();
    cross_tenant.included_provider_group_ids = vec![foreign_group];
    assert!(matches!(
        state.db.create_routed_model_route(cross_tenant).await,
        Err(AppError::NotFound)
    ));
    assert!(sqlx::query("INSERT INTO model_route_group_catalog_evidence (tenant_id, model_route_id, upstream_account_id, upstream_model, protocol, discovered_at) VALUES ($1, $2, $3, 'group-private-model', 'openai', 0)")
        .bind(authenticated.tenant_id.to_string()).bind(route.to_string()).bind(foreign.to_string())
        .execute(&pool).await.is_err());
    sqlx::query("UPDATE upstream_credentials SET revoked_at = 1 WHERE upstream_account_id = $1")
        .bind(known.to_string())
        .execute(&pool)
        .await
        .unwrap();
    let resolved = state
        .db
        .resolve_authorized_upstream_candidates_with_hint(
            key.key_id,
            authenticated.tenant_id,
            &input.public_model,
            "openai",
            RouteSelectionOptions {
                upstream_account_hint: None,
                avoid_route_account: None,
                selection_seed: Uuid::now_v7(),
            },
            pepper,
        )
        .await
        .unwrap();
    assert!(resolved.is_empty());
    pool.close().await;
}

#[tokio::test]
async fn sqlite_group_custom_dynamic_discovery_generation_readback_and_grants() {
    let directory = tempfile::tempdir().unwrap();
    exercise(format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("group-custom.db").display()
    ))
    .await;
}

#[tokio::test]
async fn postgres_group_custom_dynamic_discovery_generation_readback_and_grants() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        assert!(
            std::env::var_os("CI").is_none(),
            "PostgreSQL group-custom CI must not silently skip"
        );
        return;
    };
    exercise(database_url).await;
}
