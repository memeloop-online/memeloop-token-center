use super::super::*;

pub(in crate::api) async fn list_models(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let key = authenticate_downstream(&headers, &state).await?;
    let models = state.db.allowed_models(&key).await?;
    Ok(Json(json!({
        "object": "list",
        "data": models.into_iter().map(|id| json!({
            "id": id,
            "object": "model",
            "owned_by": "memeloop-token-center"
        })).collect::<Vec<_>>()
    })))
}
