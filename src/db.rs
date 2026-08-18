use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{Days, Utc};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{
    Any, AnyConnection, AnyPool, Row, Transaction,
    any::{AnyPoolOptions, AnyQueryResult, AnyRow},
};
use uuid::Uuid;

use crate::{
    conversation::{ConversationHints, RelationKind, build_prefix, extract_atoms},
    crypto,
    error::{AppError, LimitReason},
    model::{
        ArchivedGenerationAsset, AuthenticatedKey, AuthenticatedService, ConversationClusterDetail,
        ConversationClusterView, ConversationCursor, ConversationEdgeView, ConversationRequestView,
        EntitlementReconcileResult, EntitlementView, GenerationAssetDownload, GenerationAssetView,
        GenerationJobView, GenerationJobWork, GenerationPrice, GenerationStagedAssets, IssuedKey,
        IssuedServiceToken, JSON_SAFE_INTEGER_MAX, KeyAliasView, KeyBudgetSnapshot,
        KeyConcurrencySnapshot, KeyLimitSnapshot, KeyPolicy, KeyRateLimitSnapshot, KeyView,
        LedgerEntryView, LegacyCredentialView, ManagedKeyView, ModelPrice, ModelPriceTier,
        ModelPriceTierView, ModelPriceView, OperatorStats, RequestArchiveRefs, RequestEventView,
        RequestProvenanceView, RequestView, SelfStats, ServiceTokenView, StatsBucket, StatsSummary,
        TenantView, TokenUsage, UsageReservation, micros_to_decimal_string, priced_tokens,
    },
    provider::{
        ModelRouteView, ResolvedUpstream, UpstreamAccountView, UpstreamCredential, open_credential,
        open_private_json, seal_credential, seal_private_json, validate_config,
    },
};

mod archive_staging;
mod billing;
mod credentials;
mod generation;
mod migrations;
mod providers;
mod requests;
mod usage_analysis;

pub use billing::{
    CancelEntitlementInput, EntitlementOperation, ReconcileEntitlementInput,
    ReplaceEntitlementInput,
};
pub use credentials::{CreateKeyInput, CreateServiceTokenInput};
pub use generation::{
    AttachGenerationJobResult, AttachSynchronousImageRequestObject, CreateGenerationJobInput,
    CreateGenerationJobResult, FinishGenerationJobInput, FinishSynchronousImageRequest,
    FinishSynchronousImageResult, GenerationJobIdempotency, StartGenerationJobInput,
    StartSynchronousImageRequest, StartSynchronousImageResult, SynchronousImageIdempotencyClaim,
};
pub use migrations::{BlockedPartition, PartitionMaintenanceReport};
pub(crate) use migrations::{POSTGRES_MIGRATIONS, SQLITE_MIGRATIONS};
#[cfg(test)]
use migrations::{apply_migration_range, maintain_postgres_partitions};
pub use providers::{
    CreateModelRouteInput, CreateUpstreamAccountInput, ImportManagedOAuthAccountInput,
    ManagedOAuthImportResult, ManagedOAuthImportStatus, UpdateModelRouteInput,
    UpdateUpstreamAccountInput,
};
#[cfg(test)]
pub(crate) use requests::claim_request_record_locator;
pub use requests::{
    AttachProxyArchiveResult, ConversationDetailFilter, ConversationListFilter, FinishProxyRequest,
    FinishProxyRequestResult, FinishRequest, NewRequest, ProxyConversationInput, RequestListFilter,
    SessionArchiveCommitInput, SessionArchiveCorrelation, SessionArchiveImportLock,
    SessionArchiveMatchInput, SessionArchiveTarget, SessionArchiveUnlinkedCommitInput,
    SessionArchiveUnlinkedMetadata, SessionArchiveUnlinkedTarget, StartProxyRequest, StatsFilter,
    normalize_proxy_usage,
};
pub(crate) use requests::{
    ConversationObservationInput, MAX_STATS_RANGE_MILLIS,
    attach_conversation_upstream_response_in_transaction, claim_request_event_locator,
    lock_key_budget_state, price_token_usage, proxy_contract_ceiling_micros,
    record_request_finished_in_transaction, record_request_started_in_transaction,
    reserve_usage_in_transaction, search_prefix, settle_token_usage_in_transaction,
    settle_token_usage_in_transaction_with_charge, valid_archive_identifier,
    validate_numeric_range,
};
#[cfg(test)]
pub(crate) use requests::{
    FILTERED_ACTIVITY_SOURCE_FACTS, FILTERED_ACTIVITY_SOURCE_PENDING,
    FILTERED_ACTIVITY_SOURCE_ROLLUPS,
};
pub use usage_analysis::{UsageAnalysisFilter, UsageAnalysisUpstreamFilter};

