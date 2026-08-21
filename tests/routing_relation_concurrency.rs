use memeloop_token_center::{
    AppState,
    config::Config,
    db::{
        CreateGroupInput, CreateKeyInput, CreateRoutedModelRouteInput, CreateUpstreamAccountInput,
        GroupKind, ReplaceCredentialRoutingInput, ReplaceGroupMembersInput,
        ReplaceRouteRoutingInput,
    },
    error::AppError,
    model::KeyPolicy,
    provider::UpstreamCredential,
};
use rust_decimal::Decimal;
use serde_json::json;
use uuid::Uuid;

async fn exercise_relation_cas(database_url: String, tenant: &str) {
    let state = AppState::initialize(Config::for_test(database_url))
        .await
        .expect("routing relation CAS state");
    let pepper = state.config.key_pepper.as_bytes();
    let account = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: tenant.to_owned(),
                name: "relation-cas-upstream".to_owned(),
                driver: "http-json".to_owned(),
                config: json!({
                    "base_url": "http://127.0.0.1:18081",
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
        .expect("relation CAS upstream");
    let credential = state
        .db
        .create_key(
            CreateKeyInput {
                tenant_external_id: tenant.to_owned(),
                principal_external_id: "relation-cas-principal".to_owned(),
                alias: "relation-cas-credential".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy::default(),
                initial_balance: Decimal::TEN,
                idempotency_key: None,
            },
            pepper,
        )
        .await
        .expect("relation CAS credential");
    let route_group = state
        .db
        .create_group(
            GroupKind::Route,
            CreateGroupInput {
                tenant_external_id: tenant.to_owned(),
                name: "relation-cas-route-group".to_owned(),
            },
        )
        .await
        .expect("relation CAS route group");
    let (route, _) = state
        .db
        .create_routed_model_route(CreateRoutedModelRouteInput {
            tenant_external_id: tenant.to_owned(),
            public_model: "relation-cas-public-model".to_owned(),
            upstream_model: "relation-cas-custom-model".to_owned(),
            protocol: "openai".to_owned(),
            priority: 0,
            upstream_account_ids: vec![account.id],
            included_provider_group_ids: Vec::new(),
            excluded_provider_group_ids: Vec::new(),
            route_group_ids: vec![route_group.id],
            route_group_names: Vec::new(),
            granted_credential_ids: Vec::new(),
            custom_model_confirmed: true,
        })
        .await
        .expect("relation CAS route");

    // Both editors start from the same empty exact-grant edge. Whichever full
    // replacement commits first bumps the reverse relation revision, fencing
    // the other editor's stale snapshot.
    let route_before_exact = state
        .db
        .route_routing(route.id, tenant)
        .await
        .expect("route before exact grant race");
    let credential_before_exact = state
        .db
        .credential_routing(credential.key_id, tenant)
        .await
        .expect("credential before exact grant race");
    let credential_edit = state.db.replace_credential_routing(
        credential.key_id,
        ReplaceCredentialRoutingInput {
            tenant_external_id: tenant.to_owned(),
            route_ids: vec![route.id],
            route_group_ids: Vec::new(),
            expected_grant_revision: credential_before_exact.grant_revision,
        },
    );
    let route_edit = state.db.replace_route_routing(
        route.id,
        ReplaceRouteRoutingInput {
            tenant_external_id: tenant.to_owned(),
            upstream_account_ids: vec![account.id],
            included_provider_group_ids: Vec::new(),
            excluded_provider_group_ids: Vec::new(),
            route_group_ids: vec![route_group.id],
            route_group_names: Vec::new(),
            granted_credential_ids: vec![credential.key_id],
            expected_updated_at: route_before_exact.updated_at,
            expected_grant_revision: route_before_exact.grant_revision,
            custom_model_confirmed: true,
        },
    );
    let (credential_result, route_result) = tokio::join!(credential_edit, route_edit);
    assert_ne!(
        credential_result.is_ok(),
        route_result.is_ok(),
        "exact-grant editors must have exactly one winner"
    );
    assert!(
        matches!(credential_result, Err(AppError::Conflict(_)))
            || matches!(route_result, Err(AppError::Conflict(_))),
        "the stale exact-grant editor must receive Conflict"
    );
    let route_after_exact = state
        .db
        .route_routing(route.id, tenant)
        .await
        .expect("route after exact grant race");
    let credential_after_exact = state
        .db
        .credential_routing(credential.key_id, tenant)
        .await
        .expect("credential after exact grant race");
    assert_eq!(
        route_after_exact.granted_credential_ids,
        [credential.key_id]
    );
    assert_eq!(credential_after_exact.route_ids, [route.id]);

    // A byte-for-byte equivalent credential routing replay is a validated
    // no-op: neither side's direct-grant revision advances.
    let route_revision_before_noop = route_after_exact.grant_revision;
    let credential_revision_before_noop = credential_after_exact.grant_revision;
    let credential_after_noop = state
        .db
        .replace_credential_routing(
            credential.key_id,
            ReplaceCredentialRoutingInput {
                tenant_external_id: tenant.to_owned(),
                route_ids: credential_after_exact.route_ids.clone(),
                route_group_ids: credential_after_exact.route_group_ids.clone(),
                expected_grant_revision: credential_revision_before_noop,
            },
        )
        .await
        .expect("identical credential routing replay");
    assert_eq!(
        credential_after_noop.grant_revision,
        credential_revision_before_noop
    );
    assert_eq!(
        state
            .db
            .route_routing(route.id, tenant)
            .await
            .expect("route after credential no-op")
            .grant_revision,
        route_revision_before_noop
    );

    // Route and GroupManager both replace the complete membership edge set.
    // Removing the same edge from a shared snapshot must fence one editor.
    let route_before_membership = state
        .db
        .route_routing(route.id, tenant)
        .await
        .expect("route before membership race");
    let group_before_membership = state
        .db
        .list_groups(GroupKind::Route, tenant)
        .await
        .expect("group before membership race")
        .into_iter()
        .find(|group| group.id == route_group.id)
        .expect("route group view before race");
    let route_membership_edit = state.db.replace_route_routing(
        route.id,
        ReplaceRouteRoutingInput {
            tenant_external_id: tenant.to_owned(),
            upstream_account_ids: vec![account.id],
            included_provider_group_ids: Vec::new(),
            excluded_provider_group_ids: Vec::new(),
            route_group_ids: Vec::new(),
            route_group_names: Vec::new(),
            granted_credential_ids: vec![credential.key_id],
            expected_updated_at: route_before_membership.updated_at,
            expected_grant_revision: route_before_membership.grant_revision,
            custom_model_confirmed: true,
        },
    );
    let group_membership_edit = state.db.replace_group_members(
        GroupKind::Route,
        route_group.id,
        ReplaceGroupMembersInput {
            tenant_external_id: tenant.to_owned(),
            member_ids: Vec::new(),
            expected_updated_at: group_before_membership.updated_at,
        },
    );
    let (route_result, group_result) = tokio::join!(route_membership_edit, group_membership_edit);
    assert_ne!(
        route_result.is_ok(),
        group_result.is_ok(),
        "route membership editors must have exactly one winner"
    );
    assert!(
        matches!(route_result, Err(AppError::Conflict(_)))
            || matches!(group_result, Err(AppError::Conflict(_))),
        "the stale route membership editor must receive Conflict"
    );
    assert!(
        !state
            .db
            .route_routing(route.id, tenant)
            .await
            .expect("route after membership race")
            .route_group_ids
            .contains(&route_group.id),
        "the winning removal must not be silently overwritten"
    );
}

#[tokio::test]
async fn sqlite_relation_editors_are_cas_safe() {
    let directory = tempfile::tempdir().expect("routing relation CAS directory");
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("routing-relation-cas.db").display()
    );
    exercise_relation_cas(database_url, "routing-relation-sqlite").await;
}

#[tokio::test]
async fn postgres_relation_editors_are_cas_safe_when_configured() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let tenant = format!("routing-relation-postgres-{}", Uuid::now_v7());
    exercise_relation_cas(database_url, &tenant).await;
}
