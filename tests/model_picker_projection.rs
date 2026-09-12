use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use memeloop_token_center::{
    AppState, api,
    config::{Config, RuntimeRole},
    db::{
        CreateGroupInput, CreateRoutedModelRouteInput, CreateServiceTokenInput,
        CreateUpstreamAccountInput, DiscoveredUpstreamModel, GroupKind,
        ModelPickerProjectionFilter, ModelPickerSelectionIdentity, ModelPickerSelectionKind,
        ReplaceGroupMembersInput, ReplaceModelCatalogResult,
    },
    provider::UpstreamCredential,
};
use serde_json::{Value, json};
use sqlx::AnyPool;
use tower::ServiceExt;
use uuid::Uuid;

async fn sqlite_state(label: &str) -> (AppState, tempfile::TempDir, String) {
    let directory = tempfile::tempdir().expect("model picker temporary directory");
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join(format!("{label}.db")).display()
    );
    let state = AppState::initialize(Config::for_test(database_url.clone()))
        .await
        .expect("initialize model picker state");
    (state, directory, database_url)
}

async fn create_account(state: &AppState, tenant: &str, name: &str, driver: &str) -> Uuid {
    state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: tenant.to_owned(),
                name: name.to_owned(),
                driver: driver.to_owned(),
                config: json!({"base_url": "https://example.com"}),
                credential: UpstreamCredential::None,
                oauth_session_id: None,
                oauth_driver: None,
                oauth_refresh_url: None,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .expect("create model picker account")
        .id
}

struct RouteFixtureInput<'a> {
    public_model: &'a str,
    upstream_model: &'a str,
    protocol: &'a str,
    priority: i64,
    account_ids: Vec<Uuid>,
    included_provider_group_ids: Vec<Uuid>,
}

async fn create_route(state: &AppState, tenant: &str, input: RouteFixtureInput<'_>) -> Uuid {
    state
        .db
        .create_routed_model_route(CreateRoutedModelRouteInput {
            tenant_external_id: tenant.to_owned(),
            public_model: input.public_model.to_owned(),
            upstream_model: input.upstream_model.to_owned(),
            protocol: input.protocol.to_owned(),
            priority: input.priority,
            enabled: true,
            upstream_account_ids: input.account_ids,
            included_provider_group_ids: input.included_provider_group_ids,
            excluded_provider_group_ids: Vec::new(),
            route_group_ids: Vec::new(),
            route_group_names: Vec::new(),
            granted_credential_ids: Vec::new(),
            custom_model_confirmed: true,
        })
        .await
        .expect("create model picker route")
        .0
        .id
}

async fn publish_catalog(state: &AppState, tenant: &str, account_id: Uuid, upstream_model: &str) {
    let lease = Uuid::now_v7();
    assert!(
        state
            .db
            .claim_upstream_model_catalog_sync(account_id, tenant, 1, lease)
            .await
            .expect("claim catalog")
    );
    assert_eq!(
        state
            .db
            .replace_upstream_model_catalog(
                account_id,
                tenant,
                1,
                lease,
                "openai_v1",
                &[DiscoveredUpstreamModel {
                    model_id: upstream_model.to_owned(),
                    protocol: "any".to_owned(),
                    context_window: Some(128_000),
                    reservation_token_bound: Some(128_000),
                    reservation_bound_source: Some("mtc_context_window_bound".to_owned()),
                }],
            )
            .await
            .expect("publish catalog"),
        ReplaceModelCatalogResult::Replaced
    );
}

struct ProjectionFixture {
    tenant: String,
    first_account: Uuid,
    second_account: Uuid,
    third_account: Uuid,
    fourth_account: Uuid,
    first_route: Uuid,
    second_route: Uuid,
    provider_group: Uuid,
}

