use super::*;
use crate::http_settings::HttpSettings;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PutHttpSettings {
    expected_revision: u64,
    settings: HttpSettings,
}

pub(super) async fn get(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "providers:read").await?;
    require_global_service(&service)?;
    state.db.refresh_http_settings().await?;
    Ok((
        [(header::CACHE_CONTROL, "private, no-store")],
        Json(state.db.http_clients.snapshot().document.view()),
    ))
}

pub(super) async fn put(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<PutHttpSettings>, axum::extract::rejection::JsonRejection>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "providers:write").await?;
    require_global_service(&service)?;
    let Json(body) =
        body.map_err(|_| AppError::BadRequest("invalid HTTP settings document".into()))?;
    state
        .db
        .replace_http_settings(body.expected_revision, body.settings, service.service_id)
        .await?;
    Ok((
        [(header::CACHE_CONTROL, "private, no-store")],
        Json(state.db.http_clients.snapshot().document.view()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use tower::ServiceExt;

    const ENDPOINT: &str = "/internal/v1/system-settings/http";

    fn request(method: &str, token: Option<&str>, body: Value) -> Request {
        let mut builder = axum::http::Request::builder()
            .method(method)
            .uri(ENDPOINT)
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        builder.body(Body::from(body.to_string())).unwrap()
    }

    #[tokio::test]
    async fn settings_require_global_authority_and_correct_scope() {
        let (state, _directory) = crate::api::tests::test_state().await;
        state
            .db
            .create_tenant("http-policy-tenant", None)
            .await
            .unwrap();
        let scoped = state
            .db
            .create_service_token(
                crate::db::CreateServiceTokenInput {
                    name: "http-scoped".into(),
                    scopes: vec!["providers:read".into(), "providers:write".into()],
                    tenant_external_id: Some("http-policy-tenant".into()),
                },
                state.config.key_pepper.as_bytes(),
            )
            .await
            .unwrap();
        let reader = state
            .db
            .create_service_token(
                crate::db::CreateServiceTokenInput {
                    name: "http-reader".into(),
                    scopes: vec!["providers:read".into()],
                    tenant_external_id: None,
                },
                state.config.key_pepper.as_bytes(),
            )
            .await
            .unwrap();
        let app = router_for_role(state.clone(), crate::config::RuntimeRole::Control);
        for method in ["GET", "PUT"] {
            let body = json!({"expected_revision":0,"settings":{}});
            assert_eq!(
                app.clone()
                    .oneshot(request(method, None, body.clone()))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::UNAUTHORIZED
            );
            assert_eq!(
                app.clone()
                    .oneshot(request(method, Some(&scoped.token), body))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::FORBIDDEN
            );
        }
        assert_eq!(
            app.clone()
                .oneshot(request("GET", Some(&reader.token), Value::Null))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            app.oneshot(request(
                "PUT",
                Some(&reader.token),
                json!({"expected_revision":0,"settings":{}})
            ))
            .await
            .unwrap()
            .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(state.db.http_clients.snapshot().document.revision, 0);
    }

    #[tokio::test]
    async fn settings_api_preserves_inheritance_rejects_stale_writes_and_reports_effective_values()
    {
        let (state, _directory) = crate::api::tests::test_state().await;
        let token = state.config.service_token.clone();
        let app = router_for_role(state.clone(), crate::config::RuntimeRole::Control);
        let document = json!({
            "expected_revision": 0,
            "settings": {
                "defaults": {"request_timeout_ms": 1000},
                "purposes": {"oauth": {"request_timeout_ms": null}}
            }
        });
        let response = app
            .clone()
            .oneshot(request("PUT", Some(&token), document.clone()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "private, no-store"
        );
        let bytes = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        let view: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(view["revision"], 1);
        assert_eq!(
            view["effective"]["general"]["request_timeout_ms"],
            json!({"value":1000,"source":"global_default","unit":"milliseconds"})
        );
        assert_eq!(
            view["effective"]["oauth"]["request_timeout_ms"],
            json!({"value":null,"source":"purpose_override","unit":"milliseconds"})
        );
        assert_eq!(
            app.clone()
                .oneshot(request("PUT", Some(&token), document))
                .await
                .unwrap()
                .status(),
            StatusCode::CONFLICT
        );
        for settings in [
            json!({"defaults":{"typo":5}}),
            json!({"defaults":{"read_timeout_ms":0}}),
            json!({"purposes":{"unknown":{}}}),
            json!({"defaults":{"pool_max_idle_per_host":null}}),
        ] {
            let response = app
                .clone()
                .oneshot(request(
                    "PUT",
                    Some(&token),
                    json!({"expected_revision":1,"settings":settings}),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
        for revision in [
            json!(-1),
            json!(1.5),
            json!(9_007_199_254_740_992_u64),
            Value::Null,
        ] {
            let response = app
                .clone()
                .oneshot(request(
                    "PUT",
                    Some(&token),
                    json!({"expected_revision":revision,"settings":{}}),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
        let cleared = app
            .clone()
            .oneshot(request(
                "PUT",
                Some(&token),
                json!({"expected_revision":1,"settings":{}}),
            ))
            .await
            .unwrap();
        assert_eq!(cleared.status(), StatusCode::OK);
        let response = app
            .oneshot(request("GET", Some(&token), Value::Null))
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        let view: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(view["revision"], 2);
        assert_eq!(
            view["effective"]["oauth"]["request_timeout_ms"]["source"],
            "source_default"
        );
        assert_eq!(
            view["effective"]["oauth"]["request_timeout_ms"]["value"],
            Value::Null
        );
    }
}
