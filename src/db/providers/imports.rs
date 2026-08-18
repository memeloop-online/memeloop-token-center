use super::super::*;
use super::accounts::{upstream_account_view, validate_upstream_account_name};
use crate::provider::ResolvedManagedOAuthAdapter;

const CPA_MANAGED_OAUTH_IMPORT_KIND: &str = "cpa_managed_oauth";
const CPA_MANAGED_OAUTH_IMPORT_CONTRACT_VERSION: i64 = 1;
const CPA_MANAGED_OAUTH_NAME_LOCK_SEED: i64 = 734_627_102_948_335;

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
}

impl Database {
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
            let row = managed_oauth_import_row(&mut tx, &tenant_id, &input.source_key).await?;
            let existing_digest: String = row.try_get("payload_digest")?;
            let existing_contract: i64 = row.try_get("contract_version")?;
            if existing_digest != input.payload_digest
                || existing_contract != input.contract_version
            {
                return Err(AppError::Conflict(
                    "managed OAuth import source already exists with different immutable metadata"
                        .into(),
                ));
            }
            let view = upstream_account_view(row)?;
            tx.commit().await?;
            return Ok(ManagedOAuthImportResult {
                account: view,
                replayed: true,
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
            "INSERT INTO upstream_accounts (id, tenant_id, name, driver, auth_kind, config_json, status, credential_generation, oauth_session_id, oauth_driver, oauth_refresh_url, created_at, updated_at) VALUES ($1, $2, $3, $4, 'oauth', $5, $6, 1, $1, $4, $7, $8, $8)",
        )
        .bind(account_id.to_string())
        .bind(&tenant_id)
        .bind(&name)
        .bind(input.adapter.provider_driver())
        .bind(config_json)
        .bind(input.status.as_database_status())
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

async fn managed_oauth_import_row(
    tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    tenant_id: &str,
    source_key: &str,
) -> Result<sqlx::any::AnyRow, AppError> {
    sqlx::query(
        "SELECT i.payload_digest, i.contract_version, a.id, a.tenant_id, t.external_id AS tenant_external_id, a.name, a.driver, a.auth_kind, a.config_json, a.status, a.credential_generation, a.oauth_session_id, a.oauth_driver, a.oauth_refresh_url, a.created_at, a.updated_at, c.expires_at, (SELECT COUNT(*) FROM model_routes r WHERE r.upstream_account_id = a.id) AS route_count FROM upstream_account_imports i JOIN upstream_accounts a ON a.id = i.upstream_account_id JOIN tenants t ON t.id = a.tenant_id JOIN upstream_credentials c ON c.upstream_account_id = a.id AND c.generation = a.credential_generation AND c.revoked_at IS NULL WHERE i.tenant_id = $1 AND i.import_kind = $2 AND i.source_key = $3",
    )
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