async fn seed_projection(state: &AppState, label: &str) -> ProjectionFixture {
    let tenant = format!("picker-{label}-{}", Uuid::now_v7());
    let other_tenant = format!("picker-other-{label}-{}", Uuid::now_v7());
    let first_account = create_account(state, &tenant, "Alpha account", "http-json").await;
    let second_account = create_account(state, &tenant, "Beta account", "http-json").await;
    let third_account = create_account(state, &tenant, "Gamma 账号", "http-json").await;
    let fourth_account = create_account(state, &tenant, "Delta account", "http-json").await;
    let hidden_account =
        create_account(state, &tenant, "Hidden retired account", "retired-hidden").await;
    let other_account =
        create_account(state, &other_tenant, "Other tenant account", "http-json").await;

    let provider_group = state
        .db
        .create_group(
            GroupKind::Provider,
            CreateGroupInput {
                tenant_external_id: tenant.clone(),
                name: "Research providers".to_owned(),
            },
        )
        .await
        .expect("create provider group");
    state
        .db
        .replace_group_members(
            GroupKind::Provider,
            provider_group.id,
            ReplaceGroupMembersInput {
                tenant_external_id: tenant.clone(),
                member_ids: vec![first_account, second_account],
                expected_updated_at: provider_group.updated_at,
            },
        )
        .await
        .expect("populate provider group");

    let first_route = create_route(
        state,
        &tenant,
        RouteFixtureInput {
            public_model: "shared-model",
            upstream_model: "native-shared",
            protocol: "openai",
            priority: 0,
            account_ids: vec![first_account, second_account],
            included_provider_group_ids: vec![provider_group.id],
        },
    )
    .await;
    let second_route = create_route(
        state,
        &tenant,
        RouteFixtureInput {
            public_model: "shared-model",
            upstream_model: "native-shared",
            protocol: "anthropic",
            priority: 1,
            account_ids: vec![third_account, fourth_account],
            included_provider_group_ids: Vec::new(),
        },
    )
    .await;
    create_route(
        state,
        &tenant,
        RouteFixtureInput {
            public_model: "hidden-internal-model",
            upstream_model: "hidden-internal-model",
            protocol: "openai",
            priority: 0,
            account_ids: vec![hidden_account],
            included_provider_group_ids: Vec::new(),
        },
    )
    .await;
    create_route(
        state,
        &other_tenant,
        RouteFixtureInput {
            public_model: "other-tenant-model",
            upstream_model: "other-tenant-model",
            protocol: "openai",
            priority: 0,
            account_ids: vec![other_account],
            included_provider_group_ids: Vec::new(),
        },
    )
    .await;

    publish_catalog(state, &tenant, first_account, "native-shared").await;
    publish_catalog(state, &tenant, second_account, "native-shared").await;
    publish_catalog(state, &tenant, fourth_account, "native-shared").await;
    let rotated = state
        .db
        .rotate_upstream_credential(
            fourth_account,
            UpstreamCredential::None,
            &format!("picker-stale-generation-{fourth_account}"),
            state.config.key_pepper.as_bytes(),
        )
        .await
        .expect("rotate account after catalog snapshot");
    assert_eq!(rotated.credential_generation, 2);
    let second_failure = Uuid::now_v7();
    assert!(
        state
            .db
            .claim_upstream_model_catalog_sync(second_account, &tenant, 1, second_failure)
            .await
            .expect("claim retained-snapshot refresh")
    );
    assert_eq!(
        state
            .db
            .record_upstream_model_catalog_failure(
                second_account,
                &tenant,
                1,
                second_failure,
                "rate_limited",
            )
            .await
            .expect("record retained-snapshot failure"),
        ReplaceModelCatalogResult::Replaced
    );
    let third_failure = Uuid::now_v7();
    assert!(
        state
            .db
            .claim_upstream_model_catalog_sync(third_account, &tenant, 1, third_failure)
            .await
            .expect("claim first catalog refresh")
    );
    assert_eq!(
        state
            .db
            .record_upstream_model_catalog_failure(
                third_account,
                &tenant,
                1,
                third_failure,
                "invalid_response",
            )
            .await
            .expect("record first catalog failure"),
        ReplaceModelCatalogResult::Replaced
    );

    ProjectionFixture {
        tenant,
        first_account,
        second_account,
        third_account,
        fourth_account,
        first_route,
        second_route,
        provider_group: provider_group.id,
    }
}

fn projection_filter<'a>(
    fixture: &'a ProjectionFixture,
    selection_kind: ModelPickerSelectionKind,
    query: &'a str,
    accounts: &'a [Uuid],
    public_provider_ids: &'a [String],
) -> ModelPickerProjectionFilter<'a> {
    ModelPickerProjectionFilter {
        tenant_external_id: &fixture.tenant,
        selection_kind,
        query,
        after_sort_label: "",
        after_identity: "",
        explicit_account_ids: accounts,
        included_provider_group_ids: &[],
        excluded_provider_group_ids: &[],
        public_provider_ids,
        limit: 100,
    }
}

