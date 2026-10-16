//! Authenticated discovery of the same OpenAPI contract checked by CI.
use super::*;

pub(super) async fn discover(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    authenticated_service(&headers, &state).await?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        include_str!("../../openapi/openapi.json"),
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RuntimeRole;
    use tower::ServiceExt;

    #[tokio::test]
    async fn discovery_requires_service_auth_and_returns_openapi_json() {
        let (state, _directory) = crate::api::tests::test_state().await;
        let token = state.config.service_token.clone();
        let application = router_for_role(state, RuntimeRole::Control);
        let response = application
            .clone()
            .oneshot(
                Request::get("/internal/v1/api-schema")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let response = application
            .oneshot(
                Request::get("/internal/v1/api-schema")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let body = axum::body::to_bytes(response.into_body(), 2_000_000)
            .await
            .unwrap();
        let document: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(document["openapi"], "3.1.0");
        assert!(document["paths"]["/internal/v1/inference/chat/completions"]["post"].is_object());
    }
}
