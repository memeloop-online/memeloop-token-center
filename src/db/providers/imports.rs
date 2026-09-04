use super::super::*;
use super::accounts::{upstream_account_view, validate_upstream_account_name};
use crate::provider::ResolvedManagedOAuthAdapter;
use serde::Serialize;

const CPA_MANAGED_OAUTH_IMPORT_KIND: &str = "cpa_managed_oauth";
const CPA_MANAGED_OAUTH_IMPORT_CONTRACT_VERSION: i64 = 1;
const CPA_MANAGED_OAUTH_NAME_LOCK_SEED: i64 = 734_627_102_948_335;
const NATIVE_CODEX_UPGRADE_MAX_ACCOUNTS: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedOAuthImportStatus {
    Active,
    Disabled,
    RefreshRequired,
}

impl ManagedOAuthImportStatus {
    fn as_database_status(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Disabled | Self::RefreshRequired => "disabled",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ImportManagedOAuthAccountInput {
    pub tenant_external_id: String,
    /// HMAC-SHA256 of the source identity, encoded as lowercase hex.
    pub source_key: String,
    /// HMAC-SHA256 of the canonical source payload, encoded as lowercase hex.
    pub payload_digest: String,
    pub contract_version: i64,
    pub account_name: String,
    pub config: Value,
    pub credential: UpstreamCredential,
    pub status: ManagedOAuthImportStatus,
    /// Opaque identity resolved from the current server/plugin catalog.
    pub adapter: ResolvedManagedOAuthAdapter,
}

#[derive(Clone, Debug)]
pub struct ManagedOAuthImportResult {
    pub account: UpstreamAccountView,
    pub replayed: bool,
    pub updated: bool,
}

/// A compare-and-swap snapshot emitted by the review phase of the controlled
/// Codex account upgrade. It intentionally contains no account name, tenant,
/// provider configuration, or credential material.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeCodexUpgradeTarget {
    pub account_id: Uuid,
    pub expected_updated_at: i64,
    pub expected_credential_generation: i64,
    pub has_proxy: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_network_scope: Option<crate::network::OutboundScope>,
}

#[derive(Clone, Debug, Serialize)]
pub struct NativeCodexUpgradeReport {
    pub upgraded_account_ids: Vec<Uuid>,
    pub already_native_account_ids: Vec<Uuid>,
}

impl Database {
    /// Read the exact, operator-selected imported Codex accounts that may be
    /// migrated. The companion apply operation requires the returned CAS
    /// values, so this endpoint is safe to use as a human review stage rather
    /// than a best-effort bulk rewrite.
    pub async fn prepare_native_codex_upgrade(
        &self,
        account_ids: &[Uuid],
        key_material: &[u8],
    ) -> Result<Vec<NativeCodexUpgradeTarget>, AppError> {
        validate_native_codex_upgrade_ids(account_ids)?;
        let mut targets = Vec::with_capacity(account_ids.len());
        for account_id in account_ids {
            self.require_managed_oauth_import(*account_id).await?;
            let (account, credential) = self
                .upstream_account_with_credential(*account_id, key_material)
                .await?;
            validate_native_codex_upgrade_candidate(&account, &credential)?;
            targets.push(NativeCodexUpgradeTarget {
                account_id: *account_id,
                expected_updated_at: account.updated_at,
                expected_credential_generation: account.credential_generation,
                has_proxy: credential.proxy().is_some(),
                proxy_network_scope: credential.proxy().map(|(_, scope)| scope),
            });
        }
        Ok(targets)
    }

