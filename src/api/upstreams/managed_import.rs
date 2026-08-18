use hmac::{Hmac, Mac};
use sha2::Sha256;

use super::super::*;
use super::accounts::{validate_provider_schema, validate_upstream_destination};
use crate::{
    db::{ImportManagedOAuthAccountInput, ManagedOAuthImportStatus},
    oauth::normalize_managed_oauth_document,
};

const MAX_MANAGED_OAUTH_DOCUMENT: usize = 1024 * 1024;
pub(in crate::api) const MAX_MANAGED_OAUTH_IMPORT_REQUEST: usize =
    MAX_MANAGED_OAUTH_DOCUMENT + 64 * 1024;

const SOURCE_KEY_DOMAIN: &[u8] = b"memeloop:cpa-managed-oauth:source-key:v1\0";
const PAYLOAD_DIGEST_DOMAIN: &[u8] = b"memeloop:cpa-managed-oauth:payload-digest:v1\0";
const MANAGED_OAUTH_IMPORT_CONTRACT_VERSION: u8 = 1;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct ImportCpaManagedOAuthRequest {
    contract_version: u8,
    tenant_external_id: String,
    source: ManagedOAuthImportSource,
    source_type: String,
    document: Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManagedOAuthImportSource {
    kind: ManagedOAuthImportSourceKind,
    relative_path: String,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ManagedOAuthImportSourceKind {
    AuthFile,
}

impl ManagedOAuthImportSourceKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::AuthFile => "auth_file",
        }
    }
}

pub(in crate::api) async fn import_cpa_managed_oauth(
    State(state): State<AppState>,
    headers: HeaderMap,
    request_body: Bytes,
) -> Result<Response, AppError> {
    let service = require_service(&headers, &state, "imports:cpa:write").await?;
    require_global_service(&service)?;
    let body: ImportCpaManagedOAuthRequest = serde_json::from_slice(&request_body)
        .map_err(|_| AppError::BadRequest("managed OAuth import request is invalid".into()))?;

    validate_request_structure(&body)?;
    let normalized_path = validate_posix_relative_path(&body.source.relative_path)?;
    let canonical_document = checked_canonical_document(&body.document)?;

    let source_key = source_key(
        state.config.key_pepper.as_bytes(),
        &body.tenant_external_id,
        body.source.kind,
        normalized_path,
    )?;
    let payload_digest = payload_digest(
        state.config.key_pepper.as_bytes(),
        body.contract_version,
        &body.source_type,
        &canonical_document,
    )?;

    if let Some(account) = state
        .db
        .lookup_cpa_managed_oauth_import(&body.tenant_external_id, &source_key, &payload_digest)
        .await?
    {
        return Ok((
            StatusCode::OK,
            Json(json!({"disposition": "replayed", "account": account})),
        )
            .into_response());
    }

    // Resolution occurs only after the immutable provenance lookup. Exact
    // replays therefore do not depend on the current catalog or adapter.
    let adapter = state
        .providers
        .managed_oauth_adapter_for_source(&body.source_type)?;
    let normalized = normalize_managed_oauth_document(
        &state.http,
        &adapter,
        &body.document,
        state.config.allow_oauth_loopback,
    )
    .await?;

    validate_provider_schema(
        &state,
        adapter.provider_driver(),
        &normalized.config,
        &normalized.credential,
    )
    .map_err(|_| invalid_adapter_result())?;
    normalized
        .credential
        .validate(i64::MIN)
        .map_err(|_| invalid_adapter_result())?;
    validate_upstream_destination(
        adapter.provider_driver(),
        &normalized.config,
        &service,
        &state,
    )
    .await
    .map_err(|_| invalid_adapter_result())?;

    let status = initial_status(
        normalized.enabled,
        &normalized.credential,
        adapter.can_refresh(),
        unix_millis(),
    )?;
    let imported = state
        .db
        .import_cpa_managed_oauth_account(
            ImportManagedOAuthAccountInput {
                tenant_external_id: body.tenant_external_id,
                source_key,
                payload_digest,
                contract_version: i64::from(body.contract_version),
                account_name: normalized.account_name,
                config: normalized.config,
                credential: normalized.credential,
                status,
                adapter,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await?;
    let (status, disposition) = if imported.replayed {
        (StatusCode::OK, "replayed")
    } else {
        (StatusCode::CREATED, "created")
    };
    Ok((
        status,
        Json(json!({"disposition": disposition, "account": imported.account})),
    )
        .into_response())
}

pub(in crate::api) async fn cpa_managed_oauth_capabilities(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "imports:cpa:write").await?;
    require_global_service(&service)?;
    let source_types = state.providers.managed_oauth_source_types();
    Ok(Json(json!({
        "contract_version": MANAGED_OAUTH_IMPORT_CONTRACT_VERSION,
        "source_types": source_types,
    })))
}

fn validate_request_structure(body: &ImportCpaManagedOAuthRequest) -> Result<(), AppError> {
    if body.contract_version != MANAGED_OAUTH_IMPORT_CONTRACT_VERSION {
        return Err(AppError::BadRequest(
            "unsupported managed OAuth import contract version".into(),
        ));
    }
    if body.tenant_external_id.trim().is_empty()
        || body.tenant_external_id.len() > 200
        || body.tenant_external_id.chars().any(char::is_control)
    {
        return Err(AppError::BadRequest(
            "tenant_external_id must contain 1 to 200 non-control characters".into(),
        ));
    }
    if body.source_type.is_empty()
        || body.source_type.len() > 64
        || !body.source_type.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        })
    {
        return Err(AppError::BadRequest(
            "managed OAuth source type is invalid".into(),
        ));
    }
    Ok(())
}

