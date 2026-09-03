use axum::{
    Json,
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use serde_json::json;
use std::time::Duration;

use crate::{
    AppState, crypto,
    error::AppError,
    model::{AuthenticatedKey, AuthenticatedService},
};

use super::limits::{
    CLOUD_WEBHOOK_BODY_PERMITS, CLOUD_WEBHOOK_BODY_READ_DEADLINE, IMAGE_RESPONSE_PERMITS,
    MAX_CLOUD_WEBHOOK_BODY, REQUEST_ID_HEADER,
};

const CONTROL_BODY_READ_DEADLINE: Duration = Duration::from_secs(60);
const CONTROL_BODY_PERMIT_WAIT: Duration = Duration::from_secs(1);
const CONTROL_BODY_READ_CONCURRENCY: usize = 4;
static CONTROL_BODY_READ_PERMITS: tokio::sync::Semaphore =
    tokio::sync::Semaphore::const_new(CONTROL_BODY_READ_CONCURRENCY);

pub(super) async fn authenticate_control_before_body(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Result<Response, AppError> {
    let _ = authenticated_service(request.headers(), &state).await?;
    if matches!(
        *request.method(),
        axum::http::Method::POST | axum::http::Method::PUT | axum::http::Method::PATCH
    ) {
        let _permit = match tokio::time::timeout(
            CONTROL_BODY_PERMIT_WAIT,
            CONTROL_BODY_READ_PERMITS.acquire(),
        )
        .await
        {
            Ok(Ok(permit)) => permit,
            Ok(Err(_)) => return Err(AppError::Internal),
            Err(_) => {
                return Ok(control_body_rejection(
                    StatusCode::TOO_MANY_REQUESTS,
                    "control_body_capacity_exhausted",
                    "control request body capacity is exhausted",
                ));
            }
        };
        let maximum = match request.uri().path() {
            "/internal/v1/imports/cpa/managed-oauth" => super::MAX_MANAGED_OAUTH_IMPORT_REQUEST,
            _ => super::MAX_DEFAULT_REQUEST_BODY,
        };
        request = match crate::gateway_body::admit_request_body(
            request,
            CONTROL_BODY_READ_DEADLINE,
            maximum,
        )
        .await
        {
            Ok(request) => request,
            // The control-plane reader has its own permit, but preserve the
            // service-capacity classification if that implementation changes.
            Err(crate::gateway_body::GatewayBodyAdmissionError::CapacityExhausted) => {
                return Err(AppError::Overloaded);
            }
            Err(crate::gateway_body::GatewayBodyAdmissionError::Timeout) => {
                return Ok(control_body_rejection(
                    StatusCode::REQUEST_TIMEOUT,
                    "request_body_timeout",
                    "request body was not received before the deadline",
                ));
            }
            Err(crate::gateway_body::GatewayBodyAdmissionError::Rejected(_)) => {
                return Ok(control_body_rejection(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "request_body_too_large",
                    "request body exceeds the supported limit",
                ));
            }
        };
        // Keep the permit through parsing and handler execution. Releasing it
        // immediately after buffering would still allow many maximum-sized
        // JSON values to be parsed and retained concurrently.
        return Ok(normalize_control_extractor_rejection(
            next.run(request).await,
        ));
    }
    Ok(normalize_control_extractor_rejection(
        next.run(request).await,
    ))
}

fn normalize_control_extractor_rejection(response: Response) -> Response {
    if matches!(
        response.status(),
        StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY
    ) && response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_none_or(|value| {
            let media_type = value.split(';').next().unwrap_or_default().trim();
            !(media_type == "application/json" || media_type.ends_with("+json"))
        })
    {
        return AppError::BadRequest(
            "request parameters or body do not match the API schema".to_owned(),
        )
        .into_response();
    }
    response
}

fn control_body_rejection(
    status: StatusCode,
    code: &'static str,
    message: &'static str,
) -> Response {
    (
        status,
        Json(json!({"error": {"code": code, "message": message}})),
    )
        .into_response()
}

pub(super) async fn authenticate_gateway_before_body(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Result<Response, AppError> {
    let _ = authenticate_downstream(request.headers(), &state).await?;
    let image_lifecycle_permit = if request.uri().path() == "/v1/images/generations" {
        Some(
            IMAGE_RESPONSE_PERMITS
                .try_acquire()
                .map_err(|_| AppError::Overloaded)?,
        )
    } else {
        None
    };
    if request.method() == axum::http::Method::POST {
        let request_id = safe_gateway_request_id(request.headers());
        request = match crate::gateway_body::admit_gateway_request_body(
            request,
            crate::gateway_body::GATEWAY_BODY_READ_DEADLINE,
            state.gateway_body_read_permits.clone(),
            state.responses_body_read_permits.clone(),
            state.config.responses_body_max_bytes as usize,
        )
        .await
        {
            Ok(request) => request,
            Err(error) => return Ok(gateway_body_admission_rejection(&state, &request_id, error)),
        };
    }
    let response = next.run(request).await;
    Ok(match image_lifecycle_permit {
        Some(permit) => hold_response_body_permit(response, permit),
        None => response,
    })
}

fn gateway_body_admission_rejection(
    state: &AppState,
    request_id: &str,
    error: crate::gateway_body::GatewayBodyAdmissionError,
) -> Response {
    match error {
        crate::gateway_body::GatewayBodyAdmissionError::CapacityExhausted => {
            gateway_body_capacity_rejection()
        }
        crate::gateway_body::GatewayBodyAdmissionError::Timeout => (
            StatusCode::REQUEST_TIMEOUT,
            Json(json!({"error": {
                "code": "request_body_timeout",
                "message": "request body was not received before the deadline"
            }})),
        )
            .into_response(),
        crate::gateway_body::GatewayBodyAdmissionError::Rejected(rejection) => {
            state.gateway_body_rejections.observe(rejection);
            // All fields are server-derived numbers or fixed enums. In
            // particular, never log credentials, model names, raw paths or
            // any part of the rejected request body.
            tracing::warn!(
                request_id = %request_id,
                route_class = rejection.route_class.label(),
                declared_content_length = ?rejection.declared_content_length,
                limit_bytes = rejection.limit_bytes,
                reason = rejection.reason.label(),
                "gateway request body rejected"
            );
            (
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(json!({"error": {
                    "code": "request_body_too_large",
                    "message": "request body exceeds the supported limit"
                }})),
            )
                .into_response()
        }
    }
}

fn gateway_body_capacity_rejection() -> Response {
    AppError::Overloaded.into_response()
}

fn safe_gateway_request_id(headers: &HeaderMap) -> String {
    headers
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| uuid::Uuid::parse_str(value).ok())
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unavailable".to_owned())
}

/// Keeps a lifecycle guard alive until the response body reaches EOF, errors,
/// or is dropped by the downstream connection. Returning an Axum response does
/// not mean Hyper has delivered its body, so a middleware-local guard is
/// otherwise released too early for large synchronous image responses.
fn hold_response_body_permit<P>(response: Response, permit: P) -> Response
where
    P: Send + 'static,
{
    let (parts, body) = response.into_parts();
    let stream = futures_util::stream::unfold(
        (body.into_data_stream(), permit),
        |(mut body, permit)| async move { body.next().await.map(|item| (item, (body, permit))) },
    );
    Response::from_parts(parts, Body::from_stream(stream))
}

pub(super) async fn admit_cloud_webhook_before_body(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Result<Response, AppError> {
    super::cloud_entitlements::preflight_cloud_webhook(&state, request.headers())?;
    let _body_permit = CLOUD_WEBHOOK_BODY_PERMITS
        .try_acquire()
        .map_err(|_| AppError::RateLimited)?;
    request = match crate::gateway_body::admit_request_body(
        request,
        CLOUD_WEBHOOK_BODY_READ_DEADLINE,
        MAX_CLOUD_WEBHOOK_BODY,
    )
    .await
    {
        Ok(request) => request,
        // This path has an independent webhook permit. See the same note on
        // the control-plane reader above.
        Err(crate::gateway_body::GatewayBodyAdmissionError::CapacityExhausted) => {
            return Err(AppError::Overloaded);
        }
        Err(crate::gateway_body::GatewayBodyAdmissionError::Timeout) => {
            return Ok(control_body_rejection(
                StatusCode::REQUEST_TIMEOUT,
                "request_body_timeout",
                "request body was not received before the deadline",
            ));
        }
        Err(crate::gateway_body::GatewayBodyAdmissionError::Rejected(_)) => {
            return Ok(control_body_rejection(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request_body_too_large",
                "request body exceeds the supported limit",
            ));
        }
    };
    Ok(next.run(request).await)
}

#[cfg(test)]
mod response_body_guard_tests {
    use std::sync::Arc;

    use axum::body::Bytes;

    use super::*;

    #[tokio::test]
    async fn response_body_guard_is_held_until_eof() {
        let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
        let permit = semaphore.clone().try_acquire_owned().unwrap();
        let response = hold_response_body_permit(
            Response::new(Body::from(Bytes::from_static(b"image"))),
            permit,
        );
        assert_eq!(semaphore.available_permits(), 0);

        let mut body = response.into_body().into_data_stream();
        assert_eq!(
            body.next().await.unwrap().unwrap(),
            Bytes::from_static(b"image")
        );
        assert_eq!(
            semaphore.available_permits(),
            0,
            "reading the last data frame is not the same as observing EOF"
        );
        assert!(body.next().await.is_none());
        assert_eq!(semaphore.available_permits(), 1);
    }

    #[tokio::test]
    async fn response_body_guard_is_released_on_drop_and_error() {
        let dropped = Arc::new(tokio::sync::Semaphore::new(1));
        let permit = dropped.clone().try_acquire_owned().unwrap();
        let response = hold_response_body_permit(Response::new(Body::from("image")), permit);
        assert_eq!(dropped.available_permits(), 0);
        drop(response);
        assert_eq!(dropped.available_permits(), 1);

        let failed = Arc::new(tokio::sync::Semaphore::new(1));
        let permit = failed.clone().try_acquire_owned().unwrap();
        let body = Body::from_stream(futures_util::stream::once(async {
            Err::<Bytes, _>(std::io::Error::other("downstream body failure"))
        }));
        let response = hold_response_body_permit(Response::new(body), permit);
        let mut body = response.into_body().into_data_stream();
        assert!(body.next().await.unwrap().is_err());
        assert_eq!(failed.available_permits(), 0);
        drop(body);
        assert_eq!(failed.available_permits(), 1);
    }

    #[tokio::test]
    async fn gateway_body_capacity_exhaustion_is_a_service_overload_not_a_rate_limit() {
        let response = gateway_body_capacity_rejection();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .expect("error body");
        assert!(
            std::str::from_utf8(&body)
                .expect("UTF-8 error")
                .contains("service_overloaded")
        );
    }

    #[test]
    fn gateway_body_rejection_logs_only_a_uuid_request_id() {
        let request_id = uuid::Uuid::now_v7();
        let mut headers = HeaderMap::new();
        headers.insert(
            REQUEST_ID_HEADER,
            request_id.to_string().parse().expect("request ID header"),
        );
        assert_eq!(safe_gateway_request_id(&headers), request_id.to_string());
        headers.insert(
            REQUEST_ID_HEADER,
            "user-controlled-value"
                .parse()
                .expect("valid non-UUID header"),
        );
        assert_eq!(safe_gateway_request_id(&headers), "unavailable");
    }
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

pub(super) async fn require_service_any(
    headers: &HeaderMap,
    state: &AppState,
    scopes: &[&str],
) -> Result<AuthenticatedService, AppError> {
    let service = authenticated_service(headers, state).await?;
    if !scopes.iter().any(|scope| service.allows(scope)) {
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
    let provided = downstream_credential(headers).ok_or(AppError::Unauthorized)?;
    state
        .db
        .authenticate_key(provided, state.config.key_pepper.as_bytes())
        .await
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    single_header(headers, header::AUTHORIZATION)?.strip_prefix("Bearer ")
}

fn downstream_credential(headers: &HeaderMap) -> Option<&str> {
    // Never let a second credential source rescue an ambiguous or malformed
    // Authorization header. Different proxies disagree about whether the
    // first or last duplicate wins, so accepting either would make the
    // authenticated identity depend on which hop inspected the request.
    if headers.contains_key(header::AUTHORIZATION) {
        bearer(headers)
    } else {
        single_header(headers, "x-api-key")
    }
}

fn single_header(headers: &HeaderMap, name: impl axum::http::header::AsHeaderName) -> Option<&str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        return None;
    }
    Some(value)
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    #[test]
    fn duplicate_authorization_headers_fail_closed() {
        let mut headers = HeaderMap::new();
        headers.append(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer first"),
        );
        headers.append(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer second"),
        );

        assert_eq!(bearer(&headers), None);
        assert_eq!(downstream_credential(&headers), None);
    }

    #[test]
    fn malformed_authorization_does_not_fall_back_to_api_key() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Basic ignored"),
        );
        headers.insert("x-api-key", HeaderValue::from_static("must-not-be-used"));

        assert_eq!(downstream_credential(&headers), None);
    }

    #[test]
    fn duplicate_api_key_headers_fail_closed() {
        let mut headers = HeaderMap::new();
        headers.append("x-api-key", HeaderValue::from_static("first"));
        headers.append("x-api-key", HeaderValue::from_static("second"));

        assert_eq!(downstream_credential(&headers), None);
    }

    #[test]
    fn either_single_supported_credential_is_accepted() {
        let mut bearer_headers = HeaderMap::new();
        bearer_headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer bearer-token"),
        );
        assert_eq!(downstream_credential(&bearer_headers), Some("bearer-token"));

        let mut api_key_headers = HeaderMap::new();
        api_key_headers.insert("x-api-key", HeaderValue::from_static("api-key"));
        assert_eq!(downstream_credential(&api_key_headers), Some("api-key"));
    }
}