    /// Atomically switch an exact allowlist of imported Codex accounts to the
    /// native provider ABI. The stable account id, credential generation,
    /// refresh state and encrypted private SOCKS metadata are retained. Only
    /// the encrypted adapter-state schema and lifecycle metadata are changed.
    pub async fn apply_native_codex_upgrade(
        &self,
        targets: &[NativeCodexUpgradeTarget],
        key_material: &[u8],
    ) -> Result<NativeCodexUpgradeReport, AppError> {
        validate_native_codex_upgrade_targets(targets)?;
        let mut tx = self.pool.begin().await?;
        let mut upgraded_account_ids = Vec::new();
        let mut already_native_account_ids = Vec::new();
        for target in targets {
            let row = native_codex_upgrade_row(self, &mut tx, target.account_id).await?;
            let driver: String = row.try_get("driver")?;
            let config: Value = serde_json::from_str(&row.try_get::<String, _>("config_json")?)
                .map_err(|_| AppError::Internal)?;
            let credential = open_credential(
                &row.try_get::<String, _>("credential_ciphertext")?,
                key_material,
            )?;
            if driver == crate::oauth::codex_device::PROVIDER_DRIVER {
                validate_native_codex_account_shape(&row, &config, &credential)?;
                let (credential, repaired) =
                    crate::oauth::managed::codex::restore_remote_dns_proxy(credential)?;
                if !repaired {
                    already_native_account_ids.push(target.account_id);
                    continue;
                }
                let current_updated_at: i64 = row.try_get("updated_at")?;
                let current_generation: i64 = row.try_get("credential_generation")?;
                if current_updated_at != target.expected_updated_at
                    || current_generation != target.expected_credential_generation
                {
                    return Err(AppError::Conflict(
                        "an OpenAI Codex account changed after migration review".into(),
                    ));
                }
                crate::oauth::managed::codex::validate_native_credential(&credential)?;
                let ciphertext = seal_credential(&credential, key_material)?;
                let updated_at = unix_millis().max(current_updated_at.saturating_add(1));
                let changed = sqlx::query(
                    "UPDATE upstream_accounts SET updated_at = $1 WHERE id = $2 AND updated_at = $3 AND credential_generation = $4",
                )
                .bind(updated_at)
                .bind(target.account_id.to_string())
                .bind(target.expected_updated_at)
                .bind(target.expected_credential_generation)
                .execute(&mut *tx)
                .await?;
                if changed.rows_affected() != 1 {
                    return Err(AppError::Conflict(
                        "an OpenAI Codex account changed during proxy repair".into(),
                    ));
                }
                let sealed = sqlx::query(
                    "UPDATE upstream_credentials SET credential_ciphertext = $1 WHERE upstream_account_id = $2 AND generation = $3 AND revoked_at IS NULL",
                )
                .bind(ciphertext)
                .bind(target.account_id.to_string())
                .bind(target.expected_credential_generation)
                .execute(&mut *tx)
                .await?;
                if sealed.rows_affected() != 1 {
                    return Err(AppError::Conflict(
                        "an OpenAI Codex credential changed during proxy repair".into(),
                    ));
                }
                upgraded_account_ids.push(target.account_id);
                continue;
            }
            validate_imported_codex_account_shape(&row, &config, &credential)?;
            let current_updated_at: i64 = row.try_get("updated_at")?;
            let current_generation: i64 = row.try_get("credential_generation")?;
            if current_updated_at != target.expected_updated_at
                || current_generation != target.expected_credential_generation
            {
                return Err(AppError::Conflict(
                    "an imported OpenAI Codex account changed after migration review".into(),
                ));
            }
            let native_config = crate::oauth::managed::codex::native_config_from_import(&config)?;
            let native_credential =
                crate::oauth::managed::codex::upgrade_imported_credential(credential)?;
            let ciphertext = seal_credential(&native_credential, key_material)?;
            let updated_at = unix_millis().max(current_updated_at.saturating_add(1));
            let changed = sqlx::query(
                "UPDATE upstream_accounts SET driver = $1, auth_kind = 'oauth', config_json = $2, oauth_driver = $3, oauth_refresh_url = $4, updated_at = $5 WHERE id = $6 AND updated_at = $7 AND credential_generation = $8",
            )
            .bind(crate::oauth::codex_device::PROVIDER_DRIVER)
            .bind(serde_json::to_string(&native_config).map_err(|_| AppError::Internal)?)
            .bind(crate::oauth::codex_device::OAUTH_DRIVER)
            .bind(crate::oauth::codex_device::TOKEN_ENDPOINT)
            .bind(updated_at)
            .bind(target.account_id.to_string())
            .bind(target.expected_updated_at)
            .bind(target.expected_credential_generation)
            .execute(&mut *tx)
            .await?;
            if changed.rows_affected() != 1 {
                return Err(AppError::Conflict(
                    "an imported OpenAI Codex account changed during migration".into(),
                ));
            }
            let sealed = sqlx::query(
                "UPDATE upstream_credentials SET credential_ciphertext = $1 WHERE upstream_account_id = $2 AND generation = $3 AND revoked_at IS NULL",
            )
            .bind(ciphertext)
            .bind(target.account_id.to_string())
            .bind(target.expected_credential_generation)
            .execute(&mut *tx)
            .await?;
            if sealed.rows_affected() != 1 {
                return Err(AppError::Conflict(
                    "an imported OpenAI Codex credential changed during migration".into(),
                ));
            }
            upgraded_account_ids.push(target.account_id);
        }
        tx.commit().await?;
        Ok(NativeCodexUpgradeReport {
            upgraded_account_ids,
            already_native_account_ids,
        })
    }

