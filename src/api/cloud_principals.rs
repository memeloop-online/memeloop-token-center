use super::*;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct EnsureCloudPrincipalRequest {
    tenant_external_id: String,
    principal_external_id: String,
    currency: String,
}

/// Ensures the credential and credit-account owner used by Cloud subscription
/// snapshots exists. Unlike the signed webhook, this endpoint never creates
/// entitlements, changes policy/routing, or posts ledger entries.
pub(in crate::api) async fn ensure_memeloop_cloud_principal(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<EnsureCloudPrincipalRequest>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "keys:write").await?;
    if body.tenant_external_id != body.tenant_external_id.trim()
        || body.principal_external_id != body.principal_external_id.trim()
    {
        return Err(AppError::BadRequest(
            "tenant_external_id and principal_external_id must not have surrounding whitespace"
                .into(),
        ));
    }
    require_service_tenant(&service, &body.tenant_external_id)?;
    let credential = state
        .db
        .ensure_cloud_credential(
            &body.tenant_external_id,
            &body.principal_external_id,
            &body.currency,
            &super::cloud_entitlements::cloud_principal_provisioning_key(
                &body.tenant_external_id,
                &body.principal_external_id,
            ),
            state.config.key_pepper.as_bytes(),
        )
        .await?;
    Ok(Json(credential))
}
