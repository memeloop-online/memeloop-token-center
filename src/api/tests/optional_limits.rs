use super::*;
use axum::http::Method;
use axum::response::IntoResponse;

#[tokio::test]
async fn prepaid_rate_rejections_remain_distinct_from_service_and_database_failures() {
    use crate::error::{AppError, LimitReason};

    for reason in [
        LimitReason::RpmExhausted,
        LimitReason::TpmExhausted,
        LimitReason::ConcurrencyExhausted,
        LimitReason::BalanceExhausted,
    ] {
        let response = AppError::LimitExceeded {
            reason,
            retry_after_seconds: Some(1),
        }
        .into_response();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["error"]["reason"], reason.as_str());
        assert_eq!(
            body["error"]["code"],
            if reason == LimitReason::BalanceExhausted {
                "insufficient_quota"
            } else {
                "rate_limit_exceeded"
            }
        );
    }
    for (error, expected_status, expected_code) in [
        (
            AppError::NoAuthorizedUpstreamCandidate,
            StatusCode::SERVICE_UNAVAILABLE,
            "no_authorized_upstream_candidate",
        ),
        (
            AppError::from(sqlx::Error::PoolClosed),
            StatusCode::SERVICE_UNAVAILABLE,
            "database_unavailable",
        ),
        (
            AppError::from(sqlx::Error::PoolTimedOut),
            StatusCode::SERVICE_UNAVAILABLE,
            "service_overloaded",
        ),
        (
            AppError::Overloaded,
            StatusCode::SERVICE_UNAVAILABLE,
            "service_overloaded",
        ),
        (
            AppError::Upstream("fixture transport failure".into()),
            StatusCode::BAD_GATEWAY,
            "upstream_error",
        ),
    ] {
        let response = error.into_response();
        assert_eq!(response.status(), expected_status);
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["error"]["code"], expected_code);
        assert!(body["error"].get("reason").is_none());
    }
}

async fn call(
    application: &axum::Router,
    method: Method,
    path: &str,
    credential: &str,
    body: Value,
    expected: StatusCode,
) -> Value {
    let response = application
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header(header::AUTHORIZATION, format!("Bearer {credential}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), expected);
    serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn optional_prepaid_rates_roundtrip_through_operator_and_self_apis() {
    let (state, _directory) = test_state().await;
    let control = router_for_role(state.clone(), RuntimeRole::Control);
    let gateway = router_for_role(state.clone(), RuntimeRole::Gateway);
    let service = &state.config.service_token;
    let fields = [
        "requests_per_minute",
        "tokens_per_minute",
        "max_concurrency",
    ];
    let schemas = call(
        &control,
        Method::GET,
        "/internal/v1/schemas",
        service,
        Value::Null,
        StatusCode::OK,
    )
    .await;
    for schema in [
        &schemas["key_policy"],
        &schemas["key_create"]["properties"]["policy"],
    ] {
        for field in fields {
            assert_eq!(
                schema["properties"][field]["type"],
                json!(["integer", "null"])
            );
            assert!(schema["properties"][field].get("default").is_none());
        }
    }
    for (index, policy) in [None, Some(json!({"requests_per_minute": null, "tokens_per_minute": null, "max_concurrency": null}))].into_iter().enumerate() {
        let mut input = json!({
            "tenant_external_id": "optional-policy-api",
            "principal_external_id": "member",
            "alias": format!("optional-policy-api-{index}"),
            "initial_balance": "1000"
        });
        if let Some(policy) = policy {
            input["policy"] = policy;
        }
        let issued = call(&control, Method::POST, "/internal/v1/keys", service, input, StatusCode::CREATED).await;
        let secret = issued["key"].as_str().unwrap();
        let policy_path = format!("/internal/v1/keys/{}/policy", issued["key_id"].as_str().unwrap());
        let current = call(&gateway, Method::GET, "/self/v1/key", secret, Value::Null, StatusCode::OK).await;
        assert_eq!(current["available_balance"], "1000");
        assert_eq!(current["policy"]["enforcement_mode"], "prepaid");
        for field in fields {
            assert!(current["policy"][field].is_null());
        }
        let limits = call(&gateway, Method::GET, "/self/v1/key/limits", secret, Value::Null, StatusCode::OK).await;
        for rate in ["rpm", "tpm", "concurrency"] {
            assert!(limits[rate]["limit"].is_null());
            assert!(limits[rate]["remaining"].is_null());
        }
        for rate in ["rpm", "tpm"] {
            assert!(limits[rate]["reset_at"].is_null());
        }
        let numeric = json!({"requests_per_minute": 60, "tokens_per_minute": 100000, "max_concurrency": 4});
        let updated = call(&control, Method::PUT, &policy_path, service, numeric.clone(), StatusCode::OK).await;
        for field in fields {
            assert_eq!(updated[field], numeric[field]);
        }
        for field in fields {
            let mut invalid = numeric.clone();
            invalid[field] = json!(0);
            call(&control, Method::PUT, &policy_path, service, invalid, StatusCode::BAD_REQUEST).await;
        }
        let preserved = call(&gateway, Method::GET, "/self/v1/key", secret, Value::Null, StatusCode::OK).await;
        for field in fields {
            assert_eq!(preserved["policy"][field], numeric[field]);
        }
        let cleared = call(&control, Method::PUT, &policy_path, service,
            json!({"requests_per_minute": null, "tokens_per_minute": null, "max_concurrency": null}), StatusCode::OK).await;
        assert_eq!(cleared["enforcement_mode"], "prepaid");
        let key = state.db.authenticate_key(secret, state.config.key_pepper.as_bytes()).await.unwrap();
        for field in fields {
            assert!(cleared[field].is_null());
        }
        let price = state.db.upsert_model_price("optional-policy-api", "USD", Decimal::ONE, Decimal::ONE).await.unwrap();
        let reservation = state.db.reserve_usage(&key, &price, 0, 500_000).await.unwrap();
        assert_eq!(reservation.reserved_micros, 500_000);
        assert_eq!(state.db.settle_usage(&reservation, 0, 123).await.unwrap(), 123);
        let final_limits = call(&gateway, Method::GET, "/self/v1/key/limits", secret, Value::Null, StatusCode::OK).await;
        assert_eq!(final_limits["available_balance"], "999.999877");
        assert_eq!(final_limits["reserved_balance"], "0");
        assert!(final_limits["tpm"]["limit"].is_null());
        assert_eq!(final_limits["tpm"]["used"], 0);
    }
}