    async fn require_managed_oauth_import(&self, account_id: Uuid) -> Result<(), AppError> {
        let imported = sqlx::query(
            "SELECT 1 FROM upstream_account_imports WHERE upstream_account_id = $1 AND import_kind = $2",
        )
        .bind(account_id.to_string())
        .bind(CPA_MANAGED_OAUTH_IMPORT_KIND)
        .fetch_optional(&self.pool)
        .await?
        .is_some();
        if imported {
            Ok(())
        } else {
            Err(AppError::NotFound)
        }
    }
    /// Check immutable import provenance before a caller sends the secret
    /// source document to an adapter. Exact replays therefore need neither the
    /// document nor a currently reachable adapter.
    pub async fn lookup_cpa_managed_oauth_import(
        &self,
        tenant_external_id: &str,
        source_key: &str,
        payload_digest: &str,
    ) -> Result<Option<UpstreamAccountView>, AppError> {
        validate_lowercase_hex_digest(source_key, "managed OAuth source key")?;
        validate_lowercase_hex_digest(payload_digest, "managed OAuth payload digest")?;
        let row = sqlx::query(
            "SELECT i.payload_digest, a.id, a.tenant_id, t.external_id AS tenant_external_id, a.name, a.driver, a.auth_kind, a.config_json, a.status, a.credential_generation, a.oauth_session_id, a.oauth_driver, a.oauth_refresh_url, a.created_at, a.updated_at, c.expires_at, (SELECT COUNT(*) FROM model_routes r WHERE r.upstream_account_id = a.id) AS route_count FROM upstream_account_imports i JOIN tenants t ON t.id = i.tenant_id JOIN upstream_accounts a ON a.id = i.upstream_account_id JOIN upstream_credentials c ON c.upstream_account_id = a.id AND c.generation = a.credential_generation AND c.revoked_at IS NULL WHERE t.external_id = $1 AND i.import_kind = $2 AND i.source_key = $3",
        )
        .bind(tenant_external_id)
        .bind(CPA_MANAGED_OAUTH_IMPORT_KIND)
        .bind(source_key)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        if row.try_get::<String, _>("payload_digest")? != payload_digest {
            let driver: String = row.try_get("driver")?;
            if matches!(
                driver.as_str(),
                crate::oauth::codex_device::PROVIDER_DRIVER
                    | crate::oauth::codex_device::IMPORTED_PROVIDER_DRIVER
            ) {
                // Changed Codex source material is resolved by the controlled
                // transaction below. Do not return a stable account here: that
                // would make a rotated refresh token look like an exact replay.
                return Ok(None);
            }
            return Err(AppError::Conflict(
                "managed OAuth import source already exists with different immutable metadata"
                    .into(),
            ));
        }
        upstream_account_view(row).map(Some)
    }

