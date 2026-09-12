use super::super::*;
use super::accounts::{upstream_account_view, validate_upstream_account_name};

const KIMI_COHORT_CONTRACT: &str = "atomic_kimi_cohort_v2";
const NATIVE_OAUTH_TENANT_LOCK_SEED: i64 = 734_627_102_948_337;

#[derive(Clone)]
pub struct NativeOAuthImportAccountInput {
    pub tenant_external_id: String,
    pub ordinal: i64,
    pub source_identity_hash: String,
    pub source_document_sha256: String,
    pub payload_digest: String,
    pub expected_current_account_id: Option<String>,
    pub expected_current_document_sha256: Option<String>,
    pub expected_current_credential_generation: Option<i64>,
    pub account_name: String,
    pub config: Value,
    pub credential: UpstreamCredential,
}

#[derive(Clone, Debug)]
pub struct NativeOAuthImportApproval {
    pub expected_current_cohort_sha256: String,
    pub new_cohort_sha256: String,
}

#[derive(Clone, Debug)]
pub struct NativeOAuthImportCohortResult {
    pub accounts: Vec<UpstreamAccountView>,
    pub created: usize,
    pub rotated: usize,
}

impl Database {
    /// Apply the complete two-account Kimi cohort under one tenant lock and
    /// one database transaction. The result preserves request order.
    pub async fn import_native_kimi_oauth_cohort(
        &self,
        inputs: Vec<NativeOAuthImportAccountInput>,
        approval: NativeOAuthImportApproval,
        key_material: &[u8],
    ) -> Result<NativeOAuthImportCohortResult, AppError> {
        if inputs.len() != 2 {
            return Err(AppError::BadRequest(
                "native Kimi OAuth import requires exactly two accounts".into(),
            ));
        }
        validate_digest(
            &approval.expected_current_cohort_sha256,
            "expected cohort digest",
        )?;
        validate_digest(&approval.new_cohort_sha256, "new cohort digest")?;
        let now = unix_millis();
        let tenant_external_id = inputs[0].tenant_external_id.clone();
        if tenant_external_id.trim().is_empty()
            || tenant_external_id.trim() != tenant_external_id
            || tenant_external_id.len() > 200
            || tenant_external_id.chars().any(char::is_control)
        {
            return Err(AppError::BadRequest(
                "native Kimi OAuth tenant is invalid".into(),
            ));
        }
        let expected_config = crate::oauth::managed::kimi::native_import_config();
        let mut identities = std::collections::BTreeSet::new();
        let mut ordinals = std::collections::BTreeSet::new();
        for input in &inputs {
            validate_digest(&input.source_identity_hash, "source identity")?;
            validate_digest(&input.source_document_sha256, "source document")?;
            validate_digest(&input.payload_digest, "payload")?;
            validate_current_cas(input)?;
            validate_upstream_account_name(&input.account_name)?;
            input.credential.validate(i64::MIN)?;
            crate::oauth::managed::kimi::validate_credential(&input.credential)?;
            if input.tenant_external_id != tenant_external_id
                || input.config != expected_config
                || !matches!(input.ordinal, 1 | 2)
                || !ordinals.insert(input.ordinal)
                || !identities.insert(input.source_identity_hash.clone())
                || input.account_name != format!("Kimi OAuth {}", input.ordinal)
                || !input
                    .credential
                    .expires_at()
                    .is_some_and(|expires_at| expires_at > now)
            {
                return Err(AppError::BadRequest(
                    "native Kimi OAuth cohort is invalid or expired".into(),
                ));
            }
        }
        let mut sorted_identities = inputs
            .iter()
            .map(|input| input.source_identity_hash.as_str())
            .collect::<Vec<_>>();
        sorted_identities.sort_unstable();
        if inputs.iter().any(|input| {
            sorted_identities
                .binary_search(&input.source_identity_hash.as_str())
                .map(|index| index as i64 + 1)
                != Ok(input.ordinal)
        }) {
            return Err(AppError::BadRequest(
                "native Kimi OAuth account ordinals are not canonical".into(),
            ));
        }

        let mut prepared = Vec::with_capacity(2);
        for input in inputs {
            prepared.push((
                serde_json::to_string(&input.config).map_err(|_| AppError::Internal)?,
                seal_credential(&input.credential, key_material)?,
                input,
            ));
        }
        let mut tx = self.begin_write_transaction().await?;
        sqlx::query(
            "INSERT INTO tenants (id, external_id, created_at) VALUES ($1, $2, $3) ON CONFLICT(external_id) DO NOTHING",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(&tenant_external_id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        let tenant_id: String = sqlx::query("SELECT id FROM tenants WHERE external_id = $1")
            .bind(&tenant_external_id)
            .fetch_one(&mut *tx)
            .await?
            .try_get("id")?;
        lock_native_oauth_import_tenant(self.backend, &mut tx, &tenant_id).await?;

        let inventory_sql = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT a.id, receipt.source_identity_hash FROM upstream_accounts a LEFT JOIN native_oauth_import_receipts receipt ON receipt.tenant_id = a.tenant_id AND receipt.upstream_account_id = a.id WHERE a.tenant_id = $1 AND a.driver = $2 FOR UPDATE OF a"
            }
            DatabaseBackend::Sqlite => {
                "SELECT a.id, receipt.source_identity_hash FROM upstream_accounts a LEFT JOIN native_oauth_import_receipts receipt ON receipt.tenant_id = a.tenant_id AND receipt.upstream_account_id = a.id WHERE a.tenant_id = $1 AND a.driver = $2"
            }
        };
        let inventory = sqlx::query(inventory_sql)
            .bind(&tenant_id)
            .bind(crate::oauth::managed::kimi::PROVIDER_DRIVER)
            .fetch_all(&mut *tx)
            .await?;
        let requested: std::collections::BTreeSet<&str> = prepared
            .iter()
            .map(|(_, _, input)| input.source_identity_hash.as_str())
            .collect();
        let mut inventory_identities = std::collections::BTreeSet::new();
        for row in inventory {
            let identity = row
                .try_get::<Option<String>, _>("source_identity_hash")?
                .filter(|value| requested.contains(value.as_str()))
                .ok_or_else(|| {
                    AppError::Conflict(
                        "tenant Kimi inventory is outside the approved native cohort".into(),
                    )
                })?;
            if !inventory_identities.insert(identity) {
                return Err(AppError::Conflict(
                    "tenant Kimi inventory has duplicate import provenance".into(),
                ));
            }
        }

        let cohort = native_oauth_cohort_row(self.backend, &mut tx, &tenant_id).await?;
        let prior_approval_matches = if let Some(row) = cohort.as_ref() {
            row.try_get::<String, _>("expected_current_cohort_sha256")?
                == approval.expected_current_cohort_sha256
                && row.try_get::<String, _>("new_cohort_sha256")? == approval.new_cohort_sha256
        } else {
            false
        };
        let existing_cohort_id = cohort
            .as_ref()
            .map(|row| row.try_get::<String, _>("id"))
            .transpose()?;
        let mut exists = Vec::with_capacity(2);
        let mut changed = Vec::with_capacity(2);
        let mut current_cas_matches = Vec::with_capacity(2);
        for (_, _, input) in &prepared {
            let row = native_oauth_receipt_row(
                self.backend,
                &mut tx,
                &tenant_id,
                &input.source_identity_hash,
            )
            .await?;
            let Some(row) = row else {
                if input.expected_current_account_id.is_some()
                    || input.expected_current_document_sha256.is_some()
                    || input.expected_current_credential_generation.is_some()
                {
                    return Err(AppError::Conflict(
                        "native Kimi OAuth current-state CAS is stale".into(),
                    ));
                }
                exists.push(false);
                changed.push(false);
                current_cas_matches.push(false);
                continue;
            };
            if row.try_get::<String, _>("cohort_id")?
                != existing_cohort_id.as_deref().ok_or_else(|| {
                    AppError::Conflict("native Kimi OAuth receipt has no matching cohort".into())
                })?
            {
                return Err(AppError::Conflict(
                    "native Kimi OAuth receipt belongs to another cohort".into(),
                ));
            }
            validate_existing_kimi_account(&row, input, now, key_material)?;
            exists.push(true);
            changed.push(
                row.try_get::<String, _>("payload_digest")? != input.payload_digest
                    || row.try_get::<String, _>("import_source_document_sha256")?
                        != input.source_document_sha256,
            );
            current_cas_matches.push(current_cas_matches_account(&row, input)?);
        }

        let existing_count = exists.iter().filter(|value| **value).count();
        let changed_count = changed.iter().filter(|value| **value).count();
        let all_existing_cas_match = exists
            .iter()
            .zip(&current_cas_matches)
            .all(|(exists, matches)| !exists || *matches);
        let allowed = match (existing_count, changed_count) {
            (0, 0) => true,
            (1, 0) => all_existing_cas_match,
            (2, 0) => all_existing_cas_match || prior_approval_matches,
            (2, 2) => all_existing_cas_match,
            _ => false,
        };
        if !allowed || (existing_count == 0) != cohort.is_none() {
            return Err(AppError::Conflict(
                "native Kimi OAuth cohort cannot mix rotation, replay, or foreign state".into(),
            ));
        }

        let (cohort_id, cohort_updated_at) = if let Some(row) = cohort {
            (
                row.try_get::<String, _>("id")?,
                row.try_get::<i64, _>("updated_at")?,
            )
        } else {
            let id = Uuid::now_v7().to_string();
            sqlx::query(
                "INSERT INTO native_oauth_import_cohorts (id, tenant_id, contract, expected_current_cohort_sha256, new_cohort_sha256, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, $6, $6)",
            )
            .bind(&id)
            .bind(&tenant_id)
            .bind(KIMI_COHORT_CONTRACT)
            .bind(&approval.expected_current_cohort_sha256)
            .bind(&approval.new_cohort_sha256)
            .bind(now)
            .execute(&mut *tx)
            .await?;
            (id, now)
        };

        let mut accounts = Vec::with_capacity(2);
        let mut created = 0;
        let mut rotated = 0;
        for (index, (config_json, credential_ciphertext, input)) in prepared.into_iter().enumerate()
        {
            if exists[index] {
                let row = native_oauth_receipt_row(
                    self.backend,
                    &mut tx,
                    &tenant_id,
                    &input.source_identity_hash,
                )
                .await?
                .ok_or(AppError::Internal)?;
                if !changed[index] {
                    accounts.push(upstream_account_view(row)?);
                    continue;
                }
                rotate_native_kimi_credential(
                    &mut tx,
                    &tenant_id,
                    &input,
                    config_json,
                    credential_ciphertext,
                    now,
                    key_material,
                    &row,
                )
                .await?;
                let updated = native_oauth_receipt_row(
                    self.backend,
                    &mut tx,
                    &tenant_id,
                    &input.source_identity_hash,
                )
                .await?
                .ok_or(AppError::Internal)?;
                accounts.push(upstream_account_view(updated)?);
                rotated += 1;
                continue;
            }

            let account_id = Uuid::now_v7();
            if sqlx::query("SELECT id FROM upstream_accounts WHERE tenant_id = $1 AND name = $2")
                .bind(&tenant_id)
                .bind(&input.account_name)
                .fetch_optional(&mut *tx)
                .await?
                .is_some()
            {
                return Err(AppError::Conflict(
                    "another upstream provider already uses the approved Kimi name".into(),
                ));
            }
            let account_insert = sqlx::query(
                "INSERT INTO upstream_accounts (id, tenant_id, name, driver, auth_kind, config_json, status, credential_generation, oauth_session_id, oauth_driver, oauth_refresh_url, created_at, updated_at) VALUES ($1, $2, $3, $4, 'oauth', $5, 'active', 1, $1, $4, $6, $7, $7)",
            )
            .bind(account_id.to_string())
            .bind(&tenant_id)
            .bind(&input.account_name)
            .bind(crate::oauth::managed::kimi::PROVIDER_DRIVER)
            .bind(config_json)
            .bind(crate::oauth::managed::kimi::TOKEN_ENDPOINT)
            .bind(now)
            .execute(&mut *tx)
            .await;
            if let Err(error) = account_insert {
                if error
                    .as_database_error()
                    .is_some_and(|database_error| database_error.is_unique_violation())
                {
                    return Err(AppError::Conflict(
                        "another upstream provider already uses the approved Kimi name".into(),
                    ));
                }
                return Err(error.into());
            }
            sqlx::query(
                "INSERT INTO upstream_credentials (id, upstream_account_id, generation, credential_ciphertext, expires_at, created_at) VALUES ($1, $2, 1, $3, $4, $5)",
            )
            .bind(Uuid::now_v7().to_string())
            .bind(account_id.to_string())
            .bind(credential_ciphertext)
            .bind(input.credential.expires_at())
            .bind(now)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "INSERT INTO native_oauth_import_receipts (cohort_id, tenant_id, ordinal, source_identity_hash, source_document_sha256, payload_digest, upstream_account_id, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $8)",
            )
            .bind(&cohort_id)
            .bind(&tenant_id)
            .bind(input.ordinal)
            .bind(&input.source_identity_hash)
            .bind(&input.source_document_sha256)
            .bind(&input.payload_digest)
            .bind(account_id.to_string())
            .bind(now)
            .execute(&mut *tx)
            .await?;
            let inserted = native_oauth_receipt_row(
                self.backend,
                &mut tx,
                &tenant_id,
                &input.source_identity_hash,
            )
            .await?
            .ok_or(AppError::Internal)?;
            accounts.push(upstream_account_view(inserted)?);
            created += 1;
        }