const CREDENTIAL_ROTATION_AAD: &str = "memeloop-token-center/credential-rotation-response/v1";
const CREDENTIAL_ROTATION_REPLAY_TTL_MILLIS: i64 = 24 * 60 * 60 * 1_000;
const SYNCHRONOUS_IMAGE_IDEMPOTENCY_LEASE_MILLIS: i64 = 15 * 60 * 1_000;
const GENERATION_JOB_IDEMPOTENCY_LOCK_SEED: i64 = 734_627_102_948_316;
const GENERATION_PREPARATION_LEASE_MILLIS: i64 = 15 * 60 * 1_000;
struct RotationReplay {
    response_ciphertext: Option<String>,
    expires_at: i64,
}

#[derive(Clone)]
pub struct Database {
    pool: AnyPool,
    backend: DatabaseBackend,
}

#[derive(Clone, Copy)]
enum DatabaseBackend {
    PostgreSql,
    Sqlite,
}

impl Database {
    pub async fn readiness_check(&self) -> Result<(), AppError> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        self.archive_staging_readiness_check().await?;
        Ok(())
    }

    pub async fn list_tenants(&self) -> Result<Vec<TenantView>, AppError> {
        let rows = sqlx::query("SELECT external_id FROM tenants ORDER BY external_id ASC")
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|row| {
                Ok(TenantView {
                    external_id: row.try_get("external_id")?,
                })
            })
            .collect()
    }

    pub async fn connect(database_url: &str) -> Result<Self, sqlx::Error> {
        Self::connect_with_max(database_url, 8).await
    }

    pub async fn connect_with_max(
        database_url: &str,
        maximum_connections: u32,
    ) -> Result<Self, sqlx::Error> {
        // `$n` placeholders are accepted by both PostgreSQL and SQLite. `sqlx::Any` deliberately
        // does not translate `?` into PostgreSQL placeholders, so all queries in this module use
        // the shared `$n` form.
        sqlx::any::install_default_drivers();
        let backend = if database_url.starts_with("sqlite:") {
            DatabaseBackend::Sqlite
        } else {
            DatabaseBackend::PostgreSql
        };
        let pool = AnyPoolOptions::new()
            .min_connections(0)
            .max_connections(maximum_connections.clamp(1, 32))
            .acquire_timeout(Duration::from_secs(10))
            .idle_timeout(Some(Duration::from_secs(5 * 60)))
            .max_lifetime(Some(Duration::from_secs(30 * 60)))
            .connect(database_url)
            .await?;
        Ok(Self { pool, backend })
    }

    pub async fn require_account_tenant(
        &self,
        account_id: Uuid,
        tenant_external_id: &str,
    ) -> Result<(), AppError> {
        let exists = sqlx::query(
            "SELECT a.id FROM credit_accounts a JOIN tenants t ON t.id = a.tenant_id WHERE a.id = $1 AND t.external_id = $2",
        )
        .bind(account_id.to_string())
        .bind(tenant_external_id)
        .fetch_optional(&self.pool)
        .await?
        .is_some();
        exists.then_some(()).ok_or(AppError::Forbidden)
    }

    pub async fn plugin_kv_get(
        &self,
        plugin_id: &str,
        key: &str,
    ) -> Result<Option<Vec<u8>>, AppError> {
        validate_plugin_kv_key(plugin_id, key)?;
        let row = sqlx::query("SELECT value FROM plugin_kv WHERE plugin_id = $1 AND key = $2")
            .bind(plugin_id)
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|row| row.try_get("value").map_err(AppError::from))
            .transpose()
    }

    pub async fn plugin_kv_put(
        &self,
        plugin_id: &str,
        key: &str,
        value: &[u8],
    ) -> Result<(), AppError> {
        const MAX_VALUE_BYTES: usize = 1024 * 1024;
        const MAX_PLUGIN_BYTES: i64 = 16 * 1024 * 1024;
        validate_plugin_kv_key(plugin_id, key)?;
        if value.len() > MAX_VALUE_BYTES {
            return Err(AppError::BadRequest("plugin KV value exceeds 1 MiB".into()));
        }
        let mut transaction = self.pool.begin().await?;
        if matches!(self.backend, DatabaseBackend::PostgreSql) {
            sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 734627102948313))")
                .bind(plugin_id)
                .execute(&mut *transaction)
                .await?;
        }
        let length_expression = match self.backend {
            DatabaseBackend::PostgreSql => "OCTET_LENGTH(value)",
            DatabaseBackend::Sqlite => "LENGTH(value)",
        };
        let usage_query = format!(
            "SELECT COALESCE(SUM({length_expression}), 0) AS total_bytes, COALESCE(MAX(CASE WHEN key = $2 THEN {length_expression} ELSE 0 END), 0) AS current_bytes FROM plugin_kv WHERE plugin_id = $1"
        );
        let usage = sqlx::query(&usage_query)
            .bind(plugin_id)
            .bind(key)
            .fetch_one(&mut *transaction)
            .await?;
        let total_bytes: i64 = usage.try_get("total_bytes")?;
        let current_bytes: i64 = usage.try_get("current_bytes")?;
        let next_bytes = total_bytes
            .saturating_sub(current_bytes)
            .saturating_add(value.len() as i64);
        if next_bytes > MAX_PLUGIN_BYTES {
            return Err(AppError::BadRequest(
                "plugin KV namespace exceeds 16 MiB".into(),
            ));
        }
        sqlx::query(
            "INSERT INTO plugin_kv (plugin_id, key, value, updated_at) VALUES ($1, $2, $3, $4) ON CONFLICT(plugin_id, key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        )
        .bind(plugin_id)
        .bind(key)
        .bind(value)
        .bind(unix_millis())
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }
}