    pub async fn import_cpa_managed_oauth_account(
        &self,
        input: ImportManagedOAuthAccountInput,
        key_material: &[u8],
    ) -> Result<ManagedOAuthImportResult, AppError> {
        validate_managed_oauth_import(&input)?;
        let now = unix_millis();
        validate_initial_status(
            input.status,
            &input.credential,
            input.adapter.can_refresh(),
            now,
        )?;
        let name = input.account_name.trim().to_owned();
        let config_json = serde_json::to_string(&input.config).map_err(|_| AppError::Internal)?;
        let credential_ciphertext = seal_credential(&input.credential, key_material)?;
        let credential_expires_at = input.credential.expires_at();
        let can_refresh = input.adapter.can_refresh();
        let oauth_refresh_url = can_refresh.then(|| input.adapter.refresh_url().to_owned());
        let oauth_driver =
            if input.adapter.provider_driver() == crate::oauth::codex_device::PROVIDER_DRIVER {
                crate::oauth::codex_device::OAUTH_DRIVER
            } else {
                input.adapter.provider_driver()
            };
        let account_id = Uuid::now_v7();
        let mut tx = self.pool.begin().await?;

        sqlx::query(
            "INSERT INTO tenants (id, external_id, created_at) VALUES ($1, $2, $3) ON CONFLICT(external_id) DO NOTHING",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(&input.tenant_external_id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        let tenant_id: String = sqlx::query("SELECT id FROM tenants WHERE external_id = $1")
            .bind(&input.tenant_external_id)
            .fetch_one(&mut *tx)
            .await?
            .try_get("id")?;

        if matches!(self.backend, DatabaseBackend::PostgreSql) {
            sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, $2))")
                .bind(
                    serde_json::to_string(&(tenant_id.as_str(), name.as_str()))
                        .map_err(|_| AppError::Internal)?,
                )
                .bind(CPA_MANAGED_OAUTH_NAME_LOCK_SEED)
                .execute(&mut *tx)
                .await?;
        }

        let claimed = sqlx::query(
            "INSERT INTO upstream_account_imports (tenant_id, import_kind, source_key, contract_version, payload_digest, upstream_account_id, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT(tenant_id, import_kind, source_key) DO NOTHING",
        )
        .bind(&tenant_id)
        .bind(CPA_MANAGED_OAUTH_IMPORT_KIND)
        .bind(&input.source_key)
        .bind(input.contract_version)
        .bind(&input.payload_digest)
        .bind(account_id.to_string())
        .bind(now)
        .execute(&mut *tx)
        .await?;

        if claimed.rows_affected() == 0 {
            let row =
                managed_oauth_import_row(self.backend, &mut tx, &tenant_id, &input.source_key)
                    .await?;
            let existing_digest: String = row.try_get("payload_digest")?;
            let existing_contract: i64 = row.try_get("contract_version")?;
            if existing_contract != input.contract_version {
                return Err(AppError::Conflict(
                    "managed OAuth import source uses an unsupported contract version".into(),
                ));
            }
            if existing_digest == input.payload_digest {
                let view = upstream_account_view(row)?;
                tx.commit().await?;
                return Ok(ManagedOAuthImportResult {
                    account: view,
                    replayed: true,
                    updated: false,
                });
            }
            if input.adapter.provider_driver() != crate::oauth::codex_device::PROVIDER_DRIVER {
                return Err(AppError::Conflict(
                    "managed OAuth source material changed and requires a dedicated reauthorization"
                        .into(),
                ));
            }
            let current_driver: String = row.try_get("driver")?;
            let current_credential = open_credential(
                &row.try_get::<String, _>("credential_ciphertext")?,
                key_material,
            )?;
            let current_native = if current_driver == crate::oauth::codex_device::PROVIDER_DRIVER {
                crate::oauth::managed::codex::validate_native_credential(&current_credential)?;
                current_credential
            } else if current_driver == crate::oauth::codex_device::IMPORTED_PROVIDER_DRIVER {
                crate::oauth::managed::codex::upgrade_imported_credential(current_credential)?
            } else {
                return Err(AppError::Conflict(
                    "managed OAuth source no longer identifies an OpenAI Codex account".into(),
                ));
            };
            let replacement = input
                .credential
                .clone()
                .preserve_proxy_from(&current_native);
            crate::oauth::managed::codex::validate_native_credential(&replacement)?;
            if crate::oauth::managed::codex::account_header_value(&current_native)?
                != crate::oauth::managed::codex::account_header_value(&replacement)?
            {
                return Err(AppError::Conflict(
                    "managed OAuth source changed its external account identity".into(),
                ));
            }
            let current_generation: i64 = row.try_get("credential_generation")?;
            let generation = current_generation
                .checked_add(1)
                .ok_or(AppError::Internal)?;
            let current_updated_at: i64 = row.try_get("updated_at")?;
            let updated_at = now.max(current_updated_at.saturating_add(1));
            let replacement_ciphertext = seal_credential(&replacement, key_material)?;
            let existing_id: String = row.try_get("id")?;
            sqlx::query(
                "UPDATE upstream_credentials SET revoked_at = $1 WHERE upstream_account_id = $2 AND generation = $3 AND revoked_at IS NULL",
            )
            .bind(now)
            .bind(&existing_id)
            .bind(current_generation)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "INSERT INTO upstream_credentials (id, upstream_account_id, generation, credential_ciphertext, expires_at, created_at) VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(Uuid::now_v7().to_string())
            .bind(&existing_id)
            .bind(generation)
            .bind(replacement_ciphertext)
            .bind(replacement.expires_at())
            .bind(now)
            .execute(&mut *tx)
            .await?;
            let changed = sqlx::query(
                "UPDATE upstream_accounts SET driver = $1, auth_kind = 'oauth', config_json = $2, status = $3, credential_generation = $4, oauth_driver = $5, oauth_refresh_url = $6, updated_at = $7 WHERE id = $8 AND credential_generation = $9 AND updated_at = $10",
            )
            .bind(crate::oauth::codex_device::PROVIDER_DRIVER)
            .bind(&config_json)
            .bind(input.status.as_database_status())
            .bind(generation)
            .bind(crate::oauth::codex_device::OAUTH_DRIVER)
            .bind(oauth_refresh_url)
            .bind(updated_at)
            .bind(&existing_id)
            .bind(current_generation)
            .bind(current_updated_at)
            .execute(&mut *tx)
            .await?;
            if changed.rows_affected() != 1 {
                return Err(AppError::Conflict(
                    "managed OAuth source changed during credential refresh".into(),
                ));
            }
            sqlx::query(
                "UPDATE upstream_account_imports SET payload_digest = $1 WHERE tenant_id = $2 AND import_kind = $3 AND source_key = $4",
            )
            .bind(&input.payload_digest)
            .bind(&tenant_id)
            .bind(CPA_MANAGED_OAUTH_IMPORT_KIND)
            .bind(&input.source_key)
            .execute(&mut *tx)
            .await?;
            let existing_id = parse_uuid(existing_id)?;
            tx.commit().await?;
            let (account, _) = self
                .upstream_account_with_credential(existing_id, key_material)
                .await?;
            return Ok(ManagedOAuthImportResult {
                account,
                replayed: false,
                updated: true,
            });
        }

        if sqlx::query("SELECT id FROM upstream_accounts WHERE tenant_id = $1 AND name = $2")
            .bind(&tenant_id)
            .bind(&name)
            .fetch_optional(&mut *tx)
            .await?
            .is_some()
        {
            return Err(AppError::Conflict(
                "another upstream provider already uses this name".into(),
            ));
        }

        sqlx::query(
            "INSERT INTO upstream_accounts (id, tenant_id, name, driver, auth_kind, config_json, status, credential_generation, oauth_session_id, oauth_driver, oauth_refresh_url, created_at, updated_at) VALUES ($1, $2, $3, $4, 'oauth', $5, $6, 1, $1, $7, $8, $9, $9)",
        )
        .bind(account_id.to_string())
        .bind(&tenant_id)
        .bind(&name)
        .bind(input.adapter.provider_driver())
        .bind(config_json)
        .bind(input.status.as_database_status())
        .bind(oauth_driver)
        .bind(oauth_refresh_url)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO upstream_credentials (id, upstream_account_id, generation, credential_ciphertext, expires_at, created_at) VALUES ($1, $2, 1, $3, $4, $5)",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(account_id.to_string())
        .bind(credential_ciphertext)
        .bind(credential_expires_at)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        Ok(ManagedOAuthImportResult {
            account: UpstreamAccountView {
                id: account_id,
                tenant_id: parse_uuid(tenant_id)?,
                tenant_external_id: Some(input.tenant_external_id),
                name,
                driver: input.adapter.provider_driver().to_owned(),
                auth_kind: "oauth".to_owned(),
                connection_method: "oauth".to_owned(),
                credential_generation: 1,
                status: input.status.as_database_status().to_owned(),
                config: input.config,
                credential_expires_at,
                can_refresh,
                can_rotate: true,
                can_reauthorize: false,
                route_count: 0,
                created_at: now,
                updated_at: now,
            },
            replayed: false,
            updated: false,
        })
    }

