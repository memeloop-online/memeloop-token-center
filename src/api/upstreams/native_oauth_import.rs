use std::{collections::BTreeMap, path::Component};

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::super::*;
use crate::db::{
    NativeOAuthImportAccountInput, NativeOAuthImportApproval, NativeOAuthImportCohortResult,
};

const CONTRACT_VERSION: i64 = 2;
const KIMI_COHORT_CONTRACT: &str = "atomic_kimi_cohort_v2";
const APPROVAL_CONTRACT: &str = "kimi-cohort-digest-pair-v1";
const SOURCE_IDENTITY_CONTRACT: &str = "operator-hmac-sha256-v1";
const ACCOUNT_NAME_POLICY: &str = "neutral-server-keyed-source-suffix-v1";
const CREDENTIAL_ENVELOPE_CONTRACT: &str = "chacha20poly1305-hkdf-sha256-v2-aad-v1";
const PAYLOAD_DIGEST_DOMAIN: &[u8] = b"memeloop:native-oauth-import:payload-digest:v2\0";
const MAX_NATIVE_OAUTH_DOCUMENT: usize = 1024 * 1024;
pub(in crate::api) const MAX_NATIVE_KIMI_COHORT_REQUEST: usize =
    2 * MAX_NATIVE_OAUTH_DOCUMENT + 64 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct NativeKimiCohortRequest {
    contract_version: i64,
    tenant_external_id: String,
    cohort_contract: String,
    approval: NativeKimiCohortApproval,
    accounts: Vec<NativeKimiCohortAccount>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeKimiCohortApproval {
    contract: String,
    expected_current_cohort_sha256: String,
    new_cohort_sha256: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeKimiCohortAccount {
    source: NativeOAuthImportSource,
    source_type: String,
    source_identity_hash: String,
    source_document_sha256: String,
    expected_current_account_id: Option<Uuid>,
    expected_current_document_sha256: Option<String>,
    expected_current_credential_generation: Option<i64>,
    document: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeOAuthImportSource {
    kind: String,
    relative_path: String,
}

#[derive(Serialize)]
struct ExpectedCohortDigestEntry<'a> {
    source_identity_hash: &'a str,
    account_id: Option<Uuid>,
    source_document_sha256: Option<&'a str>,
    credential_generation: Option<i64>,
}

#[derive(Serialize)]
struct NewCohortDigestEntry<'a> {
    source_identity_hash: &'a str,
    source_document_sha256: &'a str,
}

pub(in crate::api) async fn native_oauth_import_capabilities(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "upstreams:import:write").await?;
    require_global_service(&service)?;
    Ok(Json(json!({
        "contract_version": CONTRACT_VERSION,
        "source_types": ["kimi"],
        "source_identity_contract": SOURCE_IDENTITY_CONTRACT,
        "account_name_policies": {"kimi": ACCOUNT_NAME_POLICY},
        "atomic_cohort_contracts": [KIMI_COHORT_CONTRACT],
        "credential_envelope_contract": CREDENTIAL_ENVELOPE_CONTRACT,
    })))
}

