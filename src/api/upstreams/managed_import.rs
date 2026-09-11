use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use super::super::*;
use super::accounts::{validate_provider_schema, validate_upstream_destination};
use crate::{
    db::{ImportManagedOAuthAccountInput, ManagedOAuthImportStatus},
    oauth::normalize_managed_oauth_document,
};

const MAX_MANAGED_OAUTH_DOCUMENT: usize = 1024 * 1024;
pub(in crate::api) const MAX_MANAGED_OAUTH_IMPORT_REQUEST: usize =
    MAX_MANAGED_OAUTH_DOCUMENT + 64 * 1024;
pub(in crate::api) const MAX_MANAGED_OAUTH_COHORT_REQUEST: usize =
    2 * MAX_MANAGED_OAUTH_DOCUMENT + 64 * 1024;

const SOURCE_KEY_DOMAIN: &[u8] = b"memeloop:cpa-managed-oauth:source-key:v1\0";
const SOURCE_IDENTITY_KEY_DOMAIN: &[u8] =
    b"memeloop:cpa-managed-oauth:operator-source-identity:v1\0";
const PAYLOAD_DIGEST_DOMAIN: &[u8] = b"memeloop:cpa-managed-oauth:payload-digest:v1\0";
const MANAGED_OAUTH_IMPORT_CONTRACT_VERSION: u8 = 1;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct ImportCpaManagedOAuthRequest {
    contract_version: u8,
    tenant_external_id: String,
    source: ManagedOAuthImportSource,
    source_type: String,
    #[serde(default)]
    source_identity_hash: Option<String>,
    #[serde(default)]
    source_document_sha256: Option<String>,
    document: Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManagedOAuthImportSource {
    kind: ManagedOAuthImportSourceKind,
    relative_path: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct ImportCpaManagedKimiCohortRequest {
    contract_version: u8,
    tenant_external_id: String,
    cohort_contract: String,
    accounts: Vec<ManagedKimiCohortAccount>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManagedKimiCohortAccount {
    source: ManagedOAuthImportSource,
    source_type: String,
    source_identity_hash: String,
    source_document_sha256: String,
    document: Value,
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

    let canonical_document_sha256 = document_sha256(&canonical_document)?;
    if body
        .source_document_sha256
        .as_deref()
        .is_some_and(|expected| expected != canonical_document_sha256)
    {
        return Err(AppError::BadRequest(
            "managed OAuth source document SHA-256 does not match the canonical document".into(),
        ));
    }

    let source_key = match body.source_identity_hash.as_deref() {
        Some(identity) => source_key_from_operator_identity(
            state.config.key_pepper.as_bytes(),
            &body.tenant_external_id,
            identity,
        )?,
        None => source_key(
            state.config.key_pepper.as_bytes(),
            &body.tenant_external_id,
            body.source.kind,
            normalized_path,
        )?,
    };
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
    validate_managed_import_destination(
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
    let account_name = managed_account_name(
        adapter.provider_driver(),
        &normalized.account_name,
        &source_key,
    );
    let imported = state
        .db
        .import_cpa_managed_oauth_account(
            ImportManagedOAuthAccountInput {
                tenant_external_id: body.tenant_external_id,
                source_key,
                payload_digest,
                source_identity_hash: body.source_identity_hash,
                source_document_sha256: body.source_document_sha256,
                contract_version: i64::from(body.contract_version),
                account_name,
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
    } else if imported.updated {
        (StatusCode::OK, "updated")
    } else {
        (StatusCode::CREATED, "created")
    };
    Ok((
        status,
        Json(json!({"disposition": disposition, "account": imported.account})),
    )
        .into_response())
}

pub(in crate::api) async fn import_cpa_managed_kimi_cohort(
    State(state): State<AppState>,
    headers: HeaderMap,
    request_body: Bytes,
) -> Result<Response, AppError> {
    let service = require_service(&headers, &state, "imports:cpa:write").await?;
    require_global_service(&service)?;
    let body: ImportCpaManagedKimiCohortRequest = serde_json::from_slice(&request_body)
        .map_err(|_| AppError::BadRequest("managed Kimi cohort request is invalid".into()))?;
    validate_tenant_and_version(body.contract_version, &body.tenant_external_id)?;
    if body.cohort_contract != "atomic_kimi_cohort_v1" || body.accounts.len() != 2 {
        return Err(AppError::BadRequest(
            "managed Kimi cohort contract is invalid".into(),
        ));
    }

    let mut identities = std::collections::BTreeSet::new();
    let mut inputs = Vec::with_capacity(2);
    for account in body.accounts {
        if account.source_type != "kimi" || !identities.insert(account.source_identity_hash.clone())
        {
            return Err(AppError::BadRequest(
                "managed Kimi cohort identities are invalid".into(),
            ));
        }
        validate_lowercase_sha256(&account.source_identity_hash)?;
        validate_lowercase_sha256(&account.source_document_sha256)?;
        let _ = validate_posix_relative_path(&account.source.relative_path)?;
        let canonical_document = checked_canonical_document(&account.document)?;
        if document_sha256(&canonical_document)? != account.source_document_sha256 {
            return Err(AppError::BadRequest(
                "managed OAuth source document SHA-256 does not match the canonical document"
                    .into(),
            ));
        }
        let source_key = source_key_from_operator_identity(
            state.config.key_pepper.as_bytes(),
            &body.tenant_external_id,
            &account.source_identity_hash,
        )?;
        let payload_digest = payload_digest(
            state.config.key_pepper.as_bytes(),
            body.contract_version,
            &account.source_type,
            &canonical_document,
        )?;
        let adapter = state
            .providers
            .managed_oauth_adapter_for_source(&account.source_type)?;
        let normalized = normalize_managed_oauth_document(
            &state.http,
            &adapter,
            &account.document,
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
        validate_managed_import_destination(
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
        inputs.push(ImportManagedOAuthAccountInput {
            tenant_external_id: body.tenant_external_id.clone(),
            source_key: source_key.clone(),
            payload_digest,
            source_identity_hash: Some(account.source_identity_hash),
            source_document_sha256: Some(account.source_document_sha256),
            contract_version: i64::from(body.contract_version),
            account_name: managed_account_name(
                adapter.provider_driver(),
                &normalized.account_name,
                &source_key,
            ),
            config: normalized.config,
            credential: normalized.credential,
            status,
            adapter,
        });
    }

    let imported = state
        .db
        .import_cpa_managed_kimi_cohort(inputs, state.config.key_pepper.as_bytes())
        .await?;
    let disposition = match imported.created {
        0 => "replayed",
        2 => "created",
        _ => "converged",
    };
    let status = if imported.created == 2 {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((
        status,
        Json(json!({"disposition": disposition, "accounts": imported.accounts})),
    )
        .into_response())
}

async fn validate_managed_import_destination(
    driver: &str,
    config: &Value,
    service: &crate::model::AuthenticatedService,
    state: &AppState,
) -> Result<(), AppError> {
    if driver == crate::oauth::managed::kimi::PROVIDER_DRIVER {
        if crate::provider::validate_config(config)? != crate::oauth::managed::kimi::BASE_URL
            || crate::network::scope_from_config(config) != crate::network::OutboundScope::Public
        {
            return Err(invalid_adapter_result());
        }
        return Ok(());
    }
    validate_upstream_destination(driver, config, service, state).await
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
        "source_identity_contract": "operator-hmac-sha256-v1",
        "account_name_policies": {
            "kimi": "neutral-server-keyed-source-suffix-v1"
        },
        "atomic_cohort_contracts": ["atomic_kimi_cohort_v1"],
        "credential_envelope_contract": "chacha20poly1305-hkdf-sha256-v2-aad-v1",
    })))
}

fn validate_request_structure(body: &ImportCpaManagedOAuthRequest) -> Result<(), AppError> {
    validate_tenant_and_version(body.contract_version, &body.tenant_external_id)?;
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
    match (
        body.source_identity_hash.as_deref(),
        body.source_document_sha256.as_deref(),
    ) {
        (Some(identity), Some(document)) => {
            validate_lowercase_sha256(identity)?;
            validate_lowercase_sha256(document)?;
        }
        (None, None) => {}
        _ => {
            return Err(AppError::BadRequest(
                "managed OAuth source fingerprints must be supplied together".into(),
            ));
        }
    }
    Ok(())
}

fn validate_tenant_and_version(
    contract_version: u8,
    tenant_external_id: &str,
) -> Result<(), AppError> {
    if contract_version != MANAGED_OAUTH_IMPORT_CONTRACT_VERSION {
        return Err(AppError::BadRequest(
            "unsupported managed OAuth import contract version".into(),
        ));
    }
    if tenant_external_id.trim().is_empty()
        || tenant_external_id.len() > 200
        || tenant_external_id.chars().any(char::is_control)
    {
        return Err(AppError::BadRequest(
            "tenant_external_id must contain 1 to 200 non-control characters".into(),
        ));
    }
    Ok(())
}

fn validate_lowercase_sha256(value: &str) -> Result<(), AppError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(AppError::BadRequest(
            "managed OAuth source fingerprint must be lowercase SHA-256 hex".into(),
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

fn source_key_from_operator_identity(
    pepper: &[u8],
    tenant_external_id: &str,
    source_identity_hash: &str,
) -> Result<String, AppError> {
    let mut mac = Hmac::<Sha256>::new_from_slice(pepper).map_err(|_| AppError::Internal)?;
    mac.update(SOURCE_IDENTITY_KEY_DOMAIN);
    mac.update(tenant_external_id.as_bytes());
    mac.update(b"\0");
    mac.update(source_identity_hash.as_bytes());
    Ok(lower_hex(&mac.finalize().into_bytes()))
}

fn document_sha256(canonical_document: &Value) -> Result<String, AppError> {
    let encoded = serde_json::to_vec(canonical_document).map_err(|_| AppError::Internal)?;
    Ok(lower_hex(&Sha256::digest(encoded)))
}

fn managed_account_name(provider_driver: &str, adapter_name: &str, source_key: &str) -> String {
    if provider_driver == crate::oauth::managed::kimi::PROVIDER_DRIVER {
        // The complete server-keyed identity avoids exposing a source path,
        // email, device id, or credential-derived value while making distinct
        // Kimi auth files deterministically tenant-unique.
        format!("Kimi account {source_key}")
    } else {
        adapter_name.to_owned()
    }
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
