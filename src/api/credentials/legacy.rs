use super::super::*;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct RegisterLegacyKeyCredentialRequest {
    credential: String,
    source_hash: String,
}

pub(in crate::api) async fn register_legacy_key_credential(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(key_id): Path<Uuid>,
    Json(body): Json<RegisterLegacyKeyCredentialRequest>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "keys:write").await?;
    if let Some(tenant) = service.tenant_external_id.as_deref() {
        state.db.require_key_tenant(key_id, tenant).await?;
    }
    Ok((
        StatusCode::CREATED,
        Json(
            state
                .db
                .register_legacy_key_credential(
                    key_id,
                    &body.credential,
                    &body.source_hash,
                    state.config.key_pepper.as_bytes(),
                )
                .await?,
        ),
    ))
}
