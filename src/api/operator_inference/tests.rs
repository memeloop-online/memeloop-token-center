use super::*;
use crate::{
    config::RuntimeRole,
    db::{CreateKeyInput, CreateServiceTokenInput},
    model::KeyPolicy,
};
use tower::ServiceExt;

fn inference(token: &str, key_id: Uuid) -> Request {
    Request::post("/internal/v1/inference/chat/completions")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({"key_id":key_id,"request":{"model":"fixture","messages":[{"role":"user","content":"hello"}],"stream":false}}).to_string()))
        .unwrap()
}

#[tokio::test]
async fn inference_requires_explicit_key_copy_permission_and_tenant() {
    let (state, _directory) = crate::api::tests::test_state().await;
    let key = state
        .db
        .create_key(
            CreateKeyInput {
                tenant_external_id: "inference-owner".into(),
                principal_external_id: "owner".into(),
                alias: "billing".into(),
                currency: "USD".into(),
                policy: KeyPolicy::default(),
                initial_balance: Decimal::ZERO,
                idempotency_key: None,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    state
        .db
        .create_tenant("inference-other", None)
        .await
        .unwrap();
    for (name, scopes, tenant) in [
        ("reader", vec!["keys:read".to_owned()], "inference-owner"),
        (
            "other-writer",
            vec!["keys:write".to_owned()],
            "inference-other",
        ),
    ] {
        let service = state
            .db
            .create_service_token(
                CreateServiceTokenInput {
                    name: name.into(),
                    scopes,
                    tenant_external_id: Some(tenant.into()),
                },
                state.config.key_pepper.as_bytes(),
            )
            .await
            .unwrap();
        let response = router_for_role(state.clone(), RuntimeRole::Control)
            .oneshot(inference(&service.token, key.key_id))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{name}");
        let bytes = axum::body::to_bytes(response.into_body(), 8192)
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains(&key.key));
    }
    let writer = state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: "owner-writer".into(),
                scopes: vec!["keys:write".into()],
                tenant_external_id: Some("inference-owner".into()),
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    // Copy authority does not add any downstream model grants.
    let response = router_for_role(state.clone(), RuntimeRole::Control)
        .oneshot(inference(&writer.token, key.key_id))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    // An active tenant query cannot override a scoped service identity.
    let mut request = inference(&writer.token, key.key_id);
    *request.uri_mut() =
        "/internal/v1/inference/chat/completions?tenant_external_id=inference-other"
            .parse()
            .unwrap();
    let response = router_for_role(state.clone(), RuntimeRole::Control)
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    // Global authority is narrowed by the UI's explicit active tenant too.
    let mut request = inference(&state.config.service_token, key.key_id);
    *request.uri_mut() =
        "/internal/v1/inference/chat/completions?tenant_external_id=inference-other"
            .parse()
            .unwrap();
    let response = router_for_role(state.clone(), RuntimeRole::Control)
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    // Invalid requests are rejected before a nonexistent key is copied.
    let mut request = inference(&writer.token, Uuid::now_v7());
    *request.body_mut() =
        Body::from(json!({"key_id":Uuid::now_v7(),"request":{"stream":true}}).to_string());
    let response = router_for_role(state.clone(), RuntimeRole::Control)
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    state
        .db
        .set_key_status(key.key_id, "suspended")
        .await
        .unwrap();
    let response = router_for_role(state.clone(), RuntimeRole::Control)
        .oneshot(inference(&writer.token, key.key_id))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    // A downstream key is never accepted as operator authorization.
    let response = router_for_role(state, RuntimeRole::Control)
        .oneshot(inference(&key.key, key.key_id))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[test]
fn operator_bridge_requires_nonstreaming_json() {
    assert!(validate_request(&json!({"stream":true})).is_err());
    assert!(validate_request(&json!({"stream":"false"})).is_err());
    assert!(validate_request(&json!([])).is_err());
    assert!(validate_request(&json!({"stream":false})).is_ok());
    assert!(validate_request(&json!({})).is_ok());
}
