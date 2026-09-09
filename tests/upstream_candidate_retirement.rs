use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use memeloop_token_center::{
    AppState, api,
    config::{Config, RuntimeRole},
    db::{
        CreateGroupInput, CreateRoutedModelRouteInput, CreateServiceTokenInput,
        CreateUpstreamAccountInput, GroupKind, ReplaceGroupMembersInput,
    },
    provider::UpstreamCredential,
};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

async fn post(state: &AppState, route: Uuid, token: &str, body: Value) -> StatusCode {
    api::router_for_role(state.clone(), RuntimeRole::Control)
        .oneshot(
            Request::post(format!(
                "/internal/v1/model-routes/{route}/retire-upstreams"
            ))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

async fn token(state: &AppState, tenant: &str, scopes: &[&str]) -> String {
    state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: Uuid::now_v7().to_string(),
                scopes: scopes.iter().map(|s| (*s).to_owned()).collect(),
                tenant_external_id: Some(tenant.to_owned()),
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap()
        .token
}

async fn exercise(database_url: String) {
    let state = AppState::initialize(Config::for_test(database_url))
        .await
        .unwrap();
    let tenant = format!("retirement-http-{}", Uuid::now_v7());
    let other_tenant = format!("retirement-other-{}", Uuid::now_v7());
    let account = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: tenant.clone(),
                name: "retirement-http".into(),
                driver: "http-json".into(),
                config: json!({"base_url":"http://127.0.0.1:1"}),
                credential: UpstreamCredential::None,
                oauth_session_id: None,
                oauth_driver: None,
                oauth_refresh_url: None,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let group = state
        .db
        .create_group(
            GroupKind::Provider,
            CreateGroupInput {
                tenant_external_id: tenant.clone(),
                name: "included-retirement".into(),
            },
        )
        .await
        .unwrap();
    state
        .db
        .replace_group_members(
            GroupKind::Provider,
            group.id,
            ReplaceGroupMembersInput {
                tenant_external_id: tenant.clone(),
                member_ids: vec![account.id],
                expected_updated_at: group.updated_at,
            },
        )
        .await
        .unwrap();
    let (route, _) = state
        .db
        .create_routed_model_route(CreateRoutedModelRouteInput {
            tenant_external_id: tenant.clone(),
            public_model: "retirement-http".into(),
            upstream_model: "retirement-http".into(),
            protocol: "openai".into(),
            priority: 0,
            enabled: false,
            upstream_account_ids: vec![account.id],
            included_provider_group_ids: vec![group.id],
            excluded_provider_group_ids: vec![],
            route_group_ids: vec![],
            route_group_names: vec![],
            granted_credential_ids: vec![],
            custom_model_confirmed: true,
        })
        .await
        .unwrap();
    state
        .db
        .set_upstream_account_status(account.id, &tenant, "disabled", account.updated_at)
        .await
        .unwrap();
    let before = state.db.route_routing(route.id, &tenant).await.unwrap();
    let body = json!({"tenant_external_id":tenant, "upstream_account_ids":[account.id],
        "expected_updated_at":before.updated_at, "expected_grant_revision":before.grant_revision});
    for scopes in [
        &["routes:write"][..],
        &["providers:write"][..],
        &["routes:read", "providers:read"][..],
    ] {
        let restricted = token(&state, &tenant, scopes).await;
        assert_eq!(
            post(&state, route.id, &restricted, body.clone()).await,
            StatusCode::FORBIDDEN
        );
    }
    let full = token(&state, &tenant, &["routes:write", "providers:write"]).await;
    let mut foreign_body = body.clone();
    foreign_body["tenant_external_id"] = json!(other_tenant);
    assert_eq!(
        post(&state, route.id, &full, foreign_body).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        post(&state, Uuid::now_v7(), &full, body.clone()).await,
        StatusCode::NOT_FOUND
    );
    // A dual-authorized caller still cannot erase an explicit candidate while
    // that same account remains selected through an included provider group.
    assert_eq!(
        post(&state, route.id, &full, body.clone()).await,
        StatusCode::CONFLICT
    );
    let after = state.db.route_routing(route.id, &tenant).await.unwrap();
    assert_eq!(after.upstream_account_ids, before.upstream_account_ids);
    assert_eq!(
        after.included_provider_group_ids,
        before.included_provider_group_ids
    );
    assert_eq!(after.grant_revision, before.grant_revision);
    assert_eq!(after.updated_at, before.updated_at);
    let current_group = state
        .db
        .list_groups(GroupKind::Provider, &tenant)
        .await
        .unwrap()
        .into_iter()
        .find(|item| item.id == group.id)
        .unwrap();
    state
        .db
        .replace_group_members(
            GroupKind::Provider,
            group.id,
            ReplaceGroupMembersInput {
                tenant_external_id: tenant.clone(),
                member_ids: vec![],
                expected_updated_at: current_group.updated_at,
            },
        )
        .await
        .unwrap();
    let current = state.db.route_routing(route.id, &tenant).await.unwrap();
    assert_eq!(post(&state, route.id, &full, json!({
        "tenant_external_id":tenant, "upstream_account_ids":[account.id],
        "expected_updated_at":current.updated_at, "expected_grant_revision":current.grant_revision,
    })).await, StatusCode::CONFLICT, "an empty included group is still outside this contract");
}

#[tokio::test]
async fn retirement_http_requires_dual_scopes_tenant_and_group_review() {
    let directory = tempfile::tempdir().unwrap();
    exercise(format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("retirement.db").display()
    ))
    .await;
}

#[tokio::test]
async fn postgres_retirement_http_contract_when_configured() {
    if let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") {
        exercise(url).await;
    }
}
