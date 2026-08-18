use axum::{
    Json,
    extract::{Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde_json::json;

use crate::{
    AppState, crypto,
    error::AppError,
    model::{AuthenticatedKey, AuthenticatedService},
};

pub(super) async fn authenticate_control_before_body(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, AppError> {
    let _ = authenticated_service(request.headers(), &state).await?;
    Ok(next.run(request).await)
}

pub(super) async fn authenticate_gateway_before_body(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Result<Response, AppError> {
    let _ = authenticate_downstream(request.headers(), &state).await?;
    if request.method() == axum::http::Method::POST {
        request = match crate::gateway_body::admit_gateway_request_body(
            request,
            crate::gateway_body::GATEWAY_BODY_READ_DEADLINE,
        )
        .await
        {
            Ok(request) => request,
            Err(crate::gateway_body::GatewayBodyAdmissionError::Timeout) => {
                return Ok((
                    StatusCode::REQUEST_TIMEOUT,
                    Json(json!({"error": {
                        "code": "request_body_timeout",
                        "message": "request body was not received before the deadline"
                    }})),
                )
                    .into_response());
            }
            Err(crate::gateway_body::GatewayBodyAdmissionError::Rejected) => {
                return Ok((
                    StatusCode::PAYLOAD_TOO_LARGE,
                    Json(json!({"error": {
                        "code": "request_body_too_large",
                        "message": "request body exceeds the supported limit"
                    }})),
                )
                    .into_response());
            }
        };
    }
    Ok(next.run(request).await)
}

pub(super) async fn require_service(
    headers: &HeaderMap,
    state: &AppState,
    scope: &str,
) -> Result<AuthenticatedService, AppError> {
    let service = authenticated_service(headers, state).await?;
    if !service.allows(scope) {
        return Err(AppError::Forbidden);
    }
    Ok(service)
}

async fn authenticated_service(
    headers: &HeaderMap,
    state: &AppState,
) -> Result<AuthenticatedService, AppError> {
    let provided = bearer(headers).ok_or(AppError::Unauthorized)?;
    let service =
        if crypto::constant_time_eq(provided.as_bytes(), state.config.service_token.as_bytes()) {
            AuthenticatedService::bootstrap()
        } else {
            state
                .db
                .authenticate_service_token(provided, state.config.key_pepper.as_bytes())
                .await?
        };
    Ok(service)
}

pub(super) fn require_service_tenant(
    service: &AuthenticatedService,
    tenant_external_id: &str,
) -> Result<(), AppError> {
    if service
        .tenant_external_id
        .as_deref()
        .is_some_and(|tenant| tenant != tenant_external_id)
    {
        return Err(AppError::Forbidden);
    }
    Ok(())
}

pub(super) fn management_tenant(
    service: &AuthenticatedService,
    requested: Option<String>,
) -> Result<Option<String>, AppError> {
    let requested = requested
        .map(|tenant| tenant.trim().to_owned())
        .filter(|tenant| !tenant.is_empty());
    if requested.as_ref().is_some_and(|tenant| tenant.len() > 200) {
        return Err(AppError::BadRequest(
            "tenant_external_id must contain at most 200 characters".into(),
        ));
    }
    match service.tenant_external_id.as_deref() {
        Some(scoped) => {
            if requested.as_deref().is_some_and(|tenant| tenant != scoped) {
                return Err(AppError::Forbidden);
            }
            Ok(Some(scoped.to_owned()))
        }
        None => Ok(requested),
    }
}

pub(super) fn require_global_service(service: &AuthenticatedService) -> Result<(), AppError> {
    if service.tenant_external_id.is_some() {
        return Err(AppError::Forbidden);
    }
    Ok(())
}

pub(super) async fn authenticate_downstream(
    headers: &HeaderMap,
    state: &AppState,
) -> Result<AuthenticatedKey, AppError> {
    let provided = bearer(headers)
        .or_else(|| {
            headers
                .get("x-api-key")
                .and_then(|value| value.to_str().ok())
        })
        .ok_or(AppError::Unauthorized)?;
    state
        .db
        .authenticate_key(provided, state.config.key_pepper.as_bytes())
        .await
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}
