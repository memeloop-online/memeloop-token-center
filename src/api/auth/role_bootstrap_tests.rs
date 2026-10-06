use super::*;
use crate::{
    api::{router_for_role, tests::test_state},
    config::RuntimeRole,
    db::{CreateKeyInput, CreateServiceTokenInput},
    model::KeyPolicy,
};
use rust_decimal::Decimal;
use std::sync::Arc;
use tower::ServiceExt;

fn authenticated_get(path: &str, token: &str) -> Request {
    Request::get(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn bootstrap_requires_an_explicit_control_serving_role() {
    let (mut state, _directory) = test_state().await;
    for configured in [
        "isolated-custom-bootstrap-token-at-least-32-bytes",
        DISABLED_BOOTSTRAP_SERVICE_TOKEN,
    ] {
        Arc::make_mut(&mut state.config).service_token = configured.to_owned();
        for role in [None, Some(RuntimeRole::Gateway), Some(RuntimeRole::Worker)] {
            state.service_auth_role = role;
            let request = authenticated_get("/metrics", configured);
            assert!(matches!(
                authenticated_service(request.headers(), &state).await,
                Err(AppError::Unauthorized)
            ));
        }
    }
}

#[tokio::test]
async fn disabled_sentinel_cannot_bootstrap_even_on_control_or_all() {
    let (mut state, _directory) = test_state().await;
    Arc::make_mut(&mut state.config).service_token = DISABLED_BOOTSTRAP_SERVICE_TOKEN.to_owned();
    for role in [RuntimeRole::Gateway, RuntimeRole::Control, RuntimeRole::All] {
        let application = router_for_role(state.clone(), role);
        let response = application
            .clone()
            .oneshot(authenticated_get(
                "/metrics",
                DISABLED_BOOTSTRAP_SERVICE_TOKEN,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let response = application
            .oneshot(authenticated_get(
                "/internal/v1/keys",
                DISABLED_BOOTSTRAP_SERVICE_TOKEN,
            ))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            if role.serves_control() {
                StatusCode::UNAUTHORIZED
            } else {
                StatusCode::NOT_FOUND
            }
        );
    }
}

#[tokio::test]
async fn router_role_is_local_and_control_bootstrap_remains_valid() {
    let (state, _directory) = test_state().await;
    let token = state.config.service_token.clone();
    let gateway = router_for_role(state.clone(), RuntimeRole::Gateway);
    for role in [RuntimeRole::Control, RuntimeRole::All] {
        let application = router_for_role(state.clone(), role);
        for path in ["/metrics", "/internal/v1/keys"] {
            let response = application
                .clone()
                .oneshot(authenticated_get(path, &token))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }
        let response = gateway
            .clone()
            .oneshot(authenticated_get("/metrics", &token))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    let worker = router_for_role(state.clone(), RuntimeRole::Worker);
    let response = worker
        .oneshot(authenticated_get("/metrics", &token))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(state.service_auth_role, None);
}

#[tokio::test]
async fn gateway_scoped_metrics_identity_is_not_promoted_to_bootstrap() {
    let (mut state, _directory) = test_state().await;
    let issued = state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: "isolated-gateway-metrics".to_owned(),
                scopes: vec!["metrics:read".to_owned()],
                tenant_external_id: None,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    Arc::make_mut(&mut state.config).service_token = issued.token.clone();
    state.service_auth_role = Some(RuntimeRole::Gateway);
    let request = authenticated_get("/metrics", &issued.token);
    let identity = authenticated_service(request.headers(), &state)
        .await
        .unwrap();
    assert_eq!(identity.service_id, Some(issued.service_id));
    assert!(identity.allows("metrics:read"));
    assert!(!identity.allows("keys:write"));
    for configured in [issued.token.as_str(), DISABLED_BOOTSTRAP_SERVICE_TOKEN] {
        Arc::make_mut(&mut state.config).service_token = configured.to_owned();
        let response = router_for_role(state.clone(), RuntimeRole::Gateway)
            .oneshot(authenticated_get("/metrics", &issued.token))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
    state
        .db
        .set_service_token_status(issued.service_id, "revoked")
        .await
        .unwrap();
    let response = router_for_role(state, RuntimeRole::Gateway)
        .oneshot(authenticated_get("/metrics", &issued.token))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn gateway_metrics_still_requires_active_metrics_scope() {
    let (state, _directory) = test_state().await;
    let issued = state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: "isolated-non-metrics-service".to_owned(),
                scopes: vec!["keys:read".to_owned()],
                tenant_external_id: None,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let application = router_for_role(state.clone(), RuntimeRole::Gateway);
    let response = application
        .clone()
        .oneshot(authenticated_get("/metrics", &issued.token))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = application
        .oneshot(authenticated_get("/metrics", "invalid-service-credential"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn gateway_client_key_authentication_does_not_use_bootstrap() {
    let (mut state, _directory) = test_state().await;
    Arc::make_mut(&mut state.config).service_token = DISABLED_BOOTSTRAP_SERVICE_TOKEN.to_owned();
    let issued = state
        .db
        .create_key(
            CreateKeyInput {
                tenant_external_id: "isolated-role-client".to_owned(),
                principal_external_id: "isolated-role-client".to_owned(),
                alias: "isolated-role-client".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy::default(),
                initial_balance: Decimal::ZERO,
                idempotency_key: None,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let application = router_for_role(state, RuntimeRole::Gateway);
    let response = application
        .clone()
        .oneshot(authenticated_get("/v1/models", &issued.key))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = application
        .oneshot(authenticated_get(
            "/v1/models",
            DISABLED_BOOTSTRAP_SERVICE_TOKEN,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[test]
fn chart_disabled_token_is_the_reserved_non_authenticating_value() {
    assert!(
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/charts/memeloop-token-center/templates/deployment.yaml"
        ))
        .contains(&format!("value: \"{DISABLED_BOOTSTRAP_SERVICE_TOKEN}\""))
    );
}