fn credential_rotation_request_hash(resource_kind: &str, resource_id: Uuid) -> String {
    let canonical = format!(
        "memeloop-token-center/credential-rotation-request/v1\0{resource_kind}\0{resource_id}"
    );
    format!("{:x}", Sha256::digest(canonical.as_bytes()))
}

fn credential_rotation_aad(
    resource_kind: &str,
    resource_id: Uuid,
    idempotency_key: &str,
    request_hash: &str,
    expires_at: i64,
) -> Vec<u8> {
    format!(
        "{CREDENTIAL_ROTATION_AAD}\0{resource_kind}\0{resource_id}\0{idempotency_key}\0{request_hash}\0{expires_at}"
    )
    .into_bytes()
}

async fn claim_credential_rotation(
    transaction: &mut sqlx::Transaction<'_, sqlx::Any>,
    resource_kind: &str,
    resource_id: Uuid,
    idempotency_key: &str,
    request_hash: &str,
    now: i64,
    expires_at: i64,
) -> Result<Option<RotationReplay>, AppError> {
    let claimed = sqlx::query(
        "INSERT INTO credential_rotation_replays (idempotency_key, resource_kind, resource_id, request_hash, response_ciphertext, expires_at, created_at) VALUES ($1, $2, $3, $4, NULL, $5, $6) ON CONFLICT(idempotency_key) DO NOTHING",
    )
    .bind(idempotency_key)
    .bind(resource_kind)
    .bind(resource_id.to_string())
    .bind(request_hash)
    .bind(expires_at)
    .bind(now)
    .execute(&mut **transaction)
    .await?;
    if claimed.rows_affected() == 1 {
        return Ok(None);
    }

    let row = sqlx::query(
        "SELECT resource_kind, resource_id, request_hash, response_ciphertext, expires_at FROM credential_rotation_replays WHERE idempotency_key = $1",
    )
    .bind(idempotency_key)
    .fetch_one(&mut **transaction)
    .await?;
    let existing_kind: String = row.try_get("resource_kind")?;
    let existing_id: String = row.try_get("resource_id")?;
    let existing_hash: String = row.try_get("request_hash")?;
    if existing_kind != resource_kind
        || existing_id != resource_id.to_string()
        || existing_hash != request_hash
    {
        return Err(AppError::BadRequest(
            "Idempotency-Key was already used for a different credential rotation".into(),
        ));
    }
    Ok(Some(RotationReplay {
        response_ciphertext: row.try_get("response_ciphertext")?,
        expires_at: row.try_get("expires_at")?,
    }))
}

fn open_rotation_replay<T: for<'de> Deserialize<'de>>(
    replay: RotationReplay,
    resource_kind: &str,
    resource_id: Uuid,
    idempotency_key: &str,
    request_hash: &str,
    pepper: &[u8],
    now: i64,
) -> Result<T, AppError> {
    if replay.expires_at <= now {
        return Err(AppError::BadRequest(
            "idempotent credential rotation response is no longer available; rotate with a new Idempotency-Key"
                .into(),
        ));
    }
    let ciphertext = replay.response_ciphertext.ok_or_else(|| {
        AppError::BadRequest(
            "idempotent credential rotation response is no longer available; rotate with a new Idempotency-Key"
                .into(),
        )
    })?;
    let aad = credential_rotation_aad(
        resource_kind,
        resource_id,
        idempotency_key,
        request_hash,
        replay.expires_at,
    );
    open_private_json(&ciphertext, pepper, &aad)
}