async fn exercise_database_projection(state: &AppState, label: &str, database_url: &str) {
    let fixture = seed_projection(state, label).await;
    let inspection = AnyPool::connect(database_url)
        .await
        .expect("connect model picker fixture inspection pool");
    sqlx::query(
        "UPDATE model_route_upstream_accounts
            SET upstream_model = $1, catalog_policy = 'required'
          WHERE model_route_id = $2 AND upstream_account_id = $3",
    )
    .bind("historical-direct-model")
    .bind(fixture.first_route.to_string())
    .bind(fixture.first_account.to_string())
    .execute(&inspection)
    .await
    .expect("stage historical direct/group model disagreement");
    inspection.close().await;
    let public_provider_ids = vec!["http-json".to_owned()];
    let route_items = state
        .db
        .model_picker_projection(projection_filter(
            &fixture,
            ModelPickerSelectionKind::Route,
            "",
            &[],
            &public_provider_ids,
        ))
        .await
        .expect("route projection");
    assert_eq!(route_items.len(), 2);
    assert!(route_items.iter().all(|item| item.label == "shared-model"));
    let route_ids = route_items
        .iter()
        .map(|item| match &item.selection {
            ModelPickerSelectionIdentity::Route { route_id } => *route_id,
            ModelPickerSelectionIdentity::Model { .. } => panic!("expected route identity"),
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        route_ids,
        [fixture.first_route, fixture.second_route]
            .into_iter()
            .collect()
    );

    let model_items = state
        .db
        .model_picker_projection(projection_filter(
            &fixture,
            ModelPickerSelectionKind::Model,
            "",
            &[],
            &public_provider_ids,
        ))
        .await
        .expect("model projection");
    assert_eq!(model_items.len(), 1);
    assert_eq!(model_items[0].value, "shared-model");
    assert_eq!(model_items[0].sources.len(), 4);
    assert!(!model_items[0].sources_truncated);
    assert!(model_items[0].sources.iter().all(|source| {
        source.provider.id == "http-json" && source.passive_health.status == "unknown"
    }));
    let catalogs = model_items[0]
        .sources
        .iter()
        .map(|source| (source.account.id.clone(), source.catalog.status.as_str()))
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(catalogs[&fixture.first_account.to_string()], "ready");
    assert_eq!(catalogs[&fixture.second_account.to_string()], "partial");
    assert_eq!(catalogs[&fixture.third_account.to_string()], "error");
    assert_eq!(catalogs[&fixture.fourth_account.to_string()], "stale");
    let historical_direct_source = model_items[0]
        .sources
        .iter()
        .find(|source| source.account.id == fixture.first_account.to_string())
        .expect("historical direct source");
    assert_eq!(
        historical_direct_source.capabilities.upstream_model, "historical-direct-model",
        "direct assignment wins over group expansion for the same route/account"
    );
    assert_eq!(
        historical_direct_source.configuration_availability.status,
        "unavailable"
    );
    assert!(
        historical_direct_source
            .configuration_availability
            .reasons
            .iter()
            .any(|reason| reason == "route_candidate_ineligible"),
        "eligibility for the group's different upstream model must not leak to the direct source"
    );
    assert!(
        model_items[0]
            .sources
            .iter()
            .find(|source| source.account.id == fixture.first_account.to_string())
            .expect("first source")
            .provider_groups
            .iter()
            .any(|group| group.id == fixture.provider_group.to_string())
    );
    assert!(model_items[0].sources.iter().all(|source| {
        source
            .provider_groups
            .iter()
            .filter(|group| group.id == fixture.provider_group.to_string())
            .count()
            <= 1
    }));

    let searched = state
        .db
        .model_picker_projection(projection_filter(
            &fixture,
            ModelPickerSelectionKind::Model,
            "GAMMA 账号",
            &[],
            &public_provider_ids,
        ))
        .await
        .expect("account-label search");
    assert_eq!(searched.len(), 1);
    assert_eq!(
        searched[0].sources.len(),
        4,
        "search selects logical items, then returns all sources"
    );

    let selected_account = [fixture.first_account];
    let filtered = state
        .db
        .model_picker_projection(projection_filter(
            &fixture,
            ModelPickerSelectionKind::Model,
            "",
            &selected_account,
            &public_provider_ids,
        ))
        .await
        .expect("account-filtered projection");
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].sources.len(), 1);
    assert_eq!(
        filtered[0].sources[0].account.id,
        fixture.first_account.to_string()
    );

    let included_provider_groups = [fixture.provider_group];
    let mut group_filter = projection_filter(
        &fixture,
        ModelPickerSelectionKind::Model,
        "",
        &[],
        &public_provider_ids,
    );
    group_filter.included_provider_group_ids = &included_provider_groups;
    let included = state
        .db
        .model_picker_projection(group_filter)
        .await
        .expect("included provider-group projection");
    assert_eq!(included.len(), 1);
    assert_eq!(included[0].sources.len(), 2);

    let mut exclusion_filter = projection_filter(
        &fixture,
        ModelPickerSelectionKind::Model,
        "",
        &[],
        &public_provider_ids,
    );
    exclusion_filter.excluded_provider_group_ids = &included_provider_groups;
    let excluded = state
        .db
        .model_picker_projection(exclusion_filter)
        .await
        .expect("excluded provider-group projection");
    assert_eq!(excluded.len(), 1);
    assert_eq!(excluded[0].sources.len(), 2);
    assert!(excluded[0].sources.iter().all(|source| {
        source.account.id != fixture.first_account.to_string()
            && source.account.id != fixture.second_account.to_string()
    }));
}

