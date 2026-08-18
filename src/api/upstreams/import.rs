use super::super::*;
use super::accounts::{validate_provider_schema, validate_upstream_destination};
use sha2::{Digest, Sha256};

#[derive(Debug, Deserialize)]
pub(in crate::api) struct ImportCpaSubscriptionAccountsRequest {
    #[serde(default = "default_tenant")]
    tenant_external_id: String,
    bridge_base_url: String,
    bridge_secret: Option<String>,
    auth_files: Vec<CpaAuthFile>,
}

#[derive(Debug, Deserialize)]
struct CpaAuthFile {
    filename: String,
    document: Value,
}

pub(in crate::api) async fn import_cpa_subscription_accounts(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ImportCpaSubscriptionAccountsRequest>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "providers:write").await?;
    require_service_tenant(&service, &body.tenant_external_id)?;
    // Imported subscription handles always target the in-cluster bridge, so only
    // the global operator may authorize this private-network destination.
    require_global_service(&service)?;
    if body.auth_files.is_empty() || body.auth_files.len() > 32 {
        return Err(AppError::BadRequest(
            "auth_files must contain 1 to 32 CPA auth documents".into(),
        ));
    }
    let base_url = crate::provider::validate_config(&json!({
        "base_url": body.bridge_base_url
    }))?;
    if body
        .bridge_secret
        .as_deref()
        .is_some_and(|secret| secret.trim().is_empty())
    {
        return Err(AppError::BadRequest("bridge_secret cannot be empty".into()));
    }

    let mut imported = Vec::new();
    let mut skipped = Vec::new();
    for auth_file in body.auth_files {
        validate_cpa_auth_filename(&auth_file.filename)?;
        let source_fingerprint = cpa_source_fingerprint(&auth_file.filename);
        let serialized = serde_json::to_vec(&auth_file.document).map_err(|_| AppError::Internal)?;
        if serialized.len() > 1024 * 1024 {
            return Err(AppError::BadRequest(
                "CPA auth document exceeds 1 MiB".into(),
            ));
        }
        match cpa_subscription_account(&auth_file.document)? {
            Some((provider, handle, label)) => {
                let digest = Sha256::digest(
                    format!("{}\0{provider}\0{handle}", body.tenant_external_id).as_bytes(),
                );
                let mut session_bytes = [0_u8; 16];
                session_bytes.copy_from_slice(&digest[..16]);
                let oauth_session_id = Uuid::from_bytes(session_bytes);
                let suffix = format!(
                    "{:02x}{:02x}{:02x}{:02x}",
                    digest[0], digest[1], digest[2], digest[3]
                );
                let provider_config = json!({
                    "base_url": base_url.clone(),
                    "provider": provider.clone(),
                    "network_scope": "private"
                });
                let credential = UpstreamCredential::SubscriptionBridge {
                    handle,
                    secret: body.bridge_secret.clone(),
                };
                validate_provider_schema(
                    &state,
                    "cpa-subscription-bridge",
                    &provider_config,
                    &credential,
                )?;
                validate_upstream_destination(
                    "cpa-subscription-bridge",
                    &provider_config,
                    &service,
                    &state,
                )
                .await?;
                let account = state
                    .db
                    .create_upstream_account(
                        CreateUpstreamAccountInput {
                            tenant_external_id: body.tenant_external_id.clone(),
                            name: format!("cpa-{provider}-{suffix}"),
                            driver: "cpa-subscription-bridge".to_owned(),
                            config: provider_config,
                            credential,
                            oauth_session_id: Some(oauth_session_id),
                            oauth_driver: None,
                            oauth_refresh_url: None,
                        },
                        state.config.key_pepper.as_bytes(),
                    )
                    .await?;
                imported.push(json!({
                    "source_fingerprint": source_fingerprint,
                    "provider": provider,
                    "label": label,
                    "account": account
                }));
            }
            None => skipped.push(json!({
                "source_fingerprint": source_fingerprint,
                "reason": "requires_provider_adapter"
            })),
        }
    }
    Ok((
        StatusCode::CREATED,
        Json(json!({"imported": imported, "skipped": skipped})),
    ))
}

fn cpa_source_fingerprint(filename: &str) -> String {
    let digest = Sha256::digest(filename.as_bytes());
    format!(
        "sha256:{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        digest[0], digest[1], digest[2], digest[3], digest[4], digest[5], digest[6], digest[7]
    )
}

pub(in crate::api) fn validate_cpa_auth_filename(filename: &str) -> Result<(), AppError> {
    if filename.is_empty()
        || filename.len() > 200
        || filename.contains('/')
        || filename.contains('\\')
        || filename == "."
        || filename == ".."
        || !filename.ends_with(".json")
    {
        return Err(AppError::BadRequest(
            "CPA auth filename must be a safe .json basename".into(),
        ));
    }
    Ok(())
}

pub(in crate::api) fn cpa_subscription_account(
    document: &Value,
) -> Result<Option<(String, String, Option<String>)>, AppError> {
    let object = document
        .as_object()
        .ok_or_else(|| AppError::BadRequest("CPA auth document must be an object".into()))?;
    if object
        .get("disabled")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Ok(None);
    }
    let Some(provider) = object.get("upstream").and_then(Value::as_str) else {
        return Ok(None);
    };
    if !matches!(provider, "copilot" | "cursor") {
        return Ok(None);
    }
    let handle = object
        .get("handle")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::BadRequest("CPA subscription auth omitted handle".into()))?;
    UpstreamCredential::SubscriptionBridge {
        handle: handle.to_owned(),
        secret: None,
    }
    .validate(unix_millis())?;
    let label = object
        .get("label")
        .and_then(Value::as_str)
        .filter(|label| label.len() <= 200)
        .map(str::to_owned);
    Ok(Some((provider.to_owned(), handle.to_owned(), label)))
}