    pub async fn managed_oauth_lifecycle(
        &self,
        account_id: Uuid,
    ) -> Result<(String, String), AppError> {
        let row = sqlx::query(
            "SELECT a.oauth_driver, a.oauth_refresh_url FROM upstream_accounts a JOIN upstream_account_imports i ON i.upstream_account_id = a.id WHERE a.id = $1 AND i.import_kind = $2",
        )
        .bind(account_id.to_string())
        .bind(CPA_MANAGED_OAUTH_IMPORT_KIND)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::NotFound)?;
        let driver = row
            .try_get::<Option<String>, _>("oauth_driver")?
            .ok_or(AppError::Internal)?;
        let refresh_url = row
            .try_get::<Option<String>, _>("oauth_refresh_url")?
            .ok_or(AppError::Internal)?;
        Ok((driver, refresh_url))
    }
}

async fn native_codex_upgrade_row(
    database: &Database,
    tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    account_id: Uuid,
) -> Result<sqlx::any::AnyRow, AppError> {
    let select = match database.backend {
        DatabaseBackend::PostgreSql => {
            "SELECT a.driver, a.auth_kind, a.config_json, a.credential_generation, a.oauth_session_id, a.oauth_driver, a.oauth_refresh_url, a.updated_at, c.credential_ciphertext FROM upstream_accounts a JOIN upstream_account_imports i ON i.upstream_account_id = a.id AND i.import_kind = $2 JOIN upstream_credentials c ON c.upstream_account_id = a.id AND c.generation = a.credential_generation AND c.revoked_at IS NULL WHERE a.id = $1 FOR UPDATE OF a, c"
        }
        DatabaseBackend::Sqlite => {
            "SELECT a.driver, a.auth_kind, a.config_json, a.credential_generation, a.oauth_session_id, a.oauth_driver, a.oauth_refresh_url, a.updated_at, c.credential_ciphertext FROM upstream_accounts a JOIN upstream_account_imports i ON i.upstream_account_id = a.id AND i.import_kind = $2 JOIN upstream_credentials c ON c.upstream_account_id = a.id AND c.generation = a.credential_generation AND c.revoked_at IS NULL WHERE a.id = $1"
        }
    };
    sqlx::query(select)
        .bind(account_id.to_string())
        .bind(CPA_MANAGED_OAUTH_IMPORT_KIND)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(AppError::NotFound)
}