fn validate_posix_relative_path(path: &str) -> Result<&str, AppError> {
    if path.is_empty()
        || path.len() > 512
        || path.starts_with('/')
        || path.contains('\\')
        || path.chars().any(char::is_control)
        || path
            .split('/')
            .any(|segment| segment.is_empty() || matches!(segment, "." | ".."))
    {
        return Err(AppError::BadRequest(
            "managed OAuth source path is invalid".into(),
        ));
    }
    Ok(path)
}

fn checked_canonical_document(document: &Value) -> Result<Value, AppError> {
    let canonical = canonical_json(document);
    let encoded = serde_json::to_vec(&canonical)
        .map_err(|_| AppError::BadRequest("managed OAuth document is invalid".into()))?;
    if encoded.len() > MAX_MANAGED_OAUTH_DOCUMENT {
        return Err(AppError::BadRequest(
            "managed OAuth document exceeds 1 MiB".into(),
        ));
    }
    Ok(canonical)
}

fn source_key(
    pepper: &[u8],
    tenant_external_id: &str,
    kind: ManagedOAuthImportSourceKind,
    normalized_path: &str,
) -> Result<String, AppError> {
    let mut mac = Hmac::<Sha256>::new_from_slice(pepper).map_err(|_| AppError::Internal)?;
    mac.update(SOURCE_KEY_DOMAIN);
    mac.update(tenant_external_id.as_bytes());
    mac.update(b"\0");
    mac.update(kind.as_str().as_bytes());
    mac.update(b"\0");
    mac.update(normalized_path.as_bytes());
    let digest = mac.finalize().into_bytes();
    Ok(lower_hex(&digest))
}

fn payload_digest(
    pepper: &[u8],
    contract_version: u8,
    source_type: &str,
    canonical_document: &Value,
) -> Result<String, AppError> {
    let canonical_payload = canonical_json(&json!({
        "contract_version": contract_version,
        "source_type": source_type,
        "document": canonical_document,
    }));
    let encoded = serde_json::to_vec(&canonical_payload).map_err(|_| AppError::Internal)?;
    let mut mac = Hmac::<Sha256>::new_from_slice(pepper).map_err(|_| AppError::Internal)?;
    mac.update(PAYLOAD_DIGEST_DOMAIN);
    mac.update(&encoded);
    let digest = mac.finalize().into_bytes();
    Ok(lower_hex(&digest))
}

fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.iter().map(canonical_json).collect()),
        Value::Object(values) => {
            let mut entries: Vec<_> = values.iter().collect();
            entries.sort_unstable_by_key(|(key, _)| *key);
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key.clone(), canonical_json(value)))
                    .collect(),
            )
        }
        _ => value.clone(),
    }
}

fn lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[usize::from(byte >> 4)] as char);
        encoded.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    encoded
}

fn initial_status(
    enabled: bool,
    credential: &UpstreamCredential,
    can_refresh: bool,
    now: i64,
) -> Result<ManagedOAuthImportStatus, AppError> {
    let expired = credential
        .expires_at()
        .is_some_and(|expires_at| expires_at <= now);
    if expired && can_refresh && !credential.has_oauth_refresh_state() {
        return Err(AppError::BadRequest(
            "expired managed OAuth credential has no refresh state".into(),
        ));
    }
    Ok(match (enabled, expired, can_refresh) {
        (true, false, _) => ManagedOAuthImportStatus::Active,
        (true, true, true) => ManagedOAuthImportStatus::RefreshRequired,
        (true, true, false) | (false, _, _) => ManagedOAuthImportStatus::Disabled,
    })
}

fn invalid_adapter_result() -> AppError {
    AppError::Upstream("managed OAuth adapter returned an invalid result".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths_are_strict_posix_and_errors_do_not_echo_them() {
        assert_eq!(
            validate_posix_relative_path("accounts/codex.json").unwrap(),
            "accounts/codex.json"
        );
        for rejected in [
            "",
            "/root.json",
            "accounts//codex.json",
            "./codex.json",
            "accounts/../codex.json",
            "accounts\\codex.json",
            "accounts/secret\0.json",
        ] {
            let error = validate_posix_relative_path(rejected).unwrap_err();
            if !rejected.is_empty() {
                assert!(!error.to_string().contains(rejected));
            }
        }
        assert!(validate_posix_relative_path(&"x".repeat(513)).is_err());
    }

    #[test]
    fn canonical_digest_sorts_objects_and_preserves_array_order() {
        let left = json!({"z": [{"b": 2, "a": 1}, 3], "a": true});
        let right = serde_json::from_str::<Value>(r#"{"a":true,"z":[{"a":1,"b":2},3]}"#).unwrap();
        let reordered = payload_digest(b"pepper", 1, "codex-account", &left).unwrap();
        assert_eq!(
            reordered,
            payload_digest(b"pepper", 1, "codex-account", &right).unwrap()
        );
        let array_changed = json!({"z": [3, {"a": 1, "b": 2}], "a": true});
        assert_ne!(
            reordered,
            payload_digest(b"pepper", 1, "codex-account", &array_changed).unwrap()
        );
    }

    #[test]
    fn document_and_request_limits_leave_bounded_envelope_headroom() {
        let exact = Value::String("x".repeat(MAX_MANAGED_OAUTH_DOCUMENT - 2));
        assert!(checked_canonical_document(&exact).is_ok());
        let oversized = Value::String("x".repeat(MAX_MANAGED_OAUTH_DOCUMENT - 1));
        let error = checked_canonical_document(&oversized).unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid request: managed OAuth document exceeds 1 MiB"
        );
        assert_eq!(
            MAX_MANAGED_OAUTH_IMPORT_REQUEST,
            MAX_MANAGED_OAUTH_DOCUMENT + 64 * 1024
        );
    }
}
