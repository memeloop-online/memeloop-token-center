use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use super::super::*;
use super::native_oauth_import::{canonical_json, valid_relative_path, validate_lower_hex_digest};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeCursorRequest {
    contract: String,
    tenant_external_id: String,
    account_name: String,
    source_identity_hash: String,
    source_document_sha256: String,
    source_layout: String,
    source_relative_path: String,
    expected_current_account_id: Option<Uuid>,
    expected_current_document_sha256: Option<String>,
    expected_current_credential_generation: Option<i64>,
    proxy_url: Option<String>,
    document: Value,
}

pub(in crate::api) async fn import_native_cursor_oauth(
    State(state): State<AppState>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "upstreams:import:write").await?;
    require_global_service(&service)?;
    let body: NativeCursorRequest = serde_json::from_slice(&bytes)
        .map_err(|_| AppError::BadRequest("native Cursor source request is invalid".into()))?;
    if body.contract != "source-bound-native-cursor-v1"
        || body.tenant_external_id.trim().is_empty()
        || body.tenant_external_id.trim() != body.tenant_external_id
        || body.tenant_external_id.len() > 200
        || body.tenant_external_id.chars().any(char::is_control)
        || !valid_relative_path(&body.source_relative_path)
        || body.source_layout.is_empty()
        || body.source_layout.len() > 200
        || body.source_layout.chars().any(char::is_control)
    {
        return Err(AppError::BadRequest(
            "native Cursor source contract is invalid".into(),
        ));
    }
    validate_lower_hex_digest(&body.source_identity_hash, "source identity")?;
    validate_lower_hex_digest(&body.source_document_sha256, "source document")?;
    match (
        body.expected_current_account_id,
        &body.expected_current_document_sha256,
        body.expected_current_credential_generation,
    ) {
        (None, None, None) => {}
        (Some(_), Some(digest), Some(generation)) if generation > 0 => {
            validate_lower_hex_digest(digest, "expected source document")?
        }
        _ => {
            return Err(AppError::BadRequest(
                "native Cursor current-state CAS is incomplete".into(),
            ));
        }
    }
    let document =
        serde_json::to_vec(&canonical_json(&body.document)).map_err(|_| AppError::Internal)?;
    if document.len() > 1024 * 1024
        || format!("{:x}", Sha256::digest(&document)) != body.source_document_sha256
    {
        return Err(AppError::BadRequest(
            "native Cursor source document digest does not match".into(),
        ));
    }
    // Reuse credential validation and the same proxy policy as native Cursor
    // login. The source's bridge service URL is never used as a network proxy.
    let credential =
        crate::oauth::credential_from_cursor_auth_store(&body.document, body.proxy_url)?;
    if let Some((proxy, _)) = credential.proxy() {
        crate::provider::validate_oauth_remote_dns_proxy_url(proxy, false)?;
    }
    let subject = crate::oauth::cursor_account_id(&credential)?;
    let pepper = state.config.key_pepper.as_bytes();
    let provider_subject_hash = keyed_digest(
        pepper,
        b"memeloop:native-cursor-subject:v1\0",
        &json!({"tenant": body.tenant_external_id, "subject": subject}),
    )?;
    let payload_digest = keyed_digest(
        pepper,
        b"memeloop:native-cursor-source:v1\0",
        &json!({
            "tenant": body.tenant_external_id,
            "source_identity": body.source_identity_hash,
            "source_document": body.source_document_sha256,
            "source_layout": body.source_layout,
            "source_relative_path": body.source_relative_path,
            "credential": credential,
        }),
    )?;
    let result = state
        .db
        .import_native_cursor_source(
            crate::db::NativeCursorImportInput {
                tenant_external_id: body.tenant_external_id,
                source_identity_hash: body.source_identity_hash,
                provider_subject_hash,
                source_document_sha256: body.source_document_sha256,
                payload_digest,
                source_layout: body.source_layout,
                account_name: body.account_name,
                expected_account_id: body.expected_current_account_id,
                expected_document_sha256: body.expected_current_document_sha256,
                expected_credential_generation: body.expected_current_credential_generation,
                credential,
            },
            pepper,
        )
        .await?;
    let status = if result.disposition == "created" {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((
        status,
        Json(json!({
            "disposition": result.disposition,
            "account": super::config_secrets::public_account(&state, result.account)?,
        })),
    ))
}

fn keyed_digest(pepper: &[u8], domain: &[u8], value: &Value) -> Result<String, AppError> {
    let mut mac = Hmac::<Sha256>::new_from_slice(pepper).map_err(|_| AppError::Internal)?;
    mac.update(domain);
    mac.update(&serde_json::to_vec(&canonical_json(value)).map_err(|_| AppError::Internal)?);
    Ok(format!("{:x}", mac.finalize().into_bytes()))
}