        if created > 0 || rotated > 0 {
            let updated_at = now.max(cohort_updated_at.saturating_add(1));
            let updated = sqlx::query(
                "UPDATE native_oauth_import_cohorts SET expected_current_cohort_sha256 = $1, new_cohort_sha256 = $2, updated_at = $3 WHERE id = $4 AND tenant_id = $5 AND contract = $6 AND updated_at = $7",
            )
            .bind(&approval.expected_current_cohort_sha256)
            .bind(&approval.new_cohort_sha256)
            .bind(updated_at)
            .bind(&cohort_id)
            .bind(&tenant_id)
            .bind(KIMI_COHORT_CONTRACT)
            .bind(cohort_updated_at)
            .execute(&mut *tx)
            .await?;
            if updated.rows_affected() != 1 {
                return Err(AppError::Conflict(
                    "native Kimi OAuth cohort receipt changed during import".into(),
                ));
            }
        }
        tx.commit().await?;
        Ok(NativeOAuthImportCohortResult {
            accounts,
            created,
            rotated,
        })
    }
}

pub(super) async fn lock_native_oauth_import_tenant(
    backend: DatabaseBackend,
    tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    tenant_id: &str,
) -> Result<(), AppError> {
    if matches!(backend, DatabaseBackend::PostgreSql) {
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, $2))")
            .bind(tenant_id)
            .bind(NATIVE_OAUTH_TENANT_LOCK_SEED)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