#[allow(clippy::too_many_arguments)]
async fn store_credential_rotation_response<T: Serialize>(
    transaction: &mut sqlx::Transaction<'_, sqlx::Any>,
    idempotency_key: &str,
    response: &T,
    resource_kind: &str,
    resource_id: Uuid,
    request_hash: &str,
    expires_at: i64,
    pepper: &[u8],
) -> Result<(), AppError> {
    let aad = credential_rotation_aad(
        resource_kind,
        resource_id,
        idempotency_key,
        request_hash,
        expires_at,
    );
    let ciphertext = seal_private_json(response, pepper, &aad)?;
    let stored = sqlx::query(
        "UPDATE credential_rotation_replays SET response_ciphertext = $1 WHERE idempotency_key = $2 AND resource_kind = $3 AND resource_id = $4 AND request_hash = $5 AND response_ciphertext IS NULL",
    )
    .bind(ciphertext)
    .bind(idempotency_key)
    .bind(resource_kind)
    .bind(resource_id.to_string())
    .bind(request_hash)
    .execute(&mut **transaction)
    .await?;
    if stored.rows_affected() != 1 {
        return Err(AppError::Internal);
    }
    Ok(())
}

fn validate_currency(currency: &str) -> Result<(), AppError> {
    match currency.to_uppercase().as_str() {
        "USD" | "CNY" => Ok(()),
        _ => Err(AppError::BadRequest("currency must be USD or CNY".into())),
    }
}

fn validate_idempotency_key(value: &str, field: &str) -> Result<(), AppError> {
    let value = value.trim();
    if value.is_empty() || value.len() > 200 || !value.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(AppError::BadRequest(format!(
            "{field} must contain at most 200 visible ASCII characters"
        )));
    }
    Ok(())
}

fn validate_generation_job_idempotency(
    idempotency: &GenerationJobIdempotency,
) -> Result<(), AppError> {
    validate_idempotency_key(&idempotency.key, "Idempotency-Key")?;
    if idempotency.request_hash.len() != 64
        || !idempotency
            .request_hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(AppError::BadRequest(
            "generation request hash must be a lowercase BLAKE3 hex digest".into(),
        ));
    }
    Ok(())
}

fn validate_plugin_kv_key(plugin_id: &str, key: &str) -> Result<(), AppError> {
    if plugin_id.is_empty()
        || plugin_id.len() > 64
        || !plugin_id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(AppError::BadRequest(
            "plugin id must contain lowercase ASCII letters, digits, or hyphens".into(),
        ));
    }
    if key.is_empty()
        || key.len() > 256
        || !key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/'))
    {
        return Err(AppError::BadRequest(
            "plugin KV key must contain 1 to 256 safe ASCII characters".into(),
        ));
    }
    Ok(())
}

fn decimal_to_micros(value: Decimal) -> Result<i64, AppError> {
    let scaled = value * Decimal::from(crate::model::MONEY_SCALE);
    if !scaled.fract().is_zero() {
        return Err(AppError::BadRequest(
            "monetary values support at most 6 decimal places".into(),
        ));
    }
    scaled
        .to_i64()
        .ok_or_else(|| AppError::BadRequest("monetary value is out of range".into()))
}

fn validate_service_tier(service_tier: &str) -> Result<(), AppError> {
    if matches!(
        service_tier,
        "default" | "auto" | "priority" | "flex" | "scale" | "batch" | "standard_only"
    ) {
        Ok(())
    } else {
        Err(AppError::BadRequest(
            "service_tier must be default, auto, priority, flex, scale, batch, or standard_only"
                .into(),
        ))
    }
}

fn parse_uuid(value: String) -> Result<Uuid, AppError> {
    Uuid::parse_str(&value).map_err(|_| AppError::Internal)
}

fn generation_asset_download(row: AnyRow) -> Result<GenerationAssetDownload, AppError> {
    Ok(GenerationAssetDownload {
        view: GenerationAssetView {
            asset_id: parse_uuid(row.try_get("id")?)?,
            index: row.try_get("asset_index")?,
            mime_type: row.try_get("mime_type")?,
            size_bytes: row.try_get("size_bytes")?,
            filename: row.try_get("filename")?,
        },
        object_locator: row.try_get("object_locator")?,
    })
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub fn unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after Unix epoch")
        .as_millis() as i64
}

#[cfg(test)]
mod tests;