fn validate_native_codex_upgrade_ids(account_ids: &[Uuid]) -> Result<(), AppError> {
    if account_ids.is_empty() || account_ids.len() > NATIVE_CODEX_UPGRADE_MAX_ACCOUNTS {
        return Err(AppError::BadRequest(
            "native OpenAI Codex migration requires 1 to 64 account ids".into(),
        ));
    }
    let mut unique = std::collections::BTreeSet::new();
    if account_ids
        .iter()
        .any(|account_id| !unique.insert(*account_id))
    {
        return Err(AppError::BadRequest(
            "native OpenAI Codex migration account ids must be unique".into(),
        ));
    }
    Ok(())
}

fn validate_native_codex_upgrade_targets(
    targets: &[NativeCodexUpgradeTarget],
) -> Result<(), AppError> {
    validate_native_codex_upgrade_ids(
        &targets
            .iter()
            .map(|target| target.account_id)
            .collect::<Vec<_>>(),
    )?;
    if targets.iter().any(|target| {
        target.expected_credential_generation < 1
            || !target.has_proxy
            || target.proxy_network_scope != Some(crate::network::OutboundScope::Private)
    }) {
        return Err(AppError::BadRequest(
            "native OpenAI Codex migration requires an approved private proxy".into(),
        ));
    }
    Ok(())
}

fn validate_native_codex_upgrade_candidate(
    account: &UpstreamAccountView,
    credential: &UpstreamCredential,
) -> Result<(), AppError> {
    if account.driver == crate::oauth::codex_device::PROVIDER_DRIVER {
        crate::oauth::managed::codex::native_config_from_import(&account.config)?;
        crate::oauth::managed::codex::validate_native_credential(credential)?;
    } else if account.driver == crate::oauth::codex_device::IMPORTED_PROVIDER_DRIVER {
        crate::oauth::managed::codex::native_config_from_import(&account.config)?;
        let _ = crate::oauth::managed::codex::upgrade_imported_credential(credential.clone())?;
    } else {
        return Err(AppError::BadRequest(
            "selected account is not an imported OpenAI Codex account".into(),
        ));
    }
    require_private_codex_proxy(credential)
}

fn validate_imported_codex_account_shape(
    row: &sqlx::any::AnyRow,
    config: &Value,
    credential: &UpstreamCredential,
) -> Result<(), AppError> {
    if row.try_get::<String, _>("driver")? != crate::oauth::codex_device::IMPORTED_PROVIDER_DRIVER
        || row.try_get::<String, _>("auth_kind")? != "oauth"
        || row
            .try_get::<Option<String>, _>("oauth_session_id")?
            .is_none()
        || row.try_get::<Option<String>, _>("oauth_driver")?.as_deref()
            != Some(crate::oauth::codex_device::IMPORTED_PROVIDER_DRIVER)
        || row
            .try_get::<Option<String>, _>("oauth_refresh_url")?
            .as_deref()
            != Some(crate::oauth::codex_device::TOKEN_ENDPOINT)
    {
        return Err(AppError::BadRequest(
            "imported OpenAI Codex account has an unsupported lifecycle".into(),
        ));
    }
    crate::oauth::managed::codex::native_config_from_import(config)?;
    let _ = crate::oauth::managed::codex::upgrade_imported_credential(credential.clone())?;
    require_private_codex_proxy(credential)
}

fn validate_native_codex_account_shape(
    row: &sqlx::any::AnyRow,
    config: &Value,
    credential: &UpstreamCredential,
) -> Result<(), AppError> {
    if row.try_get::<String, _>("auth_kind")? != "oauth"
        || row
            .try_get::<Option<String>, _>("oauth_session_id")?
            .is_none()
        || row.try_get::<Option<String>, _>("oauth_driver")?.as_deref()
            != Some(crate::oauth::codex_device::OAUTH_DRIVER)
        || row
            .try_get::<Option<String>, _>("oauth_refresh_url")?
            .as_deref()
            != Some(crate::oauth::codex_device::TOKEN_ENDPOINT)
    {
        return Err(AppError::BadRequest(
            "native OpenAI Codex account has an unsupported lifecycle".into(),
        ));
    }
    crate::oauth::managed::codex::native_config_from_import(config)?;
    crate::oauth::managed::codex::validate_native_credential(credential)?;
    require_private_codex_proxy(credential)
}