pub(in crate::api) async fn import_native_kimi_oauth_cohort(
    State(state): State<AppState>,
    headers: HeaderMap,
    request_body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "upstreams:import:write").await?;
    require_global_service(&service)?;
    let body: NativeKimiCohortRequest = serde_json::from_slice(&request_body)
        .map_err(|_| AppError::BadRequest("native Kimi OAuth cohort request is invalid".into()))?;
    validate_outer_contract(&body)?;

    let expected_digest = cohort_digest(
        &body
            .accounts
            .iter()
            .map(|account| ExpectedCohortDigestEntry {
                source_identity_hash: &account.source_identity_hash,
                account_id: account.expected_current_account_id,
                source_document_sha256: account.expected_current_document_sha256.as_deref(),
                credential_generation: account.expected_current_credential_generation,
            })
            .collect::<Vec<_>>(),
    )?;
    let new_digest = cohort_digest(
        &body
            .accounts
            .iter()
            .map(|account| NewCohortDigestEntry {
                source_identity_hash: &account.source_identity_hash,
                source_document_sha256: &account.source_document_sha256,
            })
            .collect::<Vec<_>>(),
    )?;
    if body.approval.expected_current_cohort_sha256 != expected_digest
        || body.approval.new_cohort_sha256 != new_digest
    {
        return Err(AppError::Conflict(
            "native Kimi OAuth cohort approval does not match the request".into(),
        ));
    }

    let mut ordinal_by_identity = body
        .accounts
        .iter()
        .map(|account| account.source_identity_hash.clone())
        .collect::<Vec<_>>();
    ordinal_by_identity.sort_unstable();
    ordinal_by_identity.dedup();
    if ordinal_by_identity.len() != 2 {
        return Err(AppError::BadRequest(
            "native Kimi OAuth cohort source identities must be distinct".into(),
        ));
    }

    let mut inputs = Vec::with_capacity(2);
    for account in body.accounts {
        validate_source(&account)?;
        let canonical_document = canonical_json(&account.document);
        let document_bytes =
            serde_json::to_vec(&canonical_document).map_err(|_| AppError::Internal)?;
        if document_bytes.len() > MAX_NATIVE_OAUTH_DOCUMENT {
            return Err(AppError::BadRequest(
                "native Kimi OAuth source document exceeds the supported limit".into(),
            ));
        }
        if account.source_document_sha256 != format!("{:x}", Sha256::digest(&document_bytes)) {
            return Err(AppError::Conflict(
                "native Kimi OAuth source document digest does not match".into(),
            ));
        }
        let credential =
            crate::oauth::managed::kimi::credential_from_native_import(&account.document)?;
        let ordinal = ordinal_by_identity
            .binary_search(&account.source_identity_hash)
            .map_err(|_| AppError::Internal)? as i64
            + 1;
        let payload_digest = payload_digest(
            state.config.key_pepper.as_bytes(),
            &body.tenant_external_id,
            &account.source_identity_hash,
            &account.source_document_sha256,
            &credential,
        )?;
        inputs.push(NativeOAuthImportAccountInput {
            tenant_external_id: body.tenant_external_id.clone(),
            ordinal,
            source_identity_hash: account.source_identity_hash,
            source_document_sha256: account.source_document_sha256,
            payload_digest,
            expected_current_account_id: account
                .expected_current_account_id
                .map(|account_id| account_id.to_string()),
            expected_current_document_sha256: account.expected_current_document_sha256,
            expected_current_credential_generation: account
                .expected_current_credential_generation,
            account_name: format!("Kimi OAuth {ordinal}"),
            config: crate::oauth::managed::kimi::native_import_config(),
            credential,
        });
    }

    let result = state
        .db
        .import_native_kimi_oauth_cohort(
            inputs,
            NativeOAuthImportApproval {
                expected_current_cohort_sha256: body
                    .approval
                    .expected_current_cohort_sha256,
                new_cohort_sha256: body.approval.new_cohort_sha256,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await?;
    let (status, disposition) = cohort_disposition(&result)?;
    tracing::info!(
        tenant_external_id = %body.tenant_external_id,
        disposition,
        account_ids = ?result.accounts.iter().map(|account| account.id).collect::<Vec<_>>(),
        "applied native Kimi OAuth cohort import"
    );
    Ok((status, Json(json!({
        "disposition": disposition,
        "accounts": result.accounts,
    }))))
}

fn validate_outer_contract(body: &NativeKimiCohortRequest) -> Result<(), AppError> {
    if body.contract_version != CONTRACT_VERSION
        || body.cohort_contract != KIMI_COHORT_CONTRACT
        || body.approval.contract != APPROVAL_CONTRACT
        || body.accounts.len() != 2
        || body.tenant_external_id.trim().is_empty()
        || body.tenant_external_id.len() > 200
        || body.tenant_external_id.chars().any(char::is_control)
    {
        return Err(AppError::BadRequest(
            "native Kimi OAuth cohort contract is invalid".into(),
        ));
    }
    validate_lower_hex_digest(
        &body.approval.expected_current_cohort_sha256,
        "expected cohort digest",
    )?;
    validate_lower_hex_digest(&body.approval.new_cohort_sha256, "new cohort digest")
}

fn validate_source(account: &NativeKimiCohortAccount) -> Result<(), AppError> {
    validate_lower_hex_digest(&account.source_identity_hash, "source identity")?;
    validate_lower_hex_digest(&account.source_document_sha256, "source document")?;
    let cas_count = [
        account.expected_current_account_id.is_some(),
        account.expected_current_document_sha256.is_some(),
        account.expected_current_credential_generation.is_some(),
    ]
    .into_iter()
    .filter(|present| *present)
    .count();
    if account.source_type != "kimi"
        || account.source.kind != "auth_file"
        || !valid_relative_path(&account.source.relative_path)
        || !matches!(cas_count, 0 | 3)
        || account
            .expected_current_credential_generation
            .is_some_and(|generation| generation < 1)
    {
        return Err(AppError::BadRequest(
            "native Kimi OAuth source contract is invalid".into(),
        ));
    }
    if let Some(document) = account.expected_current_document_sha256.as_deref() {
        validate_lower_hex_digest(document, "expected source document")?;
    }
    Ok(())
}

fn valid_relative_path(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && !value.contains('\\')
        && !value.chars().any(char::is_control)
        && std::path::Path::new(value)
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn validate_lower_hex_digest(value: &str, label: &str) -> Result<(), AppError> {
    if value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(AppError::BadRequest(format!(
            "native OAuth {label} must be lowercase SHA-256 hex"
        )))
    }
}

fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.iter().map(canonical_json).collect()),
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| (key.clone(), canonical_json(value)))
                .collect::<BTreeMap<_, _>>()
                .into_iter()
                .collect(),
        ),
        value => value.clone(),
    }
}