#[tokio::test]
async fn sqlite_projection_preserves_identity_aggregation_and_evidence() {
    let (state, _directory, database_url) = sqlite_state("projection").await;
    exercise_database_projection(&state, "sqlite", &database_url).await;
}

#[tokio::test]
async fn postgres_projection_matches_sqlite_contract() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        eprintln!("MTC_TEST_POSTGRES_URL is unset; skipping PostgreSQL model picker contract");
        return;
    };
    let state = AppState::initialize(Config::for_test(database_url.clone()))
        .await
        .expect("initialize PostgreSQL model picker state");
    exercise_database_projection(&state, "postgres", &database_url).await;
}

async fn request(
    state: &AppState,
    role: RuntimeRole,
    token: &str,
    uri: &str,
) -> (StatusCode, Value) {
    let response = api::router_for_role(state.clone(), role)
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .expect("model picker request"),
        )
        .await
        .expect("model picker response");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .expect("read model picker response");
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("model picker JSON")
    };
    (status, value)
}

#[tokio::test]
async fn operator_api_enforces_scopes_tenant_cursor_and_control_role() {
    let (state, _directory, _database_url) = sqlite_state("api").await;
    let fixture = seed_projection(&state, "api").await;
    let allowed = state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: "model picker reader".to_owned(),
                scopes: vec!["routes:read".to_owned(), "providers:read".to_owned()],
                tenant_external_id: Some(fixture.tenant.clone()),
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .expect("create picker reader");
    let routes_only = state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: "model picker routes-only reader".to_owned(),
                scopes: vec!["routes:read".to_owned()],
                tenant_external_id: Some(fixture.tenant.clone()),
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .expect("create routes-only reader");
    let base = format!(
        "/internal/v1/model-picker-options?tenant_external_id={}&selection_kind=route&limit=1",
        fixture.tenant
    );

    let (status, first) = request(&state, RuntimeRole::Control, &allowed.token, &base).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["contract_version"], "model_picker_projection_v1");
    assert_eq!(first["data"].as_array().expect("first page").len(), 1);
    assert_eq!(
        first["data"][0]["sources"][0]["provider"]["label"],
        "HTTP JSON upstream"
    );
    assert_eq!(
        first["data"][0]["sources"][0]["passive_health"]["status"],
        "unknown"
    );
    let cursor = first["next_cursor"].as_str().expect("next cursor");
    let (status, second) = request(
        &state,
        RuntimeRole::Control,
        &allowed.token,
        &format!("{base}&cursor={cursor}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_eq!(second["data"].as_array().expect("second page").len(), 1);
    assert_ne!(
        first["data"][0]["selection"],
        second["data"][0]["selection"]
    );

    let (status, _) = request(
        &state,
        RuntimeRole::Control,
        &allowed.token,
        &format!("{base}&q=different&cursor={cursor}"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = request(
        &state,
        RuntimeRole::Control,
        &allowed.token,
        &format!(
            "{base}&account_ids={}&cursor={cursor}",
            fixture.first_account
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = request(&state, RuntimeRole::Control, &routes_only.token, &base).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = request(&state, RuntimeRole::Control, "not-a-service-token", &base).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = request(
        &state,
        RuntimeRole::Control,
        &allowed.token,
        "/internal/v1/model-picker-options?tenant_external_id=another-tenant&selection_kind=model",
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = request(&state, RuntimeRole::Gateway, &allowed.token, &base).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