fn require_private_codex_proxy(credential: &UpstreamCredential) -> Result<(), AppError> {
    match credential.proxy() {
        Some((_, crate::network::OutboundScope::Private)) => Ok(()),
        _ => Err(AppError::BadRequest(
            "native OpenAI Codex migration requires an approved private proxy".into(),
        )),
    }
}

async fn managed_oauth_import_row(
    backend: DatabaseBackend,
    tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    tenant_id: &str,
    source_key: &str,
) -> Result<sqlx::any::AnyRow, AppError> {
    let select = match backend {
        DatabaseBackend::PostgreSql => {
            "SELECT i.payload_digest, i.contract_version, a.id, a.tenant_id, t.external_id AS tenant_external_id, a.name, a.driver, a.auth_kind, a.config_json, a.status, a.credential_generation, a.oauth_session_id, a.oauth_driver, a.oauth_refresh_url, a.created_at, a.updated_at, c.expires_at, c.credential_ciphertext, (SELECT COUNT(*) FROM model_routes r WHERE r.upstream_account_id = a.id) AS route_count FROM upstream_account_imports i JOIN upstream_accounts a ON a.id = i.upstream_account_id JOIN tenants t ON t.id = a.tenant_id JOIN upstream_credentials c ON c.upstream_account_id = a.id AND c.generation = a.credential_generation AND c.revoked_at IS NULL WHERE i.tenant_id = $1 AND i.import_kind = $2 AND i.source_key = $3 FOR UPDATE OF i, a, c"
        }
        DatabaseBackend::Sqlite => {
            "SELECT i.payload_digest, i.contract_version, a.id, a.tenant_id, t.external_id AS tenant_external_id, a.name, a.driver, a.auth_kind, a.config_json, a.status, a.credential_generation, a.oauth_session_id, a.oauth_driver, a.oauth_refresh_url, a.created_at, a.updated_at, c.expires_at, c.credential_ciphertext, (SELECT COUNT(*) FROM model_routes r WHERE r.upstream_account_id = a.id) AS route_count FROM upstream_account_imports i JOIN upstream_accounts a ON a.id = i.upstream_account_id JOIN tenants t ON t.id = a.tenant_id JOIN upstream_credentials c ON c.upstream_account_id = a.id AND c.generation = a.credential_generation AND c.revoked_at IS NULL WHERE i.tenant_id = $1 AND i.import_kind = $2 AND i.source_key = $3"
        }
    };
    sqlx::query(select)
        .bind(tenant_id)
        .bind(CPA_MANAGED_OAUTH_IMPORT_KIND)
        .bind(source_key)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(AppError::Internal)
}

fn validate_managed_oauth_import(input: &ImportManagedOAuthAccountInput) -> Result<(), AppError> {
    validate_lowercase_hex_digest(&input.source_key, "managed OAuth source key")?;
    validate_lowercase_hex_digest(&input.payload_digest, "managed OAuth payload digest")?;
    if input.contract_version != CPA_MANAGED_OAUTH_IMPORT_CONTRACT_VERSION {
        return Err(AppError::BadRequest(
            "unsupported managed OAuth import contract version".into(),
        ));
    }
    validate_upstream_account_name(&input.account_name)?;
    let _ = validate_config(&input.config)?;
    if input.adapter.api_version() != crate::provider::MANAGED_OAUTH_ADAPTER_API_VERSION {
        return Err(AppError::BadRequest(
            "unsupported managed OAuth adapter contract".into(),
        ));
    }
    if !matches!(input.credential, UpstreamCredential::OAuth { .. }) {
        return Err(AppError::BadRequest(
            "managed OAuth import requires an OAuth credential".into(),
        ));
    }
    input.credential.validate(i64::MIN)?;
    Ok(())
}

fn validate_initial_status(
    status: ManagedOAuthImportStatus,
    credential: &UpstreamCredential,
    can_refresh: bool,
    now: i64,
) -> Result<(), AppError> {
    let expires_at = credential.expires_at();
    match status {
        ManagedOAuthImportStatus::Active if expires_at.is_some_and(|value| value <= now) => Err(
            AppError::BadRequest("active managed OAuth import credential is expired".into()),
        ),
        ManagedOAuthImportStatus::RefreshRequired
            if !can_refresh
                || expires_at.is_none_or(|value| value > now)
                || !credential.has_oauth_refresh_state() =>
        {
            Err(AppError::BadRequest(
                "refresh-required managed OAuth import needs expired refreshable state".into(),
            ))
        }
        ManagedOAuthImportStatus::Disabled
            if can_refresh
                && expires_at.is_some_and(|value| value <= now)
                && !credential.has_oauth_refresh_state() =>
        {
            Err(AppError::BadRequest(
                "expired disabled managed OAuth import needs refreshable state".into(),
            ))
        }
        _ => Ok(()),
    }
}