fn cohort_digest<T: Serialize>(entries: &[T]) -> Result<String, AppError> {
    let value = serde_json::to_value(entries).map_err(|_| AppError::Internal)?;
    let bytes = serde_json::to_vec(&canonical_json(&value)).map_err(|_| AppError::Internal)?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn payload_digest(
    pepper: &[u8],
    tenant_external_id: &str,
    source_identity_hash: &str,
    source_document_sha256: &str,
    credential: &UpstreamCredential,
) -> Result<String, AppError> {
    let value = json!({
        "contract": KIMI_COHORT_CONTRACT,
        "credential": credential,
        "source_document_sha256": source_document_sha256,
        "source_identity_hash": source_identity_hash,
        "tenant_external_id": tenant_external_id,
    });
    let bytes = serde_json::to_vec(&canonical_json(&value)).map_err(|_| AppError::Internal)?;
    let mut mac = Hmac::<Sha256>::new_from_slice(pepper).map_err(|_| AppError::Internal)?;
    mac.update(PAYLOAD_DIGEST_DOMAIN);
    mac.update(&bytes);
    Ok(format!("{:x}", mac.finalize().into_bytes()))
}

fn cohort_disposition(
    result: &NativeOAuthImportCohortResult,
) -> Result<(StatusCode, &'static str), AppError> {
    match (result.created, result.rotated) {
        (2, 0) => Ok((StatusCode::CREATED, "created")),
        (1, 0) => Ok((StatusCode::OK, "converged")),
        (0, 0) => Ok((StatusCode::OK, "replayed")),
        (0, 2) => Ok((StatusCode::OK, "rotated")),
        _ => Err(AppError::Internal),
    }
}
