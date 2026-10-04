use super::*;
use crate::db::transport_proxy_management::{
    BindGroup, CreateGroup, DeleteGroup, UnbindGroup, UpdateGroup,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TenantQuery {
    tenant_external_id: String,
}

async fn authority(
    headers: &HeaderMap,
    state: &AppState,
    tenant: &str,
) -> Result<Option<Uuid>, AppError> {
    let service = require_service(headers, state, "providers:write").await?;
    require_global_service(&service)?;
    require_service_tenant(&service, tenant)?;
    Ok(service.service_id)
}

fn response(result: Result<(StatusCode, Value), AppError>) -> axum::response::Response {
    let mut response = match result {
        Ok((StatusCode::NO_CONTENT, _)) => StatusCode::NO_CONTENT.into_response(),
        Ok((status, body)) => (status, Json(body)).into_response(),
        Err(AppError::Storage(_)) => AppError::Overloaded.into_response(),
        Err(error) => error.into_response(),
    };
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response
}

pub(super) async fn transport_group_response(
    mut response: axum::response::Response,
) -> axum::response::Response {
    if matches!(
        response.status(),
        StatusCode::UNPROCESSABLE_ENTITY | StatusCode::BAD_REQUEST
    ) {
        response =
            AppError::BadRequest("invalid transport proxy group request".into()).into_response();
    }
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response
}

pub(super) async fn list_transport_groups(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<TenantQuery>,
) -> axum::response::Response {
    response(
        async {
            authority(&headers, &state, &query.tenant_external_id).await?;
            Ok((
                StatusCode::OK,
                state
                    .db
                    .list_transport_groups(
                        &query.tenant_external_id,
                        state.config.key_pepper.as_bytes(),
                    )
                    .await?,
            ))
        }
        .await,
    )
}

pub(super) async fn get_transport_group(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Query(query): Query<TenantQuery>,
) -> axum::response::Response {
    response(
        async {
            authority(&headers, &state, &query.tenant_external_id).await?;
            Ok((
                StatusCode::OK,
                state
                    .db
                    .get_transport_group(
                        id,
                        &query.tenant_external_id,
                        state.config.key_pepper.as_bytes(),
                    )
                    .await?,
            ))
        }
        .await,
    )
}

pub(super) async fn create_transport_group(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateGroup>,
) -> axum::response::Response {
    response(
        async {
            let actor = authority(&headers, &state, &body.tenant_external_id).await?;
            Ok((
                StatusCode::CREATED,
                state
                    .db
                    .create_transport_group(body, actor, state.config.key_pepper.as_bytes())
                    .await?,
            ))
        }
        .await,
    )
}

pub(super) async fn update_transport_group(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateGroup>,
) -> axum::response::Response {
    response(
        async {
            let actor = authority(&headers, &state, &body.tenant_external_id).await?;
            Ok((
                StatusCode::OK,
                state
                    .db
                    .update_transport_group(id, body, actor, state.config.key_pepper.as_bytes())
                    .await?,
            ))
        }
        .await,
    )
}

pub(super) async fn delete_transport_group(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(body): Json<DeleteGroup>,
) -> axum::response::Response {
    response(
        async {
            let actor = authority(&headers, &state, &body.tenant_external_id).await?;
            state.db.delete_transport_group(id, body, actor).await?;
            Ok((StatusCode::NO_CONTENT, Value::Null))
        }
        .await,
    )
}

pub(super) async fn get_transport_binding(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Query(query): Query<TenantQuery>,
) -> axum::response::Response {
    response(
        async {
            authority(&headers, &state, &query.tenant_external_id).await?;
            let mut body = state
                .db
                .get_transport_binding(id, &query.tenant_external_id)
                .await?;
            state.transport_proxy_groups.describe_binding(&mut body);
            Ok((StatusCode::OK, body))
        }
        .await,
    )
}

pub(super) async fn bind_transport_group(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(body): Json<BindGroup>,
) -> axum::response::Response {
    response(
        async {
            let actor = authority(&headers, &state, &body.tenant_external_id).await?;
            let body = state
                .db
                .bind_transport_group(id, body, actor, state.config.key_pepper.as_bytes())
                .await?;
            Ok((StatusCode::ACCEPTED, body))
        }
        .await,
    )
}

pub(super) async fn unbind_transport_group(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(body): Json<UnbindGroup>,
) -> axum::response::Response {
    response(
        async {
            let actor = authority(&headers, &state, &body.tenant_external_id).await?;
            let body = state
                .db
                .unbind_transport_group(id, body, actor, state.config.key_pepper.as_bytes())
                .await?;
            Ok((StatusCode::ACCEPTED, body))
        }
        .await,
    )
}
