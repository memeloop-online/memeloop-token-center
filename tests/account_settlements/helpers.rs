use axum::{
    body::{Body, to_bytes},
    http::{HeaderMap, Request, StatusCode, header},
};
use memeloop_token_center::{
    AppState, api,
    config::RuntimeRole,
    db::{CreateServiceTokenInput, FinishProxyRequest, NewRequest},
    model::{AuthenticatedKey, ModelPrice, TokenUsage},
};
use serde_json::Value;
use tower::ServiceExt;
use uuid::Uuid;

pub(super) async fn service_token(
    state: &AppState,
    name: &str,
    scopes: Vec<&str>,
    tenant_external_id: Option<String>,
) -> String {
    state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: name.to_owned(),
                scopes: scopes.into_iter().map(str::to_owned).collect(),
                tenant_external_id,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .expect("create settlement service token")
        .token
}

pub(super) async fn create_text_settlement(
    state: &AppState,
    key: &AuthenticatedKey,
    price: &ModelPrice,
    model: &str,
    request_object: &str,
    response_object: &str,
) -> Uuid {
    let reservation = state
        .db
        .reserve_usage(key, price, 2, 1)
        .await
        .expect("reserve text usage");
    let request_id = Uuid::now_v7();
    state
        .db
        .record_request_started(NewRequest {
            request_id,
            key_id: key.key_id,
            tenant_id: key.tenant_id,
            protocol: "openai-chat".to_owned(),
            model: model.to_owned(),
            request_object: request_object.to_owned(),
            reservation_id: reservation.id,
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .expect("record text request start");
    let result = state
        .db
        .finish_proxy_request(FinishProxyRequest {
            request_id,
            tenant_id: key.tenant_id,
            reservation: &reservation,
            input_token_ceiling: 2,
            output_token_ceiling: 1,
            requested_service_tier: None,
            status_code: 200,
            duration_ms: 12,
            usage: TokenUsage {
                input_tokens: 2,
                output_tokens: 1,
                ..TokenUsage::default()
            },
            charge_contract_ceiling: false,
            error_code: None,
            response_object,
            conversation: None,
        })
        .await
        .expect("finish text request");
    assert!(matches!(
        result,
        memeloop_token_center::db::FinishProxyRequestResult::Finished {
            cost_micros: 3_000_000,
            ..
        }
    ));
    request_id
}

pub(super) async fn get_json(
    state: &AppState,
    path: &str,
    token: &str,
) -> (StatusCode, HeaderMap, Value) {
    let response = api::router_for_role(state.clone(), RuntimeRole::Control)
        .oneshot(
            Request::get(path)
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .expect("build settlement request"),
        )
        .await
        .expect("settlement HTTP response");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("bounded settlement response");
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("JSON settlement response")
    };
    (status, headers, body)
}

pub(super) fn assert_sanitized(body: &Value) {
    let encoded = body.to_string();
    for forbidden in [
        "request_object",
        "response_object",
        "reservation_id",
        "request-body-sentinel",
        "response-body-sentinel",
    ] {
        assert!(
            body.get(forbidden).is_none(),
            "response exposed {forbidden}"
        );
        assert!(
            !encoded.contains(forbidden),
            "response body contains forbidden value {forbidden}"
        );
    }
    for item in body["items"].as_array().into_iter().flatten() {
        let encoded = item.to_string();
        for forbidden in ["request_object", "response_object", "reservation_id"] {
            assert!(item.get(forbidden).is_none(), "item exposed {forbidden}");
            assert!(!encoded.contains(forbidden));
        }
    }
}
