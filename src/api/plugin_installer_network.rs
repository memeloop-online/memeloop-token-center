use super::*;
use crate::db::plugin_installer_network::{FrozenInstallerNetwork, InstallerNetworkInput};

pub(super) async fn installer_network_response(mut response: Response) -> Response {
    if matches!(
        response.status(),
        StatusCode::UNPROCESSABLE_ENTITY | StatusCode::BAD_REQUEST
    ) {
        response =
            AppError::BadRequest("invalid plugin installation request".into()).into_response();
    }
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct InstallerGroupsQuery {
    tenant_external_id: String,
}

pub(super) async fn installer_network_actor(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<String, AppError> {
    let service = require_service(headers, state, "plugins:write").await?;
    require_global_service(&service)?;
    Ok(service
        .service_id
        .map(|id| format!("service:{id}"))
        .unwrap_or_else(|| "bootstrap".into()))
}

pub(super) async fn authorize_installer_network_input(
    state: &AppState,
    headers: &HeaderMap,
    input: &InstallerNetworkInput,
) -> Result<(), AppError> {
    installer_network_actor(state, headers).await?;
    if let Some(tenant) = input.tenant() {
        let service = require_service(headers, state, "providers:read").await?;
        require_service_tenant(&service, tenant)?;
    }
    Ok(())
}

pub(super) async fn authorize_installer_network_snapshot(
    state: &AppState,
    headers: &HeaderMap,
    snapshot: &FrozenInstallerNetwork,
) -> Result<(), AppError> {
    installer_network_actor(state, headers).await?;
    if let Some(group) = &snapshot.group {
        let service = require_service(headers, state, "providers:read").await?;
        require_service_tenant(&service, &group.tenant_external_id)?;
    }
    Ok(())
}

pub(super) async fn get_installer_network(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let actor = installer_network_actor(&state, &headers).await?;
    let input = state
        .db
        .installer_network_preference(&actor, state.config.key_pepper.as_bytes())
        .await?;
    authorize_installer_network_input(&state, &headers, &input).await?;
    Ok((
        [(header::CACHE_CONTROL, "private, no-store")],
        Json(input.view()),
    ))
}

pub(super) async fn put_installer_network(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<InstallerNetworkInput>,
) -> Result<impl IntoResponse, AppError> {
    let actor = installer_network_actor(&state, &headers).await?;
    authorize_installer_network_input(&state, &headers, &input).await?;
    let view = state
        .db
        .put_installer_network_preference(&actor, input, state.config.key_pepper.as_bytes())
        .await?;
    Ok(([(header::CACHE_CONTROL, "private, no-store")], Json(view)))
}

pub(super) async fn installer_network_groups(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<InstallerGroupsQuery>,
) -> Result<impl IntoResponse, AppError> {
    installer_network_actor(&state, &headers).await?;
    let service = require_service(&headers, &state, "providers:read").await?;
    require_service_tenant(&service, &query.tenant_external_id)?;
    let groups = state
        .db
        .list_transport_groups(
            &query.tenant_external_id,
            state.config.key_pepper.as_bytes(),
        )
        .await?;
    Ok(([(header::CACHE_CONTROL, "private, no-store")], Json(groups)))
}
