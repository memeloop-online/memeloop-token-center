use super::super::*;

pub(in crate::api) async fn self_key(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let key = authenticate_downstream(&headers, &state).await?;
    Ok(Json(state.db.key_view(&key).await?))
}

pub(in crate::api) async fn self_key_limits(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let key = authenticate_downstream(&headers, &state).await?;
    Ok(Json(state.db.key_limit_snapshot(key.key_id).await?))
}
