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
    Json(body): Json<PutHttpSettings>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "providers:write").await?;
    require_global_service(&service)?;
    state
        .db
        .replace_http_settings(body.expected_revision, body.settings, service.service_id)
        .await?;
    Ok((
        [(header::CACHE_CONTROL, "private, no-store")],
        Json(state.db.http_clients.snapshot().document.view()),
    ))
}