fn validate_current_cas(input: &NativeOAuthImportAccountInput) -> Result<(), AppError> {
    match (
        input.expected_current_account_id.as_deref(),
        input.expected_current_document_sha256.as_deref(),
        input.expected_current_credential_generation,
    ) {
        (None, None, None) => Ok(()),
        (Some(account_id), Some(document), Some(generation))
            if Uuid::parse_str(account_id).is_ok() && generation >= 1 =>
        {
            validate_digest(document, "expected source document")
        }
        _ => Err(AppError::BadRequest(
            "native Kimi OAuth current-state CAS is invalid".into(),
        )),
    }
}

fn validate_digest(value: &str, label: &str) -> Result<(), AppError> {
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

async fn native_oauth_cohort_row(
    backend: DatabaseBackend,
    tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    tenant_id: &str,
) -> Result<Option<sqlx::any::AnyRow>, AppError> {
    let query = match backend {
        DatabaseBackend::PostgreSql => {
            "SELECT id, expected_current_cohort_sha256, new_cohort_sha256, updated_at FROM native_oauth_import_cohorts WHERE tenant_id = $1 AND contract = $2 FOR UPDATE"
        }
        DatabaseBackend::Sqlite => {
            "SELECT id, expected_current_cohort_sha256, new_cohort_sha256, updated_at FROM native_oauth_import_cohorts WHERE tenant_id = $1 AND contract = $2"
        }
    };
    Ok(sqlx::query(query)
        .bind(tenant_id)
        .bind(KIMI_COHORT_CONTRACT)
        .fetch_optional(&mut **tx)
        .await?)
}

async fn native_oauth_receipt_row(
    backend: DatabaseBackend,
    tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    tenant_id: &str,
    source_identity_hash: &str,
) -> Result<Option<sqlx::any::AnyRow>, AppError> {
    let query = match backend {
        DatabaseBackend::PostgreSql => {
            "SELECT receipt.cohort_id, receipt.ordinal, receipt.source_identity_hash AS import_source_identity_hash, receipt.source_document_sha256 AS import_source_document_sha256, receipt.payload_digest, a.id, a.tenant_id, t.external_id AS tenant_external_id, a.name, a.driver, a.auth_kind, a.config_json, a.status, a.credential_generation, a.oauth_session_id, a.oauth_driver, a.oauth_refresh_url, a.created_at, a.updated_at, c.expires_at, c.credential_ciphertext, (SELECT COUNT(*) FROM model_routes route WHERE route.tenant_id = a.tenant_id AND (route.upstream_account_id = a.id OR EXISTS (SELECT 1 FROM model_route_upstream_accounts association WHERE association.tenant_id = route.tenant_id AND association.model_route_id = route.id AND association.upstream_account_id = a.id))) AS route_count FROM native_oauth_import_receipts receipt JOIN upstream_accounts a ON a.id = receipt.upstream_account_id AND a.tenant_id = receipt.tenant_id JOIN tenants t ON t.id = a.tenant_id JOIN upstream_credentials c ON c.upstream_account_id = a.id AND c.generation = a.credential_generation AND c.revoked_at IS NULL WHERE receipt.tenant_id = $1 AND receipt.source_identity_hash = $2 FOR UPDATE OF receipt, a, c"
        }
        DatabaseBackend::Sqlite => {
            "SELECT receipt.cohort_id, receipt.ordinal, receipt.source_identity_hash AS import_source_identity_hash, receipt.source_document_sha256 AS import_source_document_sha256, receipt.payload_digest, a.id, a.tenant_id, t.external_id AS tenant_external_id, a.name, a.driver, a.auth_kind, a.config_json, a.status, a.credential_generation, a.oauth_session_id, a.oauth_driver, a.oauth_refresh_url, a.created_at, a.updated_at, c.expires_at, c.credential_ciphertext, (SELECT COUNT(*) FROM model_routes route WHERE route.tenant_id = a.tenant_id AND (route.upstream_account_id = a.id OR EXISTS (SELECT 1 FROM model_route_upstream_accounts association WHERE association.tenant_id = route.tenant_id AND association.model_route_id = route.id AND association.upstream_account_id = a.id))) AS route_count FROM native_oauth_import_receipts receipt JOIN upstream_accounts a ON a.id = receipt.upstream_account_id AND a.tenant_id = receipt.tenant_id JOIN tenants t ON t.id = a.tenant_id JOIN upstream_credentials c ON c.upstream_account_id = a.id AND c.generation = a.credential_generation AND c.revoked_at IS NULL WHERE receipt.tenant_id = $1 AND receipt.source_identity_hash = $2"
        }
    };
    Ok(sqlx::query(query)
        .bind(tenant_id)
        .bind(source_identity_hash)
        .fetch_optional(&mut **tx)
        .await?)
}

fn validate_existing_kimi_account(
    row: &sqlx::any::AnyRow,
    input: &NativeOAuthImportAccountInput,
    now: i64,
    key_material: &[u8],
) -> Result<(), AppError> {
    let account_id: String = row.try_get("id")?;
    let config: Value = serde_json::from_str(&row.try_get::<String, _>("config_json")?)
        .map_err(|_| AppError::Internal)?;
    if row.try_get::<i64, _>("ordinal")? != input.ordinal
        || row.try_get::<String, _>("driver")? != crate::oauth::managed::kimi::PROVIDER_DRIVER
        || row.try_get::<String, _>("auth_kind")? != "oauth"
        || row
            .try_get::<Option<String>, _>("oauth_session_id")?
            .as_deref()
            != Some(account_id.as_str())
        || row.try_get::<Option<String>, _>("oauth_driver")?.as_deref()
            != Some(crate::oauth::managed::kimi::PROVIDER_DRIVER)
        || row
            .try_get::<Option<String>, _>("oauth_refresh_url")?
            .as_deref()
            != Some(crate::oauth::managed::kimi::TOKEN_ENDPOINT)
        || row.try_get::<String, _>("name")? != input.account_name
        || config != input.config
        || row.try_get::<String, _>("status")? != "active"
        || !row
            .try_get::<Option<i64>, _>("expires_at")?
            .is_some_and(|expires_at| expires_at > now)
        || row.try_get::<i64, _>("route_count")? != 0
    {
        return Err(AppError::Conflict(
            "native Kimi OAuth account does not match the approved current state".into(),
        ));
    }
    let current = open_credential(
        &row.try_get::<String, _>("credential_ciphertext")?,
        key_material,
    )
    .map_err(|_| AppError::Conflict("native Kimi OAuth credential is unreadable".into()))?;
    if current.expires_at() != row.try_get::<Option<i64>, _>("expires_at")? {
        return Err(AppError::Conflict(
            "native Kimi OAuth credential expiry metadata changed".into(),
        ));
    }
    crate::oauth::managed::kimi::validate_credential(&current)
        .map_err(|_| AppError::Conflict("native Kimi OAuth credential shape changed".into()))
}

fn current_cas_matches_account(
    row: &sqlx::any::AnyRow,
    input: &NativeOAuthImportAccountInput,
) -> Result<bool, AppError> {
    let account_id: String = row.try_get("id")?;
    Ok(
        input.expected_current_account_id.as_deref() == Some(account_id.as_str())
            && input.expected_current_document_sha256.as_deref()
                == row
                    .try_get::<Option<String>, _>("import_source_document_sha256")?
                    .as_deref()
            && input.expected_current_credential_generation
                == Some(row.try_get::<i64, _>("credential_generation")?),
    )
}

#[allow(clippy::too_many_arguments)]
async fn rotate_native_kimi_credential(
    tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    tenant_id: &str,
    input: &NativeOAuthImportAccountInput,
    config_json: String,
    credential_ciphertext: String,
    now: i64,
    key_material: &[u8],
    row: &sqlx::any::AnyRow,
) -> Result<(), AppError> {
    let account_id: String = row.try_get("id")?;
    let current_generation: i64 = row.try_get("credential_generation")?;
    let current_updated_at: i64 = row.try_get("updated_at")?;
    let current = open_credential(
        &row.try_get::<String, _>("credential_ciphertext")?,
        key_material,
    )?;
    if kimi_device_identity(&current) != kimi_device_identity(&input.credential) {
        return Err(AppError::Conflict(
            "native Kimi OAuth source changed its device identity".into(),
        ));
    }
    let generation = current_generation
        .checked_add(1)
        .ok_or(AppError::Internal)?;
    let updated_at = now.max(current_updated_at.saturating_add(1));
    let revoked = sqlx::query(
        "UPDATE upstream_credentials SET revoked_at = $1 WHERE upstream_account_id = $2 AND generation = $3 AND revoked_at IS NULL",
    )
    .bind(now)
    .bind(&account_id)
    .bind(current_generation)
    .execute(&mut **tx)
    .await?;
    if revoked.rows_affected() != 1 {
        return Err(AppError::Conflict(
            "native Kimi OAuth credential changed during rotation".into(),
        ));
    }
    sqlx::query(
        "INSERT INTO upstream_credentials (id, upstream_account_id, generation, credential_ciphertext, expires_at, created_at) VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(&account_id)
    .bind(generation)
    .bind(credential_ciphertext)
    .bind(input.credential.expires_at())
    .bind(now)
    .execute(&mut **tx)
    .await?;
    let account = sqlx::query(
        "UPDATE upstream_accounts SET config_json = $1, credential_generation = $2, updated_at = $3 WHERE id = $4 AND tenant_id = $5 AND driver = $6 AND auth_kind = 'oauth' AND status = 'active' AND credential_generation = $7 AND updated_at = $8",
    )
    .bind(config_json)
    .bind(generation)
    .bind(updated_at)
    .bind(&account_id)
    .bind(tenant_id)
    .bind(crate::oauth::managed::kimi::PROVIDER_DRIVER)
    .bind(current_generation)
    .bind(current_updated_at)
    .execute(&mut **tx)
    .await?;
    let receipt = sqlx::query(
        "UPDATE native_oauth_import_receipts SET source_document_sha256 = $1, payload_digest = $2, updated_at = $3 WHERE tenant_id = $4 AND source_identity_hash = $5 AND upstream_account_id = $6 AND source_document_sha256 = $7",
    )
    .bind(&input.source_document_sha256)
    .bind(&input.payload_digest)
    .bind(updated_at)
    .bind(tenant_id)
    .bind(&input.source_identity_hash)
    .bind(&account_id)
    .bind(&input.expected_current_document_sha256)
    .execute(&mut **tx)
    .await?;
    if account.rows_affected() != 1 || receipt.rows_affected() != 1 {
        return Err(AppError::Conflict(
            "native Kimi OAuth cohort changed during rotation".into(),
        ));
    }
    Ok(())
}

fn kimi_device_identity(credential: &UpstreamCredential) -> Option<&str> {
    credential
        .adapter_state()
        .and_then(|state| state.get("device_id"))
        .and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::Digest;

    fn credential(device: &str, token: &str) -> UpstreamCredential {
        UpstreamCredential::OAuth {
            access_token: format!("{token}-access"),
            refresh_token: Some(format!("{token}-refresh")),
            expires_at: Some(4_070_908_800_000),
            header: "authorization".into(),
            prefix: "Bearer ".into(),
            adapter_state: Some(serde_json::json!({
                "schema": "kimi-oauth-v1",
                "device_id": device,
                "scope": "coding",
                "token_type": "Bearer"
            })),
            proxy_url: None,
            proxy_network_scope: None,
        }
    }

    fn input(
        tenant: &str,
        ordinal: i64,
        identity: char,
        document: char,
        token: &str,
    ) -> NativeOAuthImportAccountInput {
        NativeOAuthImportAccountInput {
            tenant_external_id: tenant.into(),
            ordinal,
            source_identity_hash: identity.to_string().repeat(64),
            source_document_sha256: document.to_string().repeat(64),
            payload_digest: format!("{:x}", sha2::Sha256::digest(token.as_bytes())),
            expected_current_account_id: None,
            expected_current_document_sha256: None,
            expected_current_credential_generation: None,
            account_name: format!("Kimi OAuth {ordinal}"),
            config: crate::oauth::managed::kimi::native_import_config(),
            credential: credential(&format!("device-{ordinal}"), token),
        }
    }

    fn approval() -> NativeOAuthImportApproval {
        NativeOAuthImportApproval {
            expected_current_cohort_sha256: "a".repeat(64),
            new_cohort_sha256: "b".repeat(64),
        }
    }

    fn bind_current(inputs: &mut [NativeOAuthImportAccountInput], current: &[UpstreamAccountView]) {
        for (input, account) in inputs.iter_mut().zip(current) {
            input.expected_current_account_id = Some(account.id.to_string());
            input.expected_current_document_sha256 = account.import_source_document_sha256.clone();
            input.expected_current_credential_generation = Some(account.credential_generation);
        }
    }

    async fn contract(db: &Database, tenant: &str) {
        let key = b"native-oauth-import-test-pepper-material";
        let created_inputs = vec![
            input(tenant, 1, '1', 'a', "alpha"),
            input(tenant, 2, '2', 'b', "bravo"),
        ];
        let created = db
            .import_native_kimi_oauth_cohort(created_inputs.clone(), approval(), key)
            .await
            .unwrap();
        assert_eq!((created.created, created.rotated), (2, 0));
        let create_retry = db
            .import_native_kimi_oauth_cohort(created_inputs.clone(), approval(), key)
            .await
            .unwrap();
        assert_eq!((create_retry.created, create_retry.rotated), (0, 0));

        let mut current_inputs = created_inputs;
        bind_current(&mut current_inputs, &created.accounts);
        let replayed = db
            .import_native_kimi_oauth_cohort(current_inputs.clone(), approval(), key)
            .await
            .unwrap();
        assert_eq!((replayed.created, replayed.rotated), (0, 0));

        let stale = current_inputs.clone();
        for (index, input) in current_inputs.iter_mut().enumerate() {
            input.source_document_sha256 =
                (if index == 0 { 'c' } else { 'd' }).to_string().repeat(64);
            input.payload_digest = (if index == 0 { 'e' } else { 'f' }).to_string().repeat(64);
            input.credential = credential(&format!("device-{}", index + 1), "rotated");
        }
        let rotated = db
            .import_native_kimi_oauth_cohort(current_inputs.clone(), approval(), key)
            .await
            .unwrap();
        assert_eq!((rotated.created, rotated.rotated), (0, 2));
        assert!(
            rotated
                .accounts
                .iter()
                .all(|account| account.credential_generation == 2)
        );
        let rotation_retry = db
            .import_native_kimi_oauth_cohort(current_inputs.clone(), approval(), key)
            .await
            .unwrap();
        assert_eq!((rotation_retry.created, rotation_retry.rotated), (0, 0));
        assert!(
            rotation_retry
                .accounts
                .iter()
                .all(|account| account.credential_generation == 2)
        );
        assert!(matches!(
            db.import_native_kimi_oauth_cohort(stale, approval(), key)
                .await,
            Err(AppError::Conflict(_))
        ));

        let disabled = &rotated.accounts[0];
        db.set_upstream_account_status(disabled.id, tenant, "disabled", disabled.updated_at)
            .await
            .unwrap();
        bind_current(&mut current_inputs, &rotated.accounts);
        assert!(matches!(
            db.import_native_kimi_oauth_cohort(current_inputs, approval(), key)
                .await,
            Err(AppError::Conflict(_))
        ));
        let inventory = db.list_upstream_accounts(tenant).await.unwrap();
        assert_eq!(inventory.len(), 2);
        assert_eq!(
            inventory
                .iter()
                .filter(|account| account.status == "disabled")
                .count(),
            1
        );
        assert!(
            inventory
                .iter()
                .all(|account| account.credential_generation == 2)
        );

        let converge_tenant = format!("{tenant}-converge");
        let mut converge_inputs = vec![
            input(&converge_tenant, 1, '3', '3', "charlie"),
            input(&converge_tenant, 2, '4', '4', "delta"),
        ];
        let initial = db
            .import_native_kimi_oauth_cohort(converge_inputs.clone(), approval(), key)
            .await
            .unwrap();
        bind_current(&mut converge_inputs, &initial.accounts);
        let missing_id = initial.accounts[1].id.to_string();
        sqlx::query("DELETE FROM native_oauth_import_receipts WHERE upstream_account_id = $1")
            .bind(&missing_id)
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM upstream_credentials WHERE upstream_account_id = $1")
            .bind(&missing_id)
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM upstream_accounts WHERE id = $1")
            .bind(&missing_id)
            .execute(&db.pool)
            .await
            .unwrap();
        converge_inputs[1].expected_current_account_id = None;
        converge_inputs[1].expected_current_document_sha256 = None;
        converge_inputs[1].expected_current_credential_generation = None;
        let converged = db
            .import_native_kimi_oauth_cohort(converge_inputs, approval(), key)
            .await
            .unwrap();
        assert_eq!((converged.created, converged.rotated), (1, 0));
        assert_eq!(converged.accounts[0].id, initial.accounts[0].id);
        assert_eq!(
            db.list_upstream_accounts(&converge_tenant)
                .await
                .unwrap()
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn sqlite_native_kimi_cohort_contract() {
        let directory = tempfile::tempdir().unwrap();
        let url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("native-kimi-contract.db").display()
        );
        let db = Database::connect(&url).await.unwrap();
        db.migrate().await.unwrap();
        contract(&db, "native-kimi-sqlite").await;
    }

    #[tokio::test]
    async fn postgres_native_kimi_cohort_contract() {
        let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
            return;
        };
        let db = Database::connect(&url).await.unwrap();
        db.migrate().await.unwrap();
        contract(&db, &format!("native-kimi-postgres-{}", Uuid::now_v7())).await;
    }

    async fn concurrent_contract(db: Database, tenant: String) {
        let key = b"native-oauth-import-test-pepper-material";
        let first = vec![
            input(&tenant, 1, '5', '5', "echo"),
            input(&tenant, 2, '6', '6', "foxtrot"),
        ];
        let second = vec![
            input(&tenant, 1, '7', '7', "golf"),
            input(&tenant, 2, '8', '8', "hotel"),
        ];
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(3));
        let spawn = |inputs| {
            let db = db.clone();
            let barrier = barrier.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                db.import_native_kimi_oauth_cohort(inputs, approval(), key)
                    .await
            })
        };
        let first = spawn(first);
        let second = spawn(second);
        barrier.wait().await;
        let outcomes = [first.await.unwrap(), second.await.unwrap()];
        assert_eq!(outcomes.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            outcomes
                .iter()
                .filter(|result| matches!(result, Err(AppError::Conflict(_))))
                .count(),
            1
        );
        assert_eq!(db.list_upstream_accounts(&tenant).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn sqlite_concurrent_cohorts_serialize_without_a_third_account() {
        let directory = tempfile::tempdir().unwrap();
        let url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("native-kimi-concurrent.db").display()
        );
        let db = Database::connect(&url).await.unwrap();
        db.migrate().await.unwrap();
        concurrent_contract(db, "native-kimi-concurrent-sqlite".into()).await;
    }

    #[tokio::test]
    async fn postgres_concurrent_cohorts_share_the_tenant_lock() {
        let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
            return;
        };
        let db = Database::connect(&url).await.unwrap();
        db.migrate().await.unwrap();
        concurrent_contract(db, format!("native-kimi-concurrent-pg-{}", Uuid::now_v7())).await;
    }
}
