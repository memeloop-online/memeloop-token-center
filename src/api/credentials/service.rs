use super::super::*;
use super::client::StatusRequest;

#[derive(Debug, Deserialize)]
pub(in crate::api) struct CreateServiceTokenRequest {
    name: String,
    scopes: Vec<String>,
    tenant_external_id: Option<String>,
}

pub(in crate::api) async fn create_service_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateServiceTokenRequest>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "service_tokens:write").await?;
    require_global_service(&service)?;
    let issued = state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: body.name,
                scopes: body.scopes,
                tenant_external_id: body.tenant_external_id,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await?;
    Ok((StatusCode::CREATED, Json(issued)))
}

pub(in crate::api) async fn list_service_tokens(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "service_tokens:read").await?;
    require_global_service(&service)?;
    Ok(Json(state.db.list_service_tokens().await?))
}

pub(in crate::api) async fn rotate_service_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(service_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "service_tokens:write").await?;
    require_global_service(&service)?;
    let idempotency_key = headers
        .get("idempotency-key")
        .ok_or_else(|| AppError::BadRequest("Idempotency-Key is required".into()))?
        .to_str()
        .map_err(|_| AppError::BadRequest("Idempotency-Key must be valid ASCII".into()))?;
    Ok(Json(
        state
            .db
            .rotate_service_token(
                service_id,
                idempotency_key,
                state.config.key_pepper.as_bytes(),
            )
            .await?,
    ))
}

pub(in crate::api) async fn set_service_token_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(service_id): Path<Uuid>,
    Json(body): Json<StatusRequest>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "service_tokens:write").await?;
    require_global_service(&service)?;
    Ok(Json(json!({
        "service_id": service_id,
        "status": state.db.set_service_token_status(service_id, &body.status).await?
    })))
}