fn validate_lowercase_hex_digest(value: &str, field: &str) -> Result<(), AppError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(AppError::BadRequest(format!(
            "{field} must be a lowercase 64-character hex digest"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod native_codex_upgrade_tests {
    use super::*;

    #[tokio::test]
    async fn native_upgrade_preserves_stable_id_generation_and_private_proxy() {
        let directory = tempfile::tempdir().unwrap();
        let database = Database::connect(&format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("native-codex-upgrade.db").display()
        ))
        .await
        .unwrap();
        database.migrate().await.unwrap();
        let key_material = b"native Codex upgrade key material longer than thirty-two bytes";
        let account = database
            .create_upstream_account(
                CreateUpstreamAccountInput {
                    tenant_external_id: "native-upgrade".to_owned(),
                    name: "Imported Codex".to_owned(),
                    driver: crate::oauth::codex_device::IMPORTED_PROVIDER_DRIVER.to_owned(),
                    config: serde_json::json!({
                        "base_url": crate::oauth::codex_device::BASE_URL,
                        "network_scope": "public",
                        "reservation_token_bounds": {"gpt-5.6-sol": 128000}
                    }),
                    credential: UpstreamCredential::OAuth {
                        access_token: "access-secret".to_owned(),
                        refresh_token: Some("refresh-secret".to_owned()),
                        expires_at: Some(unix_millis() + 3_600_000),
                        header: "authorization".to_owned(),
                        prefix: "Bearer ".to_owned(),
                        adapter_state: Some(serde_json::json!({
                            "schema": "cpa-codex-oauth-v1",
                            "account_id": "account-123"
                        })),
                        proxy_url: Some(
                            "socks5://operator:proxy-secret@100.64.0.16:1080".to_owned(),
                        ),
                        proxy_network_scope: Some(crate::network::OutboundScope::Private),
                    },
                    oauth_session_id: Some(Uuid::now_v7()),
                    oauth_driver: Some(
                        crate::oauth::codex_device::IMPORTED_PROVIDER_DRIVER.to_owned(),
                    ),
                    oauth_refresh_url: Some(crate::oauth::codex_device::TOKEN_ENDPOINT.to_owned()),
                },
                key_material,
            )
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO upstream_account_imports (tenant_id, import_kind, source_key, contract_version, payload_digest, upstream_account_id, created_at) VALUES ($1, $2, $3, 1, $4, $5, $6)",
        )
        .bind(account.tenant_id.to_string())
        .bind(CPA_MANAGED_OAUTH_IMPORT_KIND)
        .bind("a".repeat(64))
        .bind("b".repeat(64))
        .bind(account.id.to_string())
        .bind(unix_millis())
        .execute(&database.pool)
        .await
        .unwrap();

        let plan = database
            .prepare_native_codex_upgrade(&[account.id], key_material)
            .await
            .unwrap();
        assert_eq!(plan.len(), 1);
        assert!(plan[0].has_proxy);
        assert_eq!(
            plan[0].proxy_network_scope,
            Some(crate::network::OutboundScope::Private)
        );
        let result = database
            .apply_native_codex_upgrade(&plan, key_material)
            .await
            .unwrap();
        assert_eq!(result.upgraded_account_ids, vec![account.id]);
        let (upgraded, credential) = database
            .upstream_account_with_credential(account.id, key_material)
            .await
            .unwrap();
        assert_eq!(upgraded.id, account.id);
        assert_eq!(upgraded.driver, crate::oauth::codex_device::PROVIDER_DRIVER);
        assert_eq!(
            upgraded.credential_generation,
            account.credential_generation
        );
        assert_eq!(
            credential.adapter_state(),
            Some(&serde_json::json!({
                "schema": "openai-codex-oauth-v1",
                "account_id": "account-123"
            }))
        );
        assert_eq!(
            credential.proxy(),
            Some((
                "socks5h://operator:proxy-secret@100.64.0.16:1080",
                crate::network::OutboundScope::Private
            ))
        );
        let repeated = database
            .apply_native_codex_upgrade(&plan, key_material)
            .await
            .unwrap();
        assert_eq!(repeated.upgraded_account_ids, Vec::<Uuid>::new());
        assert_eq!(repeated.already_native_account_ids, vec![account.id]);
    }
}
