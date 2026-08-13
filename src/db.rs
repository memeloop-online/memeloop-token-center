use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{Days, Utc};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{
    Any, AnyConnection, AnyPool, Row, Transaction,
    any::{AnyPoolOptions, AnyQueryResult, AnyRow},
};
use uuid::Uuid;

use crate::{
    conversation::{ConversationHints, RelationKind, build_prefix, extract_atoms},
    crypto,
    error::AppError,
    model::{
        AuthenticatedKey, AuthenticatedService, ConversationClusterDetail, ConversationClusterView,
        ConversationEdgeView, EntitlementReconcileResult, EntitlementView, GenerationJobView,
        GenerationJobWork, GenerationPrice, IssuedKey, IssuedServiceToken, KeyPolicy, KeyView,
        LedgerEntryView, LegacyCredentialView, ManagedKeyView, ModelPrice, ModelPriceTier,
        ModelPriceTierView, ModelPriceView, OperatorStats, RequestArchiveRefs, RequestEventView,
        RequestView, SelfStats, ServiceTokenView, StatsBucket, StatsSummary, TenantView,
        TokenUsage, UsageReservation, micros_to_decimal_string, priced_tokens,
    },
    provider::{
        ModelRouteView, ResolvedUpstream, UpstreamAccountView, UpstreamCredential, open_credential,
        open_private_json, seal_credential, seal_private_json, validate_config,
    },
};

const KEY_PROVISIONING_AAD: &[u8] = b"memeloop-token-center/key-provisioning-response/v1";
const CREDENTIAL_ROTATION_AAD: &str = "memeloop-token-center/credential-rotation-response/v1";
const CREDENTIAL_ROTATION_REPLAY_TTL_MILLIS: i64 = 24 * 60 * 60 * 1_000;
const KEY_ROTATION_RESOURCE: &str = "key";
const SERVICE_TOKEN_ROTATION_RESOURCE: &str = "service_token";
const MAX_STATS_RANGE_MILLIS: i64 = 93 * 86_400_000;
const FILTERED_ACTIVITY_SOURCE_AGGREGATED: &str = r#"
SELECT r.created_at,
       r.model,
       r.protocol,
       CASE WHEN r.status_code BETWEEN 200 AND 399 THEN 'success'
            WHEN r.status_code IS NULL THEN 'pending' ELSE 'failure' END AS status_class,
       COALESCE(r.error_code, '') AS error_code,
       r.input_tokens,
       r.output_tokens,
       r.cost_micros,
       CAST(1 AS BIGINT) AS requests
FROM request_records r
JOIN key_records k ON k.id = r.key_id AND k.tenant_id = r.tenant_id
JOIN principals p ON p.id = k.principal_id AND p.tenant_id = k.tenant_id
JOIN tenants t ON t.id = r.tenant_id
WHERE ($1 = '' OR t.external_id = $1)
  AND ($2 = '' OR r.key_id = $2)
  AND r.created_at >= $3 AND r.created_at <= $4
  AND ($5 = '' OR r.model = $5)
  AND ($6 = '' OR r.protocol = $6)
  AND ($7 = ''
       OR ($7 = 'success' AND r.status_code BETWEEN 200 AND 399)
       OR ($7 = 'error' AND r.status_code >= 400)
       OR ($7 = 'pending' AND r.status_code IS NULL))
  AND ($8 = '' OR r.error_code = $8)
  AND ($9 = '' OR r.upstream_account_id = $9)
  AND ($10 = '' OR r.model_route_id = $10)
  AND ($11 < 0 OR r.duration_ms >= $11)
  AND ($12 < 0 OR r.duration_ms <= $12)
  AND ($13 < 0 OR r.cost_micros >= $13)
  AND ($14 < 0 OR r.cost_micros <= $14)
  AND ($15 = '' OR LOWER(k.alias) LIKE $15 ESCAPE '\')
  AND ($16 = '' OR LOWER(p.external_id) LIKE $16 ESCAPE '\')
UNION ALL
SELECT a.day_bucket * 86400000 AS created_at,
       a.model,
       'generation' AS protocol,
       a.status_class,
       a.error_code,
       0 AS input_tokens,
       0 AS output_tokens,
       a.cost_micros,
       a.requests
FROM generation_daily_aggregates a
JOIN key_records k ON k.id = a.key_id AND k.tenant_id = a.tenant_id
JOIN principals p ON p.id = k.principal_id AND p.tenant_id = k.tenant_id
JOIN tenants t ON t.id = a.tenant_id
WHERE ($1 = '' OR t.external_id = $1)
  AND ($2 = '' OR a.key_id = $2)
  AND a.day_bucket >= $17 / 86400000
  AND a.day_bucket < $18 / 86400000
  AND ($5 = '' OR a.model = $5)
  AND ($6 = '' OR $6 = 'generation')
  AND ($7 = ''
       OR ($7 = 'success' AND a.status_class = 'success')
       OR ($7 = 'error' AND a.status_class = 'failure'))
  AND ($8 = '' OR a.error_code = $8)
  AND ($9 = '' OR a.upstream_account_id = $9)
  AND $10 = ''
  AND ($15 = '' OR LOWER(k.alias) LIKE $15 ESCAPE '\')
  AND ($16 = '' OR LOWER(p.external_id) LIKE $16 ESCAPE '\')
UNION ALL
SELECT f.created_at,
       f.model,
       'generation' AS protocol,
       f.status_class,
       f.error_code,
       0 AS input_tokens,
       0 AS output_tokens,
       f.cost_micros,
       CAST(1 AS BIGINT) AS requests
FROM generation_stats_facts f
JOIN key_records k ON k.id = f.key_id AND k.tenant_id = f.tenant_id
JOIN principals p ON p.id = k.principal_id AND p.tenant_id = k.tenant_id
JOIN tenants t ON t.id = f.tenant_id
WHERE ($1 = '' OR t.external_id = $1)
  AND ($2 = '' OR f.key_id = $2)
  AND f.created_at >= $3 AND f.created_at <= $4
  AND (f.created_at < $17 OR f.created_at >= $18)
  AND ($5 = '' OR f.model = $5)
  AND ($6 = '' OR $6 = 'generation')
  AND ($7 = ''
       OR ($7 = 'success' AND f.status_class = 'success')
       OR ($7 = 'error' AND f.status_class = 'failure'))
  AND ($8 = '' OR f.error_code = $8)
  AND ($9 = '' OR f.upstream_account_id = $9)
  AND $10 = ''
  AND ($15 = '' OR LOWER(k.alias) LIKE $15 ESCAPE '\')
  AND ($16 = '' OR LOWER(p.external_id) LIKE $16 ESCAPE '\')
  AND $17 >= 0 AND $18 >= 0
"#;
const FILTERED_ACTIVITY_SOURCE_GENERATION_FACTS: &str = r#"
SELECT r.created_at,
       r.model,
       r.protocol,
       CASE WHEN r.status_code BETWEEN 200 AND 399 THEN 'success'
            WHEN r.status_code IS NULL THEN 'pending' ELSE 'failure' END AS status_class,
       COALESCE(r.error_code, '') AS error_code,
       r.input_tokens,
       r.output_tokens,
       r.cost_micros,
       CAST(1 AS BIGINT) AS requests
FROM request_records r
JOIN key_records k ON k.id = r.key_id AND k.tenant_id = r.tenant_id
JOIN principals p ON p.id = k.principal_id AND p.tenant_id = k.tenant_id
JOIN tenants t ON t.id = r.tenant_id
WHERE ($1 = '' OR t.external_id = $1)
  AND ($2 = '' OR r.key_id = $2)
  AND r.created_at >= $3 AND r.created_at <= $4
  AND ($5 = '' OR r.model = $5)
  AND ($6 = '' OR r.protocol = $6)
  AND ($7 = ''
       OR ($7 = 'success' AND r.status_code BETWEEN 200 AND 399)
       OR ($7 = 'error' AND r.status_code >= 400)
       OR ($7 = 'pending' AND r.status_code IS NULL))
  AND ($8 = '' OR r.error_code = $8)
  AND ($9 = '' OR r.upstream_account_id = $9)
  AND ($10 = '' OR r.model_route_id = $10)
  AND ($11 < 0 OR r.duration_ms >= $11)
  AND ($12 < 0 OR r.duration_ms <= $12)
  AND ($13 < 0 OR r.cost_micros >= $13)
  AND ($14 < 0 OR r.cost_micros <= $14)
  AND ($15 = '' OR LOWER(k.alias) LIKE $15 ESCAPE '\')
  AND ($16 = '' OR LOWER(p.external_id) LIKE $16 ESCAPE '\')
UNION ALL
SELECT f.created_at,
       f.model,
       'generation' AS protocol,
       f.status_class,
       f.error_code,
       0 AS input_tokens,
       0 AS output_tokens,
       f.cost_micros,
       CAST(1 AS BIGINT) AS requests
FROM generation_stats_facts f
JOIN key_records k ON k.id = f.key_id AND k.tenant_id = f.tenant_id
JOIN principals p ON p.id = k.principal_id AND p.tenant_id = k.tenant_id
JOIN tenants t ON t.id = f.tenant_id
WHERE ($1 = '' OR t.external_id = $1)
  AND ($2 = '' OR f.key_id = $2)
  AND f.created_at >= $3 AND f.created_at <= $4
  AND ($5 = '' OR f.model = $5)
  AND ($6 = '' OR $6 = 'generation')
  AND ($7 = ''
       OR ($7 = 'success' AND f.status_class = 'success')
       OR ($7 = 'error' AND f.status_class = 'failure'))
  AND ($8 = '' OR f.error_code = $8)
  AND ($9 = '' OR f.upstream_account_id = $9)
  AND $10 = ''
  AND ($11 < 0 OR f.duration_ms >= $11)
  AND ($12 < 0 OR f.duration_ms <= $12)
  AND ($13 < 0 OR f.cost_micros >= $13)
  AND ($14 < 0 OR f.cost_micros <= $14)
  AND ($15 = '' OR LOWER(k.alias) LIKE $15 ESCAPE '\')
  AND ($16 = '' OR LOWER(p.external_id) LIKE $16 ESCAPE '\')
  AND $17 >= 0 AND $18 >= 0
"#;
const FILTERED_ACTIVITY_SOURCE_PENDING_GENERATION: &str = r#"
SELECT r.created_at,
       r.model,
       r.protocol,
       CASE WHEN r.status_code BETWEEN 200 AND 399 THEN 'success'
            WHEN r.status_code IS NULL THEN 'pending' ELSE 'failure' END AS status_class,
       COALESCE(r.error_code, '') AS error_code,
       r.input_tokens,
       r.output_tokens,
       r.cost_micros,
       CAST(1 AS BIGINT) AS requests
FROM request_records r
JOIN key_records k ON k.id = r.key_id AND k.tenant_id = r.tenant_id
JOIN principals p ON p.id = k.principal_id AND p.tenant_id = k.tenant_id
JOIN tenants t ON t.id = r.tenant_id
WHERE ($1 = '' OR t.external_id = $1)
  AND ($2 = '' OR r.key_id = $2)
  AND r.created_at >= $3 AND r.created_at <= $4
  AND ($5 = '' OR r.model = $5)
  AND ($6 = '' OR r.protocol = $6)
  AND $7 = 'pending'
  AND r.status_code IS NULL
  AND ($8 = '' OR r.error_code = $8)
  AND ($9 = '' OR r.upstream_account_id = $9)
  AND ($10 = '' OR r.model_route_id = $10)
  AND ($11 < 0 OR r.duration_ms >= $11)
  AND ($12 < 0 OR r.duration_ms <= $12)
  AND ($13 < 0 OR r.cost_micros >= $13)
  AND ($14 < 0 OR r.cost_micros <= $14)
  AND ($15 = '' OR LOWER(k.alias) LIKE $15 ESCAPE '\')
  AND ($16 = '' OR LOWER(p.external_id) LIKE $16 ESCAPE '\')
UNION ALL
SELECT g.created_at,
       g.public_model AS model,
       'generation' AS protocol,
       'pending' AS status_class,
       COALESCE(g.error_code, '') AS error_code,
       0 AS input_tokens,
       0 AS output_tokens,
       g.cost_micros,
       CAST(1 AS BIGINT) AS requests
FROM generation_jobs g
JOIN key_records k ON k.id = g.key_id AND k.tenant_id = g.tenant_id
JOIN principals p ON p.id = k.principal_id AND p.tenant_id = k.tenant_id
JOIN tenants t ON t.id = g.tenant_id
WHERE ($1 = '' OR t.external_id = $1)
  AND ($2 = '' OR g.key_id = $2)
  AND g.created_at >= $3 AND g.created_at <= $4
  AND g.status IN ('queued', 'running')
  AND ($5 = '' OR g.public_model = $5)
  AND ($6 = '' OR $6 = 'generation')
  AND $7 = 'pending'
  AND ($8 = '' OR g.error_code = $8)
  AND ($9 = '' OR g.upstream_account_id = $9)
  AND $10 = ''
  AND $11 < 0
  AND $12 < 0
  AND ($13 < 0 OR g.cost_micros >= $13)
  AND ($14 < 0 OR g.cost_micros <= $14)
  AND ($15 = '' OR LOWER(k.alias) LIKE $15 ESCAPE '\')
  AND ($16 = '' OR LOWER(p.external_id) LIKE $16 ESCAPE '\')
  AND $17 >= 0 AND $18 >= 0
"#;
const UPSTREAM_CREDENTIAL_ROTATION_RESOURCE: &str = "upstream_credential";
const UPSTREAM_OAUTH_REFRESH_RESOURCE: &str = "upstream_oauth_refresh";

#[derive(Serialize, Deserialize)]
struct StoredIssuedServiceToken {
    service_id: Uuid,
    name: String,
    credential_generation: i64,
    token: String,
    fingerprint: String,
    scopes: Vec<String>,
    tenant_external_id: Option<String>,
}

impl From<&IssuedServiceToken> for StoredIssuedServiceToken {
    fn from(value: &IssuedServiceToken) -> Self {
        Self {
            service_id: value.service_id,
            name: value.name.clone(),
            credential_generation: value.credential_generation,
            token: value.token.clone(),
            fingerprint: value.fingerprint.clone(),
            scopes: value.scopes.clone(),
            tenant_external_id: value.tenant_external_id.clone(),
        }
    }
}

impl From<StoredIssuedServiceToken> for IssuedServiceToken {
    fn from(value: StoredIssuedServiceToken) -> Self {
        Self {
            service_id: value.service_id,
            name: value.name,
            credential_generation: value.credential_generation,
            token: value.token,
            fingerprint: value.fingerprint,
            scopes: value.scopes,
            tenant_external_id: value.tenant_external_id,
        }
    }
}

struct RotationReplay {
    response_ciphertext: Option<String>,
    expires_at: i64,
}

#[derive(Clone)]
pub struct Database {
    pool: AnyPool,
    backend: DatabaseBackend,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PartitionMaintenanceReport {
    pub ready_partitions: usize,
    pub blocked_partitions: Vec<BlockedPartition>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockedPartition {
    pub table: String,
    pub partition: String,
    pub day: chrono::NaiveDate,
}

#[derive(Clone, Copy)]
enum DatabaseBackend {
    PostgreSql,
    Sqlite,
}

pub struct CreateKeyInput {
    pub tenant_external_id: String,
    pub principal_external_id: String,
    pub alias: String,
    pub currency: String,
    pub policy: KeyPolicy,
    pub initial_balance: Decimal,
    pub idempotency_key: Option<String>,
}

pub struct CreateServiceTokenInput {
    pub name: String,
    pub scopes: Vec<String>,
    pub tenant_external_id: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ReconcileEntitlementInput {
    pub tenant_external_id: String,
    pub account_id: Uuid,
    pub provider: String,
    pub external_subscription_id: String,
    pub external_cycle_id: String,
    pub period_start: i64,
    pub period_end: i64,
    pub currency: String,
    pub desired_micros: i64,
    pub version: i64,
    pub source: String,
    pub proration_json: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct CancelEntitlementInput {
    pub tenant_external_id: String,
    pub provider: String,
    pub external_subscription_id: String,
    pub external_cycle_id: Option<String>,
    pub version: i64,
    pub source: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ReplaceEntitlementInput {
    pub tenant_external_id: String,
    pub provider: String,
    pub external_subscription_id: String,
    pub version: i64,
    pub source: String,
    pub replacement: ReconcileEntitlementInput,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EntitlementOperation {
    Reconcile(ReconcileEntitlementInput),
    Cancel(CancelEntitlementInput),
    Replace(ReplaceEntitlementInput),
}

impl EntitlementOperation {
    fn tenant_external_id(&self) -> &str {
        match self {
            Self::Reconcile(input) => &input.tenant_external_id,
            Self::Cancel(input) => &input.tenant_external_id,
            Self::Replace(input) => &input.tenant_external_id,
        }
    }
}

pub struct NewRequest {
    pub request_id: Uuid,
    pub key_id: Uuid,
    pub tenant_id: Uuid,
    pub protocol: String,
    pub model: String,
    pub request_object: String,
    pub reservation_id: Uuid,
    pub upstream_account_id: Option<Uuid>,
    pub model_route_id: Option<Uuid>,
}

pub struct FinishRequest {
    pub request_id: Uuid,
    pub status_code: i64,
    pub duration_ms: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost_micros: i64,
    pub error_code: Option<String>,
    pub response_object: String,
}

#[derive(Clone, Debug)]
pub struct SessionArchiveMatchInput<'a> {
    pub tenant_external_id: &'a str,
    pub cpamp_source: &'a str,
    pub archive_source: &'a str,
    pub external_request_id: &'a str,
    pub started_at: i64,
    pub requested_model: Option<&'a str>,
    pub resolved_model: Option<&'a str>,
    pub credential_hash: Option<&'a str>,
    pub legacy_key_id: Option<&'a str>,
    pub record_digest: &'a str,
    pub time_tolerance_ms: i64,
}

#[derive(Clone, Debug)]
pub struct SessionArchiveTarget {
    pub tenant_id: Uuid,
    pub target_request_id: Uuid,
    pub request_created_at: i64,
    pub key: AuthenticatedKey,
    pub external_event_hash: String,
    pub source_created_at: i64,
    pub source_model: String,
    pub replay: bool,
}

pub struct SessionArchiveCommitInput<'a> {
    pub tenant_external_id: &'a str,
    pub archive_source: &'a str,
    pub external_request_id: &'a str,
    pub target: &'a SessionArchiveTarget,
    pub record_digest: &'a str,
    pub request_digest: Option<&'a str>,
    pub response_digest: Option<&'a str>,
    pub request_object: Option<&'a str>,
    pub response_object: Option<&'a str>,
    pub request_json: Option<&'a serde_json::Value>,
    pub conversation_hints: &'a ConversationHints,
    pub client_name: Option<&'a str>,
    pub source_started_at: i64,
}

#[derive(Clone, Debug, Default)]
pub struct RequestListFilter {
    pub limit: i64,
    pub from_created_at: Option<i64>,
    pub to_created_at: Option<i64>,
    pub before_created_at: Option<i64>,
    pub before_id: Option<Uuid>,
    pub key_id: Option<Uuid>,
    pub model: Option<String>,
    pub protocol: Option<String>,
    pub status: Option<String>,
    pub error_code: Option<String>,
    pub upstream_account_id: Option<Uuid>,
    pub route_id: Option<Uuid>,
    pub min_duration_ms: Option<i64>,
    pub max_duration_ms: Option<i64>,
    pub min_cost_micros: Option<i64>,
    pub max_cost_micros: Option<i64>,
    /// Operator-only, case-insensitive prefix search over the stable credential alias.
    pub key_alias: Option<String>,
    /// Operator-only, case-insensitive prefix search over the tenant principal identifier.
    pub principal: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct StatsFilter {
    pub from_created_at: Option<i64>,
    pub to_created_at: Option<i64>,
    pub key_id: Option<Uuid>,
    pub model: Option<String>,
    pub protocol: Option<String>,
    pub status: Option<String>,
    pub error_code: Option<String>,
    pub upstream_account_id: Option<Uuid>,
    pub route_id: Option<Uuid>,
    pub min_duration_ms: Option<i64>,
    pub max_duration_ms: Option<i64>,
    pub min_cost_micros: Option<i64>,
    pub max_cost_micros: Option<i64>,
    pub key_alias: Option<String>,
    pub principal: Option<String>,
}

struct ConversationSelection {
    observation_id: String,
    cluster_id: String,
    relation: RelationKind,
    confidence: i64,
    direct_parent: bool,
    same_turn: bool,
    semantic_prefix: bool,
    client_match: bool,
    write_edge: bool,
}

pub struct CreateUpstreamAccountInput {
    pub tenant_external_id: String,
    pub name: String,
    pub driver: String,
    pub config: serde_json::Value,
    pub credential: UpstreamCredential,
    pub oauth_session_id: Option<Uuid>,
}

#[derive(Clone, Debug)]
pub struct UpdateUpstreamAccountInput {
    pub name: String,
    pub config: serde_json::Value,
    pub expected_updated_at: i64,
}

pub struct CreateModelRouteInput {
    pub tenant_external_id: String,
    pub public_model: String,
    pub upstream_account_id: Uuid,
    pub upstream_model: String,
    pub protocol: String,
    pub priority: i64,
}

#[derive(Clone, Debug)]
pub struct UpdateModelRouteInput {
    pub public_model: String,
    pub upstream_account_id: Uuid,
    pub upstream_model: String,
    pub protocol: String,
    pub priority: i64,
    pub expected_updated_at: i64,
}

pub struct CreateGenerationJobInput {
    pub job_id: Uuid,
    pub key: AuthenticatedKey,
    pub upstream_account_id: Uuid,
    pub reservation: UsageReservation,
    pub public_model: String,
    pub upstream_model: String,
    pub driver: String,
    pub request_object: String,
    pub estimated_units: i64,
    pub billing_unit: String,
    pub micros_per_unit: i64,
}

#[derive(Clone, Debug)]
pub struct GenerationJobIdempotency {
    pub key: String,
    pub request_hash: String,
}

#[derive(Clone, Debug)]
pub enum CreateGenerationJobResult {
    Created(GenerationJobView),
    Replayed(GenerationJobView),
}

pub struct FinishGenerationJobInput<'a> {
    pub job_id: Uuid,
    pub worker_id: &'a str,
    pub status: &'a str,
    pub billed_units: i64,
    pub cost_micros: i64,
    pub result: Option<&'a serde_json::Value>,
    pub error_code: Option<&'a str>,
}

impl Database {
    pub async fn readiness_check(&self) -> Result<(), AppError> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
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

    pub async fn list_managed_keys(
        &self,
        tenant_external_id: Option<&str>,
        principal_external_id: Option<&str>,
    ) -> Result<Vec<ManagedKeyView>, AppError> {
        let rows = sqlx::query(
            "SELECT k.id, k.account_id, t.external_id AS tenant_external_id, p.external_id AS principal_external_id, k.alias, k.currency, k.status, k.credential_generation, COALESCE(c.fingerprint, lc.fingerprint) AS fingerprint, k.created_at, k.updated_at, k.policy_json, a.available_micros, a.reserved_micros FROM key_records k JOIN tenants t ON t.id = k.tenant_id JOIN principals p ON p.id = k.principal_id JOIN credit_accounts a ON a.id = k.account_id LEFT JOIN key_credentials c ON c.key_id = k.id AND c.generation = k.credential_generation AND c.revoked_at IS NULL LEFT JOIN legacy_key_credentials lc ON lc.key_id = k.id AND lc.generation = k.credential_generation AND lc.revoked_at IS NULL WHERE ($1 = '' OR t.external_id = $1) AND ($2 = '' OR p.external_id = $2) ORDER BY k.created_at DESC, k.id DESC LIMIT 500",
        )
        .bind(tenant_external_id.unwrap_or_default())
        .bind(principal_external_id.unwrap_or_default())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(managed_key_view).collect()
    }

    pub async fn set_key_status(&self, key_id: Uuid, status: &str) -> Result<String, AppError> {
        if !matches!(status, "active" | "suspended" | "revoked") {
            return Err(AppError::BadRequest(
                "credential status must be active, suspended, or revoked".into(),
            ));
        }
        let mut transaction = self.pool.begin().await?;
        let current = sqlx::query("SELECT status FROM key_records WHERE id = $1")
            .bind(key_id.to_string())
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(AppError::NotFound)?;
        if current.try_get::<String, _>("status")? == "revoked" && status != "revoked" {
            return Err(AppError::BadRequest(
                "a revoked credential cannot be reactivated".into(),
            ));
        }
        let changed = sqlx::query(
            "UPDATE key_records SET status = $1, updated_at = $2 WHERE id = $3 AND NOT (status = 'revoked' AND $4 <> 'revoked')",
        )
        .bind(status)
        .bind(unix_millis())
        .bind(key_id.to_string())
        .bind(status)
        .execute(&mut *transaction)
        .await?;
        if changed.rows_affected() != 1 {
            return Err(AppError::NotFound);
        }
        if status == "revoked" {
            let now = unix_millis();
            sqlx::query(
                "UPDATE key_credentials SET revoked_at = $1 WHERE key_id = $2 AND revoked_at IS NULL",
            )
            .bind(now)
            .bind(key_id.to_string())
            .execute(&mut *transaction)
            .await?;
            sqlx::query(
                "UPDATE legacy_key_credentials SET revoked_at = $1 WHERE key_id = $2 AND revoked_at IS NULL",
            )
            .bind(now)
            .bind(key_id.to_string())
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        Ok(status.to_owned())
    }

    pub async fn list_service_tokens(&self) -> Result<Vec<ServiceTokenView>, AppError> {
        let rows = sqlx::query(
            "SELECT p.id, p.name, p.status, p.credential_generation, p.created_at, p.updated_at, c.fingerprint, c.scopes_json, c.tenant_external_id FROM service_principals p JOIN service_credentials c ON c.service_principal_id = p.id AND c.generation = p.credential_generation ORDER BY p.created_at DESC, p.id DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(service_token_view).collect()
    }

    pub async fn set_service_token_status(
        &self,
        service_id: Uuid,
        status: &str,
    ) -> Result<String, AppError> {
        if !matches!(status, "active" | "suspended" | "revoked") {
            return Err(AppError::BadRequest(
                "service credential status must be active, suspended, or revoked".into(),
            ));
        }
        let mut transaction = self.pool.begin().await?;
        let current = sqlx::query("SELECT status FROM service_principals WHERE id = $1")
            .bind(service_id.to_string())
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(AppError::NotFound)?;
        if current.try_get::<String, _>("status")? == "revoked" && status != "revoked" {
            return Err(AppError::BadRequest(
                "a revoked service credential cannot be reactivated".into(),
            ));
        }
        let changed = sqlx::query(
            "UPDATE service_principals SET status = $1, updated_at = $2 WHERE id = $3 AND NOT (status = 'revoked' AND $4 <> 'revoked')",
        )
        .bind(status)
        .bind(unix_millis())
        .bind(service_id.to_string())
        .bind(status)
        .execute(&mut *transaction)
        .await?;
        if changed.rows_affected() != 1 {
            return Err(AppError::NotFound);
        }
        if status == "revoked" {
            sqlx::query(
                "UPDATE service_credentials SET revoked_at = $1 WHERE service_principal_id = $2 AND revoked_at IS NULL",
            )
            .bind(unix_millis())
            .bind(service_id.to_string())
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        Ok(status.to_owned())
    }

    pub async fn list_model_routes(
        &self,
        tenant_external_id: Option<&str>,
    ) -> Result<Vec<ModelRouteView>, AppError> {
        let rows = sqlx::query(
            "SELECT r.id, r.tenant_id, t.external_id AS tenant_external_id, r.public_model, r.upstream_account_id, r.upstream_model, r.protocol, r.priority, r.enabled, r.created_at, r.updated_at FROM model_routes r JOIN tenants t ON t.id = r.tenant_id WHERE ($1 = '' OR t.external_id = $1) ORDER BY r.public_model, r.priority, r.id",
        )
        .bind(tenant_external_id.unwrap_or_default())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(model_route_view).collect()
    }

    pub async fn list_generation_prices(
        &self,
        currency: &str,
    ) -> Result<Vec<GenerationPrice>, AppError> {
        validate_currency(currency)?;
        let rows = sqlx::query(
            "SELECT id, model, currency, billing_unit, micros_per_unit FROM generation_prices WHERE currency = $1 ORDER BY model",
        )
        .bind(currency.to_uppercase())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(generation_price_view).collect()
    }

    pub async fn list_account_ledger(
        &self,
        account_id: Uuid,
        limit: i64,
    ) -> Result<Vec<LedgerEntryView>, AppError> {
        let rows = sqlx::query(
            "SELECT id, kind, amount_micros, currency, source, idempotency_key, created_at FROM ledger_entries WHERE account_id = $1 ORDER BY created_at DESC, id DESC LIMIT $2",
        )
        .bind(account_id.to_string())
        .bind(limit.clamp(1, 500))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(LedgerEntryView {
                    entry_id: parse_uuid(row.try_get("id")?)?,
                    kind: row.try_get("kind")?,
                    amount: micros_to_decimal_string(row.try_get("amount_micros")?),
                    currency: row.try_get("currency")?,
                    source: row.try_get("source")?,
                    idempotency_key: row.try_get("idempotency_key")?,
                    created_at: row.try_get("created_at")?,
                })
            })
            .collect()
    }

    pub async fn list_entitlements(
        &self,
        tenant_external_id: Option<&str>,
        provider: Option<&str>,
        external_subscription_id: Option<&str>,
    ) -> Result<Vec<EntitlementView>, AppError> {
        let rows = sqlx::query(
            "SELECT e.id AS entitlement_id, c.id AS cycle_id, t.external_id AS tenant_external_id, e.account_id, e.provider, e.external_subscription_id, c.external_cycle_id, c.period_start, c.period_end, c.currency, c.desired_micros, c.consumed_micros, c.funded_micros, e.status, e.version, e.replaced_by_entitlement_id, e.created_at, e.updated_at FROM subscription_entitlements e JOIN tenants t ON t.id = e.tenant_id JOIN entitlement_cycles c ON c.id = e.current_cycle_id WHERE ($1 = '' OR t.external_id = $1) AND ($2 = '' OR e.provider = $2) AND ($3 = '' OR e.external_subscription_id = $3) ORDER BY e.updated_at DESC, e.id DESC LIMIT 500",
        )
        .bind(tenant_external_id.unwrap_or_default())
        .bind(provider.unwrap_or_default())
        .bind(external_subscription_id.unwrap_or_default())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(entitlement_view).collect()
    }

    /// Reconciles an external subscription snapshot against durable account
    /// credit. The tenant and idempotency key form the replay namespace, while
    /// the stable provider/subscription identity survives credential rotation.
    pub async fn reconcile_entitlement(
        &self,
        mut operation: EntitlementOperation,
        idempotency_key: &str,
    ) -> Result<EntitlementReconcileResult, AppError> {
        validate_idempotency_key(idempotency_key, "Idempotency-Key")?;
        canonicalize_entitlement_operation(&mut operation)?;
        validate_entitlement_operation(&operation)?;
        let canonical = serde_json::to_vec(&operation).map_err(|_| AppError::Internal)?;
        let request_hash = format!("{:x}", Sha256::digest(&canonical));
        let tenant_external_id = operation.tenant_external_id().to_owned();
        let now = unix_millis();
        let reconciliation_id = Uuid::now_v7();
        let mut tx = self.pool.begin().await?;
        let tenant = sqlx::query("SELECT id FROM tenants WHERE external_id = $1")
            .bind(&tenant_external_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AppError::NotFound)?;
        let tenant_id: String = tenant.try_get("id")?;
        if matches!(self.backend, DatabaseBackend::PostgreSql) {
            sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 734627102948315))")
                .bind(format!("{tenant_id}:{idempotency_key}"))
                .execute(&mut *tx)
                .await?;
        } else {
            // Acquire SQLite's write lock before checking the replay row so two
            // concurrent first deliveries cannot both perform ledger changes.
            sqlx::query("UPDATE tenants SET created_at = created_at WHERE id = $1")
                .bind(&tenant_id)
                .execute(&mut *tx)
                .await?;
        }
        if let Some(replay) = sqlx::query(
            "SELECT request_hash, response_json FROM entitlement_reconciliations WHERE tenant_id = $1 AND idempotency_key = $2",
        )
        .bind(&tenant_id)
        .bind(idempotency_key)
        .fetch_optional(&mut *tx)
        .await?
        {
            let existing_hash: String = replay.try_get("request_hash")?;
            if existing_hash != request_hash {
                return Err(AppError::Conflict(
                    "Idempotency-Key was already used for a different entitlement request".into(),
                ));
            }
            let response_json: String = replay.try_get("response_json")?;
            let response =
                serde_json::from_str(&response_json).map_err(|_| AppError::Internal)?;
            tx.commit().await?;
            return Ok(response);
        }

        let result = match operation {
            EntitlementOperation::Reconcile(input) => {
                reconcile_entitlement_snapshot(&mut tx, &tenant_id, &input, reconciliation_id, now)
                    .await?
            }
            EntitlementOperation::Cancel(input) => {
                cancel_entitlement_snapshot(&mut tx, &tenant_id, &input, reconciliation_id, now)
                    .await?
            }
            EntitlementOperation::Replace(input) => {
                replace_entitlement_snapshot(&mut tx, &tenant_id, &input, reconciliation_id, now)
                    .await?
            }
        };
        let response_json = serde_json::to_string(&result).map_err(|_| AppError::Internal)?;
        sqlx::query(
            "INSERT INTO entitlement_reconciliations (id, tenant_id, idempotency_key, request_hash, response_json, created_at) VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(reconciliation_id.to_string())
        .bind(&tenant_id)
        .bind(idempotency_key)
        .bind(request_hash)
        .bind(response_json)
        .bind(now)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(result)
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

    pub async fn migrate(&self) -> Result<(), sqlx::Error> {
        let mut transaction = self.pool.begin().await?;
        if matches!(self.backend, DatabaseBackend::PostgreSql) {
            sqlx::query("SELECT pg_advisory_xact_lock(734627102948311)")
                .execute(&mut *transaction)
                .await?;
        }
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS schema_migrations (version BIGINT PRIMARY KEY, name TEXT NOT NULL, applied_at BIGINT NOT NULL)",
        )
        .execute(&mut *transaction)
        .await?;
        let migrations = match self.backend {
            DatabaseBackend::PostgreSql => POSTGRES_MIGRATIONS,
            DatabaseBackend::Sqlite => SQLITE_MIGRATIONS,
        };
        apply_migration_range(&mut transaction, migrations, i64::MIN, 1).await?;
        for column in ["upstream_account_id", "model_route_id"] {
            let exists = match self.backend {
                DatabaseBackend::PostgreSql => sqlx::query(
                    "SELECT column_name::TEXT AS column_name FROM information_schema.columns WHERE table_schema = current_schema() AND table_name = 'request_records' AND column_name = $1",
                )
                .bind(column)
                .fetch_optional(&mut *transaction)
                .await?
                .is_some(),
                DatabaseBackend::Sqlite => sqlx::query(
                    "SELECT name FROM pragma_table_info('request_records') WHERE name = $1",
                )
                .bind(column)
                .fetch_optional(&mut *transaction)
                .await?
                .is_some(),
            };
            if !exists {
                sqlx::query(&format!(
                    "ALTER TABLE request_records ADD COLUMN {column} TEXT"
                ))
                .execute(&mut *transaction)
                .await?;
            }
        }
        let oauth_session_column_exists = match self.backend {
            DatabaseBackend::PostgreSql => sqlx::query(
                "SELECT column_name::TEXT AS column_name FROM information_schema.columns WHERE table_schema = current_schema() AND table_name = 'upstream_accounts' AND column_name = 'oauth_session_id'",
            )
            .fetch_optional(&mut *transaction)
            .await?
            .is_some(),
            DatabaseBackend::Sqlite => sqlx::query(
                "SELECT name FROM pragma_table_info('upstream_accounts') WHERE name = 'oauth_session_id'",
            )
            .fetch_optional(&mut *transaction)
            .await?
            .is_some(),
        };
        if !oauth_session_column_exists {
            sqlx::query("ALTER TABLE upstream_accounts ADD COLUMN oauth_session_id TEXT")
                .execute(&mut *transaction)
                .await?;
        }
        sqlx::query(
            "CREATE UNIQUE INDEX IF NOT EXISTS upstream_accounts_oauth_session_idx ON upstream_accounts (oauth_session_id) WHERE oauth_session_id IS NOT NULL",
        )
        .execute(&mut *transaction)
        .await?;
        apply_migration_range(&mut transaction, migrations, 2, i64::MAX).await?;
        if matches!(self.backend, DatabaseBackend::PostgreSql) {
            maintain_postgres_partitions(&mut transaction).await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    pub async fn maintain_partitions(&self) -> Result<PartitionMaintenanceReport, sqlx::Error> {
        if matches!(self.backend, DatabaseBackend::PostgreSql) {
            let mut transaction = self.pool.begin().await?;
            sqlx::query("SELECT pg_advisory_xact_lock(734627102948311)")
                .execute(&mut *transaction)
                .await?;
            let report = maintain_postgres_partitions(&mut transaction).await?;
            transaction.commit().await?;
            return Ok(report);
        }
        Ok(PartitionMaintenanceReport::default())
    }

    pub async fn create_key(
        &self,
        input: CreateKeyInput,
        pepper: &[u8],
    ) -> Result<IssuedKey, AppError> {
        validate_currency(&input.currency)?;
        validate_key_input(&input)?;
        let idempotency_key = input
            .idempotency_key
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        if input.idempotency_key.is_some() && idempotency_key.is_none() {
            return Err(AppError::BadRequest(
                "Idempotency-Key cannot be empty".into(),
            ));
        }
        if idempotency_key.is_some_and(|value| {
            value.len() > 200 || !value.bytes().all(|byte| byte.is_ascii_graphic())
        }) {
            return Err(AppError::BadRequest(
                "Idempotency-Key must be at most 200 visible ASCII characters".into(),
            ));
        }
        let provisioning_request_hash = idempotency_key.map(|_| {
            let canonical = serde_json::to_vec(&serde_json::json!({
                "tenant_external_id": input.tenant_external_id.trim(),
                "principal_external_id": input.principal_external_id.trim(),
                "alias": input.alias.trim(),
                "currency": input.currency.to_uppercase(),
                "policy": input.policy,
                "initial_balance": input.initial_balance.normalize().to_string()
            }))
            .expect("key provisioning request is JSON serializable");
            format!("{:x}", Sha256::digest(canonical))
        });
        let now = unix_millis();
        let tenant_id = Uuid::now_v7();
        let principal_id = Uuid::now_v7();
        let account_id = Uuid::now_v7();
        let key_id = Uuid::now_v7();
        let issued = crypto::issue_credential(key_id, pepper);
        let policy_json = serde_json::to_string(&input.policy).map_err(|_| AppError::Internal)?;
        let initial_balance_micros = decimal_to_micros(input.initial_balance)?;
        let mut tx = self.pool.begin().await?;

        if let Some(idempotency_key) = idempotency_key {
            if matches!(self.backend, DatabaseBackend::PostgreSql) {
                sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 734627102948312))")
                    .bind(format!(
                        "{}:{idempotency_key}",
                        input.tenant_external_id.trim()
                    ))
                    .execute(&mut *tx)
                    .await?;
            }
            let existing = sqlx::query(
                "SELECT k.provisioning_request_hash, k.issued_key_ciphertext FROM key_records k JOIN tenants t ON t.id = k.tenant_id WHERE k.provisioning_idempotency_key = $1 AND t.external_id = $2",
            )
            .bind(idempotency_key)
            .bind(input.tenant_external_id.trim())
            .fetch_optional(&mut *tx)
            .await?;
            if let Some(existing) = existing {
                let existing_hash: Option<String> =
                    existing.try_get("provisioning_request_hash")?;
                if existing_hash.as_deref() != provisioning_request_hash.as_deref() {
                    return Err(AppError::BadRequest(
                        "Idempotency-Key was already used with a different key request".into(),
                    ));
                }
                let ciphertext: Option<String> = existing.try_get("issued_key_ciphertext")?;
                let issued = open_private_json(
                    ciphertext.as_deref().ok_or_else(|| {
                        AppError::BadRequest(
                            "idempotent key provisioning response is no longer available; rotate the key"
                                .into(),
                        )
                    })?,
                    pepper,
                    KEY_PROVISIONING_AAD,
                )?;
                tx.commit().await?;
                return Ok(issued);
            }
        }

        sqlx::query(
            "INSERT INTO tenants (id, external_id, created_at) VALUES ($1, $2, $3) ON CONFLICT(external_id) DO NOTHING",
        )
        .bind(tenant_id.to_string())
        .bind(&input.tenant_external_id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        let tenant_id: String = sqlx::query("SELECT id FROM tenants WHERE external_id = $1")
            .bind(&input.tenant_external_id)
            .fetch_one(&mut *tx)
            .await?
            .try_get("id")?;

        sqlx::query(
            "INSERT INTO principals (id, tenant_id, external_id, created_at) VALUES ($1, $2, $3, $4) ON CONFLICT(tenant_id, external_id) DO NOTHING",
        )
        .bind(principal_id.to_string())
        .bind(&tenant_id)
        .bind(&input.principal_external_id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        let principal_id: String =
            sqlx::query("SELECT id FROM principals WHERE tenant_id = $1 AND external_id = $2")
                .bind(&tenant_id)
                .bind(&input.principal_external_id)
                .fetch_one(&mut *tx)
                .await?
                .try_get("id")?;

        sqlx::query(
            "INSERT INTO credit_accounts (id, tenant_id, principal_id, currency, available_micros, reserved_micros, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, 0, $6, $7)",
        )
        .bind(account_id.to_string())
        .bind(&tenant_id)
        .bind(&principal_id)
        .bind(input.currency.to_uppercase())
        .bind(initial_balance_micros)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "INSERT INTO account_usage_state (account_id, settled_lifetime_micros, updated_at) VALUES ($1, 0, $2)",
        )
        .bind(account_id.to_string())
        .bind(now)
        .execute(&mut *tx)
        .await?;

        let issued_key = IssuedKey {
            key_id,
            account_id,
            alias: input.alias.clone(),
            currency: input.currency.to_uppercase(),
            credential_generation: 1,
            key: issued.secret.clone(),
            fingerprint: issued.fingerprint.clone(),
        };
        let issued_key_ciphertext = idempotency_key
            .map(|_| seal_private_json(&issued_key, pepper, KEY_PROVISIONING_AAD))
            .transpose()?;
        sqlx::query(
            "INSERT INTO key_records (id, tenant_id, principal_id, account_id, alias, currency, policy_json, status, credential_generation, provisioning_idempotency_key, provisioning_request_hash, issued_key_ciphertext, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, $6, $7, 'active', 1, $8, $9, $10, $11, $12)",
        )
        .bind(key_id.to_string())
        .bind(&tenant_id)
        .bind(&principal_id)
        .bind(account_id.to_string())
        .bind(&input.alias)
        .bind(input.currency.to_uppercase())
        .bind(policy_json)
        .bind(idempotency_key)
        .bind(provisioning_request_hash)
        .bind(issued_key_ciphertext)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "INSERT INTO key_budget_state (key_id, settled_lifetime_micros, reserved_micros, updated_at) VALUES ($1, 0, 0, $2)",
        )
        .bind(key_id.to_string())
        .bind(now)
        .execute(&mut *tx)
        .await?;

        insert_credential(&mut tx, &issued, 1, now).await?;
        if initial_balance_micros != 0 {
            sqlx::query(
                "INSERT INTO ledger_entries (id, account_id, key_id, kind, amount_micros, currency, source, created_at) VALUES ($1, $2, $3, 'grant', $4, $5, 'initial', $6)",
            )
            .bind(Uuid::now_v7().to_string())
            .bind(account_id.to_string())
            .bind(key_id.to_string())
            .bind(initial_balance_micros)
            .bind(input.currency.to_uppercase())
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(issued_key)
    }

    pub async fn create_service_token(
        &self,
        input: CreateServiceTokenInput,
        pepper: &[u8],
    ) -> Result<IssuedServiceToken, AppError> {
        validate_service_token_input(&input)?;
        let now = unix_millis();
        let service_id = Uuid::now_v7();
        let issued = crypto::issue_service_credential(service_id, pepper);
        let scopes_json = serde_json::to_string(&input.scopes).map_err(|_| AppError::Internal)?;
        let mut transaction = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO service_principals (id, name, status, credential_generation, created_at, updated_at) VALUES ($1, $2, 'active', 1, $3, $4)",
        )
        .bind(service_id.to_string())
        .bind(input.name.trim())
        .bind(now)
        .bind(now)
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            "INSERT INTO service_credentials (id, service_principal_id, generation, secret_hash, fingerprint, scopes_json, tenant_external_id, created_at) VALUES ($1, $2, 1, $3, $4, $5, $6, $7)",
        )
        .bind(issued.credential_id.to_string())
        .bind(service_id.to_string())
        .bind(&issued.secret_hash)
        .bind(&issued.fingerprint)
        .bind(scopes_json)
        .bind(&input.tenant_external_id)
        .bind(now)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(IssuedServiceToken {
            service_id,
            name: input.name.trim().to_owned(),
            credential_generation: 1,
            token: issued.secret,
            fingerprint: issued.fingerprint,
            scopes: input.scopes,
            tenant_external_id: input.tenant_external_id,
        })
    }

    pub async fn update_key_policy(
        &self,
        key_id: Uuid,
        policy: KeyPolicy,
    ) -> Result<KeyPolicy, AppError> {
        validate_policy_budgets(&policy)?;
        let policy_json = serde_json::to_string(&policy).map_err(|_| AppError::Internal)?;
        let result = sqlx::query(
            "UPDATE key_records SET policy_json = $1, updated_at = $2 WHERE id = $3 AND status = 'active'",
        )
        .bind(policy_json)
        .bind(unix_millis())
        .bind(key_id.to_string())
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(AppError::NotFound);
        }
        Ok(policy)
    }

    pub async fn rotate_service_token(
        &self,
        service_id: Uuid,
        idempotency_key: &str,
        pepper: &[u8],
    ) -> Result<IssuedServiceToken, AppError> {
        validate_idempotency_key(idempotency_key, "Idempotency-Key")?;
        let idempotency_key = idempotency_key.trim();
        let now = unix_millis();
        let request_hash =
            credential_rotation_request_hash(SERVICE_TOKEN_ROTATION_RESOURCE, service_id);
        let expires_at = now.saturating_add(CREDENTIAL_ROTATION_REPLAY_TTL_MILLIS);
        let mut transaction = self.pool.begin().await?;
        if let Some(replay) = claim_credential_rotation(
            &mut transaction,
            SERVICE_TOKEN_ROTATION_RESOURCE,
            service_id,
            idempotency_key,
            &request_hash,
            now,
            expires_at,
        )
        .await?
        {
            let issued = open_rotation_replay::<StoredIssuedServiceToken>(
                replay,
                SERVICE_TOKEN_ROTATION_RESOURCE,
                service_id,
                idempotency_key,
                &request_hash,
                pepper,
                now,
            )?;
            transaction.commit().await?;
            return Ok(issued.into());
        }

        let select = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT p.name, p.status, p.credential_generation, c.scopes_json, c.tenant_external_id FROM service_principals p JOIN service_credentials c ON c.service_principal_id = p.id AND c.generation = p.credential_generation AND c.revoked_at IS NULL WHERE p.id = $1 FOR UPDATE OF p"
            }
            DatabaseBackend::Sqlite => {
                "SELECT p.name, p.status, p.credential_generation, c.scopes_json, c.tenant_external_id FROM service_principals p JOIN service_credentials c ON c.service_principal_id = p.id AND c.generation = p.credential_generation AND c.revoked_at IS NULL WHERE p.id = $1"
            }
        };
        let row = sqlx::query(select)
            .bind(service_id.to_string())
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(AppError::NotFound)?;
        if row.try_get::<String, _>("status")? != "active" {
            return Err(AppError::Forbidden);
        }
        let generation = row.try_get::<i64, _>("credential_generation")? + 1;
        let scopes_json: String = row.try_get("scopes_json")?;
        let scopes: Vec<String> =
            serde_json::from_str(&scopes_json).map_err(|_| AppError::Internal)?;
        let tenant_external_id: Option<String> = row.try_get("tenant_external_id")?;
        let name: String = row.try_get("name")?;
        let issued = crypto::issue_service_credential(service_id, pepper);
        sqlx::query(
            "UPDATE service_credentials SET revoked_at = $1 WHERE service_principal_id = $2 AND revoked_at IS NULL",
        )
        .bind(now)
        .bind(service_id.to_string())
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            "INSERT INTO service_credentials (id, service_principal_id, generation, secret_hash, fingerprint, scopes_json, tenant_external_id, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(issued.credential_id.to_string())
        .bind(service_id.to_string())
        .bind(generation)
        .bind(&issued.secret_hash)
        .bind(&issued.fingerprint)
        .bind(scopes_json)
        .bind(&tenant_external_id)
        .bind(now)
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            "UPDATE service_principals SET credential_generation = $1, updated_at = $2 WHERE id = $3",
        )
        .bind(generation)
        .bind(now)
        .bind(service_id.to_string())
        .execute(&mut *transaction)
        .await?;
        let response = IssuedServiceToken {
            service_id,
            name,
            credential_generation: generation,
            token: issued.secret,
            fingerprint: issued.fingerprint,
            scopes,
            tenant_external_id,
        };
        store_credential_rotation_response(
            &mut transaction,
            idempotency_key,
            &StoredIssuedServiceToken::from(&response),
            SERVICE_TOKEN_ROTATION_RESOURCE,
            service_id,
            &request_hash,
            expires_at,
            pepper,
        )
        .await?;
        transaction.commit().await?;
        Ok(response)
    }

    pub async fn authenticate_service_token(
        &self,
        value: &str,
        pepper: &[u8],
    ) -> Result<AuthenticatedService, AppError> {
        let parsed = crypto::parse_service_credential(value).ok_or(AppError::Unauthorized)?;
        let row = sqlx::query(
            "SELECT p.status, c.secret_hash, c.scopes_json, c.tenant_external_id FROM service_principals p JOIN service_credentials c ON c.service_principal_id = p.id AND c.generation = p.credential_generation AND c.revoked_at IS NULL WHERE p.id = $1",
        )
        .bind(parsed.key_id.to_string())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::Unauthorized)?;
        let expected: Vec<u8> = row.try_get("secret_hash")?;
        if row.try_get::<String, _>("status")? != "active"
            || !crypto::verify_credential(value, pepper, &expected)
        {
            return Err(AppError::Unauthorized);
        }
        let scopes_json: String = row.try_get("scopes_json")?;
        Ok(AuthenticatedService {
            service_id: Some(parsed.key_id),
            scopes: serde_json::from_str(&scopes_json).map_err(|_| AppError::Internal)?,
            tenant_external_id: row.try_get("tenant_external_id")?,
        })
    }

    pub async fn rotate_key(
        &self,
        key_id: Uuid,
        idempotency_key: &str,
        pepper: &[u8],
    ) -> Result<IssuedKey, AppError> {
        validate_idempotency_key(idempotency_key, "Idempotency-Key")?;
        let idempotency_key = idempotency_key.trim();
        let now = unix_millis();
        let request_hash = credential_rotation_request_hash(KEY_ROTATION_RESOURCE, key_id);
        let expires_at = now.saturating_add(CREDENTIAL_ROTATION_REPLAY_TTL_MILLIS);
        let mut tx = self.pool.begin().await?;
        if let Some(replay) = claim_credential_rotation(
            &mut tx,
            KEY_ROTATION_RESOURCE,
            key_id,
            idempotency_key,
            &request_hash,
            now,
            expires_at,
        )
        .await?
        {
            let issued = open_rotation_replay(
                replay,
                KEY_ROTATION_RESOURCE,
                key_id,
                idempotency_key,
                &request_hash,
                pepper,
                now,
            )?;
            tx.commit().await?;
            return Ok(issued);
        }

        let select = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT account_id, alias, currency, credential_generation, status FROM key_records WHERE id = $1 FOR UPDATE"
            }
            DatabaseBackend::Sqlite => {
                "SELECT account_id, alias, currency, credential_generation, status FROM key_records WHERE id = $1"
            }
        };
        let row = sqlx::query(select)
            .bind(key_id.to_string())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AppError::NotFound)?;
        let status: String = row.try_get("status")?;
        if status != "active" {
            return Err(AppError::Forbidden);
        }
        let generation: i64 = row.try_get::<i64, _>("credential_generation")? + 1;
        let account_id: String = row.try_get("account_id")?;
        let alias: String = row.try_get("alias")?;
        let currency: String = row.try_get("currency")?;
        let issued = crypto::issue_credential(key_id, pepper);

        sqlx::query(
            "UPDATE key_credentials SET revoked_at = $1 WHERE key_id = $2 AND revoked_at IS NULL",
        )
        .bind(now)
        .bind(key_id.to_string())
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE legacy_key_credentials SET revoked_at = $1 WHERE key_id = $2 AND revoked_at IS NULL",
        )
        .bind(now)
        .bind(key_id.to_string())
        .execute(&mut *tx)
        .await?;
        insert_credential(&mut tx, &issued, generation, now).await?;
        sqlx::query(
            "UPDATE key_records SET credential_generation = $1, updated_at = $2 WHERE id = $3",
        )
        .bind(generation)
        .bind(now)
        .bind(key_id.to_string())
        .execute(&mut *tx)
        .await?;
        let response = IssuedKey {
            key_id,
            account_id: Uuid::parse_str(&account_id).map_err(|_| AppError::Internal)?,
            alias,
            currency,
            credential_generation: generation,
            key: issued.secret,
            fingerprint: issued.fingerprint,
        };
        store_credential_rotation_response(
            &mut tx,
            idempotency_key,
            &response,
            KEY_ROTATION_RESOURCE,
            key_id,
            &request_hash,
            expires_at,
            pepper,
        )
        .await?;
        tx.commit().await?;
        Ok(response)
    }

    pub async fn create_upstream_account(
        &self,
        input: CreateUpstreamAccountInput,
        key_material: &[u8],
    ) -> Result<UpstreamAccountView, AppError> {
        validate_upstream_account_name(&input.name)?;
        let _ = validate_config(&input.config)?;
        let now = unix_millis();
        let account_id = Uuid::now_v7();
        let tenant_candidate = Uuid::now_v7();
        let config_json = serde_json::to_string(&input.config).map_err(|_| AppError::Internal)?;
        let credential_ciphertext = seal_credential(&input.credential, key_material)?;
        let auth_kind = input.credential.auth_kind();
        let credential_expires_at = input.credential.expires_at();
        let mut tx = self.pool.begin().await?;

        sqlx::query(
            "INSERT INTO tenants (id, external_id, created_at) VALUES ($1, $2, $3) ON CONFLICT(external_id) DO NOTHING",
        )
        .bind(tenant_candidate.to_string())
        .bind(&input.tenant_external_id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        let tenant_id: String = sqlx::query("SELECT id FROM tenants WHERE external_id = $1")
            .bind(&input.tenant_external_id)
            .fetch_one(&mut *tx)
            .await?
            .try_get("id")?;
        if let Some(session_id) = input.oauth_session_id {
            let existing = sqlx::query(
                "SELECT a.id, a.tenant_id, t.external_id AS tenant_external_id, a.name, a.driver, a.auth_kind, a.config_json, a.status, a.credential_generation, a.created_at, a.updated_at, c.expires_at, (SELECT COUNT(*) FROM model_routes r WHERE r.upstream_account_id = a.id) AS route_count FROM upstream_accounts a JOIN tenants t ON t.id = a.tenant_id JOIN upstream_credentials c ON c.upstream_account_id = a.id AND c.generation = a.credential_generation WHERE a.oauth_session_id = $1",
            )
            .bind(session_id.to_string())
            .fetch_optional(&mut *tx)
            .await?;
            if let Some(existing) = existing {
                tx.commit().await?;
                return upstream_account_view(existing);
            }
        }
        if sqlx::query("SELECT id FROM upstream_accounts WHERE tenant_id = $1 AND name = $2")
            .bind(&tenant_id)
            .bind(input.name.trim())
            .fetch_optional(&mut *tx)
            .await?
            .is_some()
        {
            return Err(AppError::Conflict(
                "another upstream provider already uses this name".into(),
            ));
        }
        sqlx::query(
            "INSERT INTO upstream_accounts (id, tenant_id, name, driver, auth_kind, config_json, status, credential_generation, oauth_session_id, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, $6, 'active', 1, $7, $8, $9)",
        )
        .bind(account_id.to_string())
        .bind(&tenant_id)
        .bind(input.name.trim())
        .bind(&input.driver)
        .bind(auth_kind)
        .bind(config_json)
        .bind(input.oauth_session_id.map(|id| id.to_string()))
        .bind(now)
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

        Ok(UpstreamAccountView {
            id: account_id,
            tenant_id: parse_uuid(tenant_id)?,
            tenant_external_id: Some(input.tenant_external_id.clone()),
            name: input.name.trim().to_owned(),
            driver: input.driver.clone(),
            auth_kind: auth_kind.to_owned(),
            connection_method: upstream_connection_method(&input.driver, auth_kind),
            credential_generation: 1,
            status: "active".to_owned(),
            config: input.config,
            credential_expires_at,
            route_count: 0,
            created_at: now,
            updated_at: now,
        })
    }

    pub async fn rotate_upstream_credential(
        &self,
        account_id: Uuid,
        credential: UpstreamCredential,
        idempotency_key: &str,
        key_material: &[u8],
    ) -> Result<UpstreamAccountView, AppError> {
        validate_idempotency_key(idempotency_key, "Idempotency-Key")?;
        let idempotency_key = idempotency_key.trim();
        let now = unix_millis();
        let request_hash = upstream_credential_rotation_request_hash(account_id, &credential)?;
        let expires_at = now.saturating_add(CREDENTIAL_ROTATION_REPLAY_TTL_MILLIS);
        let mut tx = self.pool.begin().await?;
        if let Some(replay) = claim_credential_rotation(
            &mut tx,
            UPSTREAM_CREDENTIAL_ROTATION_RESOURCE,
            account_id,
            idempotency_key,
            &request_hash,
            now,
            expires_at,
        )
        .await?
        {
            let view = open_rotation_replay(
                replay,
                UPSTREAM_CREDENTIAL_ROTATION_RESOURCE,
                account_id,
                idempotency_key,
                &request_hash,
                key_material,
                now,
            )?;
            tx.commit().await?;
            return Ok(view);
        }
        let view = self
            .rotate_upstream_credential_claimed(
                &mut tx,
                account_id,
                credential,
                idempotency_key,
                UPSTREAM_CREDENTIAL_ROTATION_RESOURCE,
                &request_hash,
                expires_at,
                key_material,
            )
            .await?;
        tx.commit().await?;
        Ok(view)
    }

    /// Claim an OAuth refresh before contacting the authorization server. A
    /// retry with the same key returns the stored result, while a concurrent
    /// retry observes the in-progress tombstone and cannot refresh twice.
    pub async fn begin_upstream_oauth_refresh(
        &self,
        account_id: Uuid,
        idempotency_key: &str,
        key_material: &[u8],
    ) -> Result<Option<UpstreamAccountView>, AppError> {
        validate_idempotency_key(idempotency_key, "Idempotency-Key")?;
        let idempotency_key = idempotency_key.trim();
        let now = unix_millis();
        let request_hash =
            credential_rotation_request_hash(UPSTREAM_OAUTH_REFRESH_RESOURCE, account_id);
        let expires_at = now.saturating_add(CREDENTIAL_ROTATION_REPLAY_TTL_MILLIS);
        let mut tx = self.pool.begin().await?;
        let replay = claim_credential_rotation(
            &mut tx,
            UPSTREAM_OAUTH_REFRESH_RESOURCE,
            account_id,
            idempotency_key,
            &request_hash,
            now,
            expires_at,
        )
        .await?;
        let replay = replay
            .map(|replay| {
                open_rotation_replay(
                    replay,
                    UPSTREAM_OAUTH_REFRESH_RESOURCE,
                    account_id,
                    idempotency_key,
                    &request_hash,
                    key_material,
                    now,
                )
            })
            .transpose()?;
        tx.commit().await?;
        Ok(replay)
    }

    pub async fn finish_upstream_oauth_refresh(
        &self,
        account_id: Uuid,
        credential: UpstreamCredential,
        idempotency_key: &str,
        key_material: &[u8],
    ) -> Result<UpstreamAccountView, AppError> {
        validate_idempotency_key(idempotency_key, "Idempotency-Key")?;
        let idempotency_key = idempotency_key.trim();
        let request_hash =
            credential_rotation_request_hash(UPSTREAM_OAUTH_REFRESH_RESOURCE, account_id);
        let mut tx = self.pool.begin().await?;
        let replay_row = sqlx::query(
            "SELECT resource_kind, resource_id, request_hash, expires_at FROM credential_rotation_replays WHERE idempotency_key = $1",
        )
        .bind(idempotency_key)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| AppError::BadRequest("OAuth refresh claim is missing; start the refresh again with a new Idempotency-Key".into()))?;
        if replay_row.try_get::<String, _>("resource_kind")? != UPSTREAM_OAUTH_REFRESH_RESOURCE
            || replay_row.try_get::<String, _>("resource_id")? != account_id.to_string()
            || replay_row.try_get::<String, _>("request_hash")? != request_hash
        {
            return Err(AppError::BadRequest(
                "Idempotency-Key was already used for a different credential rotation".into(),
            ));
        }
        let expires_at: i64 = replay_row.try_get("expires_at")?;
        let view = self
            .rotate_upstream_credential_claimed(
                &mut tx,
                account_id,
                credential,
                idempotency_key,
                UPSTREAM_OAUTH_REFRESH_RESOURCE,
                &request_hash,
                expires_at,
                key_material,
            )
            .await?;
        tx.commit().await?;
        Ok(view)
    }

    #[allow(clippy::too_many_arguments)]
    async fn rotate_upstream_credential_claimed(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Any>,
        account_id: Uuid,
        credential: UpstreamCredential,
        idempotency_key: &str,
        resource_kind: &str,
        request_hash: &str,
        replay_expires_at: i64,
        key_material: &[u8],
    ) -> Result<UpstreamAccountView, AppError> {
        let now = unix_millis();
        let ciphertext = seal_credential(&credential, key_material)?;
        let select = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT a.tenant_id, t.external_id AS tenant_external_id, a.name, a.driver, a.auth_kind, a.config_json, a.status, a.credential_generation, a.created_at, a.updated_at, (SELECT COUNT(*) FROM model_routes r WHERE r.upstream_account_id = a.id) AS route_count FROM upstream_accounts a JOIN tenants t ON t.id = a.tenant_id WHERE a.id = $1 FOR UPDATE OF a"
            }
            DatabaseBackend::Sqlite => {
                "SELECT a.tenant_id, t.external_id AS tenant_external_id, a.name, a.driver, a.auth_kind, a.config_json, a.status, a.credential_generation, a.created_at, a.updated_at, (SELECT COUNT(*) FROM model_routes r WHERE r.upstream_account_id = a.id) AS route_count FROM upstream_accounts a JOIN tenants t ON t.id = a.tenant_id WHERE a.id = $1"
            }
        };
        let row = sqlx::query(select)
            .bind(account_id.to_string())
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(AppError::NotFound)?;
        let status: String = row.try_get("status")?;
        if !matches!(status.as_str(), "active" | "disabled") {
            return Err(AppError::Forbidden);
        }
        let auth_kind: String = row.try_get("auth_kind")?;
        if auth_kind != credential.auth_kind() {
            return Err(AppError::BadRequest(
                "credential rotation cannot change auth type; create a new upstream account".into(),
            ));
        }
        let generation: i64 = row.try_get::<i64, _>("credential_generation")? + 1;
        let updated_at = now.max(row.try_get::<i64, _>("updated_at")?.saturating_add(1));
        sqlx::query(
            "UPDATE upstream_credentials SET revoked_at = $1 WHERE upstream_account_id = $2 AND revoked_at IS NULL",
        )
        .bind(now)
        .bind(account_id.to_string())
        .execute(&mut **tx)
        .await?;
        sqlx::query(
            "INSERT INTO upstream_credentials (id, upstream_account_id, generation, credential_ciphertext, expires_at, created_at) VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(account_id.to_string())
        .bind(generation)
        .bind(ciphertext)
        .bind(credential.expires_at())
        .bind(now)
        .execute(&mut **tx)
        .await?;
        sqlx::query(
            "UPDATE upstream_accounts SET credential_generation = $1, updated_at = $2 WHERE id = $3",
        )
        .bind(generation)
        .bind(updated_at)
        .bind(account_id.to_string())
        .execute(&mut **tx)
        .await?;

        let config_json: String = row.try_get("config_json")?;
        let view = UpstreamAccountView {
            id: account_id,
            tenant_id: parse_uuid(row.try_get("tenant_id")?)?,
            tenant_external_id: Some(row.try_get("tenant_external_id")?),
            name: row.try_get("name")?,
            driver: row.try_get("driver")?,
            auth_kind,
            connection_method: upstream_connection_method(
                row.try_get::<String, _>("driver")?.as_str(),
                credential.auth_kind(),
            ),
            credential_generation: generation,
            status,
            config: serde_json::from_str(&config_json).map_err(|_| AppError::Internal)?,
            credential_expires_at: credential.expires_at(),
            route_count: row.try_get("route_count")?,
            created_at: row.try_get("created_at")?,
            updated_at,
        };
        store_credential_rotation_response(
            tx,
            idempotency_key,
            &view,
            resource_kind,
            account_id,
            request_hash,
            replay_expires_at,
            key_material,
        )
        .await?;
        Ok(view)
    }

    pub async fn upstream_account_with_credential(
        &self,
        account_id: Uuid,
        key_material: &[u8],
    ) -> Result<(UpstreamAccountView, UpstreamCredential), AppError> {
        let row = sqlx::query(
            "SELECT a.id, a.tenant_id, t.external_id AS tenant_external_id, a.name, a.driver, a.auth_kind, a.config_json, a.status, a.credential_generation, a.created_at, a.updated_at, c.expires_at, c.credential_ciphertext, (SELECT COUNT(*) FROM model_routes r WHERE r.upstream_account_id = a.id) AS route_count FROM upstream_accounts a JOIN tenants t ON t.id = a.tenant_id JOIN upstream_credentials c ON c.upstream_account_id = a.id AND c.generation = a.credential_generation AND c.revoked_at IS NULL WHERE a.id = $1",
        )
        .bind(account_id.to_string())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::NotFound)?;
        let ciphertext: String = row.try_get("credential_ciphertext")?;
        let credential = open_credential(&ciphertext, key_material)?;
        Ok((upstream_account_view(row)?, credential))
    }

    pub async fn update_upstream_account(
        &self,
        account_id: Uuid,
        tenant_external_id: &str,
        input: UpdateUpstreamAccountInput,
    ) -> Result<UpstreamAccountView, AppError> {
        validate_upstream_account_name(&input.name)?;
        let config_json = serde_json::to_string(&input.config).map_err(|_| AppError::Internal)?;
        let mut tx = self.pool.begin().await?;
        let current = sqlx::query(
            "SELECT a.id, a.tenant_id, t.external_id AS tenant_external_id, a.name, a.driver, a.auth_kind, a.config_json, a.status, a.credential_generation, a.created_at, a.updated_at, c.expires_at, (SELECT COUNT(*) FROM model_routes r WHERE r.upstream_account_id = a.id) AS route_count FROM upstream_accounts a JOIN tenants t ON t.id = a.tenant_id LEFT JOIN upstream_credentials c ON c.upstream_account_id = a.id AND c.generation = a.credential_generation AND c.revoked_at IS NULL WHERE a.id = $1 AND t.external_id = $2",
        )
        .bind(account_id.to_string())
        .bind(tenant_external_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AppError::NotFound)?;
        let current_view = upstream_account_view(current)?;
        if current_view.name == input.name.trim() && current_view.config == input.config {
            tx.commit().await?;
            return Ok(current_view);
        }
        if current_view.updated_at != input.expected_updated_at {
            return Err(AppError::Conflict(
                "reload the upstream provider before saving it again".into(),
            ));
        }
        let duplicate = sqlx::query(
            "SELECT id FROM upstream_accounts WHERE tenant_id = $1 AND name = $2 AND id <> $3",
        )
        .bind(current_view.tenant_id.to_string())
        .bind(input.name.trim())
        .bind(account_id.to_string())
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
        if duplicate {
            return Err(AppError::Conflict(
                "another upstream provider already uses this name".into(),
            ));
        }
        let updated_at = unix_millis().max(current_view.updated_at.saturating_add(1));
        let changed = sqlx::query(
            "UPDATE upstream_accounts SET name = $1, config_json = $2, updated_at = $3 WHERE id = $4 AND tenant_id = $5 AND updated_at = $6",
        )
        .bind(input.name.trim())
        .bind(config_json)
        .bind(updated_at)
        .bind(account_id.to_string())
        .bind(current_view.tenant_id.to_string())
        .bind(input.expected_updated_at)
        .execute(&mut *tx)
        .await?;
        if changed.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "reload the upstream provider before saving it again".into(),
            ));
        }
        tx.commit().await?;
        Ok(UpstreamAccountView {
            name: input.name.trim().to_owned(),
            config: input.config,
            updated_at,
            ..current_view
        })
    }

    pub async fn set_upstream_account_status(
        &self,
        account_id: Uuid,
        tenant_external_id: &str,
        status: &str,
        expected_updated_at: i64,
    ) -> Result<UpstreamAccountView, AppError> {
        if !matches!(status, "active" | "disabled") {
            return Err(AppError::BadRequest(
                "upstream provider status must be active or disabled".into(),
            ));
        }
        let mut tx = self.pool.begin().await?;
        let current = sqlx::query(
            "SELECT a.id, a.tenant_id, t.external_id AS tenant_external_id, a.name, a.driver, a.auth_kind, a.config_json, a.status, a.credential_generation, a.created_at, a.updated_at, c.expires_at, (SELECT COUNT(*) FROM model_routes r WHERE r.upstream_account_id = a.id) AS route_count FROM upstream_accounts a JOIN tenants t ON t.id = a.tenant_id LEFT JOIN upstream_credentials c ON c.upstream_account_id = a.id AND c.generation = a.credential_generation AND c.revoked_at IS NULL WHERE a.id = $1 AND t.external_id = $2",
        )
        .bind(account_id.to_string())
        .bind(tenant_external_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AppError::NotFound)?;
        let mut view = upstream_account_view(current)?;
        if view.status == status {
            tx.commit().await?;
            return Ok(view);
        }
        if view.updated_at != expected_updated_at {
            return Err(AppError::Conflict(
                "reload the upstream provider before changing its status".into(),
            ));
        }
        let updated_at = unix_millis().max(view.updated_at.saturating_add(1));
        let changed = sqlx::query(
            "UPDATE upstream_accounts SET status = $1, updated_at = $2 WHERE id = $3 AND tenant_id = $4 AND updated_at = $5",
        )
        .bind(status)
        .bind(updated_at)
        .bind(account_id.to_string())
        .bind(view.tenant_id.to_string())
        .bind(expected_updated_at)
        .execute(&mut *tx)
        .await?;
        if changed.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "reload the upstream provider before changing its status".into(),
            ));
        }
        tx.commit().await?;
        view.status = status.to_owned();
        view.updated_at = updated_at;
        Ok(view)
    }

    pub async fn delete_upstream_account(
        &self,
        account_id: Uuid,
        tenant_external_id: &str,
        expected_updated_at: i64,
    ) -> Result<(), AppError> {
        let mut tx = self.pool.begin().await?;
        let account = sqlx::query(
            "SELECT a.tenant_id, a.status, a.updated_at FROM upstream_accounts a JOIN tenants t ON t.id = a.tenant_id WHERE a.id = $1 AND t.external_id = $2",
        )
        .bind(account_id.to_string())
        .bind(tenant_external_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(account) = account else {
            tx.commit().await?;
            return Ok(());
        };
        if account.try_get::<String, _>("status")? != "disabled" {
            return Err(AppError::Conflict(
                "disable the upstream provider before deleting it".into(),
            ));
        }
        if account.try_get::<i64, _>("updated_at")? != expected_updated_at {
            return Err(AppError::Conflict(
                "reload the upstream provider before deleting it".into(),
            ));
        }
        let has_routes =
            sqlx::query("SELECT id FROM model_routes WHERE upstream_account_id = $1 LIMIT 1")
                .bind(account_id.to_string())
                .fetch_optional(&mut *tx)
                .await?
                .is_some();
        if has_routes {
            return Err(AppError::Conflict(
                "the upstream provider still has model routes and must be retained".into(),
            ));
        }
        let has_request_history =
            sqlx::query("SELECT id FROM request_records WHERE upstream_account_id = $1 LIMIT 1")
                .bind(account_id.to_string())
                .fetch_optional(&mut *tx)
                .await?
                .is_some();
        let has_generation_history =
            sqlx::query("SELECT id FROM generation_jobs WHERE upstream_account_id = $1 LIMIT 1")
                .bind(account_id.to_string())
                .fetch_optional(&mut *tx)
                .await?
                .is_some();
        if has_request_history || has_generation_history {
            return Err(AppError::Conflict(
                "the upstream provider has request history and must be retained for audit".into(),
            ));
        }
        sqlx::query("DELETE FROM upstream_credentials WHERE upstream_account_id = $1")
            .bind(account_id.to_string())
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "DELETE FROM credential_rotation_replays WHERE resource_id = $1 AND resource_kind IN ($2, $3)",
        )
        .bind(account_id.to_string())
        .bind(UPSTREAM_CREDENTIAL_ROTATION_RESOURCE)
        .bind(UPSTREAM_OAUTH_REFRESH_RESOURCE)
        .execute(&mut *tx)
        .await?;
        let deleted = sqlx::query(
            "DELETE FROM upstream_accounts WHERE id = $1 AND tenant_id = $2 AND status = 'disabled' AND updated_at = $3",
        )
        .bind(account_id.to_string())
        .bind(account.try_get::<String, _>("tenant_id")?)
        .bind(expected_updated_at)
        .execute(&mut *tx)
        .await?;
        if deleted.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "reload the upstream provider before deleting it".into(),
            ));
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn create_model_route(
        &self,
        input: CreateModelRouteInput,
    ) -> Result<ModelRouteView, AppError> {
        validate_model_route_fields(
            &input.public_model,
            &input.upstream_model,
            &input.protocol,
            input.priority,
        )?;
        let now = unix_millis();
        let route_id = Uuid::now_v7();
        let mut tx = self.pool.begin().await?;
        let tenant_id: String = sqlx::query("SELECT id FROM tenants WHERE external_id = $1")
            .bind(&input.tenant_external_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AppError::NotFound)?
            .try_get("id")?;
        let account_tenant: String = sqlx::query(
            "SELECT tenant_id FROM upstream_accounts WHERE id = $1 AND status = 'active'",
        )
        .bind(input.upstream_account_id.to_string())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AppError::NotFound)?
        .try_get("tenant_id")?;
        if account_tenant != tenant_id {
            return Err(AppError::Forbidden);
        }
        let inserted = sqlx::query(
            "INSERT INTO model_routes (id, tenant_id, public_model, upstream_account_id, upstream_model, protocol, priority, enabled, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, $6, $7, 1, $8, $9) ON CONFLICT(tenant_id, public_model, protocol, priority) DO NOTHING",
        )
        .bind(route_id.to_string())
        .bind(&tenant_id)
        .bind(input.public_model.trim())
        .bind(input.upstream_account_id.to_string())
        .bind(input.upstream_model.trim())
        .bind(&input.protocol)
        .bind(input.priority)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        if inserted.rows_affected() == 0 {
            let existing = sqlx::query(
                "SELECT r.id, r.tenant_id, t.external_id AS tenant_external_id, r.public_model, r.upstream_account_id, r.upstream_model, r.protocol, r.priority, r.enabled, r.created_at, r.updated_at FROM model_routes r JOIN tenants t ON t.id = r.tenant_id WHERE r.tenant_id = $1 AND r.public_model = $2 AND r.protocol = $3 AND r.priority = $4",
            )
            .bind(&tenant_id)
            .bind(input.public_model.trim())
            .bind(&input.protocol)
            .bind(input.priority)
            .fetch_one(&mut *tx)
            .await?;
            let existing = model_route_view(existing)?;
            if existing.upstream_account_id == input.upstream_account_id
                && existing.upstream_model == input.upstream_model.trim()
            {
                tx.commit().await?;
                return Ok(existing);
            }
            return Err(AppError::Conflict(
                "another route already uses this public model, protocol, and priority".into(),
            ));
        }
        tx.commit().await?;
        Ok(ModelRouteView {
            id: route_id,
            tenant_id: parse_uuid(tenant_id)?,
            tenant_external_id: Some(input.tenant_external_id),
            public_model: input.public_model.trim().to_owned(),
            upstream_account_id: input.upstream_account_id,
            upstream_model: input.upstream_model.trim().to_owned(),
            protocol: input.protocol,
            priority: input.priority,
            enabled: true,
            created_at: now,
            updated_at: now,
        })
    }

    pub async fn update_model_route(
        &self,
        route_id: Uuid,
        tenant_external_id: &str,
        input: UpdateModelRouteInput,
    ) -> Result<ModelRouteView, AppError> {
        validate_model_route_fields(
            &input.public_model,
            &input.upstream_model,
            &input.protocol,
            input.priority,
        )?;
        let public_model = input.public_model.trim();
        let upstream_model = input.upstream_model.trim();
        let mut tx = self.pool.begin().await?;
        let current = sqlx::query(
            "SELECT r.id, r.tenant_id, t.external_id AS tenant_external_id, r.public_model, r.upstream_account_id, r.upstream_model, r.protocol, r.priority, r.enabled, r.created_at, r.updated_at FROM model_routes r JOIN tenants t ON t.id = r.tenant_id WHERE r.id = $1 AND t.external_id = $2",
        )
        .bind(route_id.to_string())
        .bind(tenant_external_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AppError::NotFound)?;
        let current_view = model_route_view(current)?;
        let unchanged = current_view.public_model == public_model
            && current_view.upstream_account_id == input.upstream_account_id
            && current_view.upstream_model == upstream_model
            && current_view.protocol == input.protocol
            && current_view.priority == input.priority;
        if unchanged {
            tx.commit().await?;
            return Ok(current_view);
        }
        if current_view.updated_at != input.expected_updated_at {
            return Err(AppError::Conflict(
                "reload the model route before saving it again".into(),
            ));
        }
        let account_tenant = sqlx::query(
            "SELECT tenant_id FROM upstream_accounts WHERE id = $1 AND status = 'active'",
        )
        .bind(input.upstream_account_id.to_string())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AppError::NotFound)?
        .try_get::<String, _>("tenant_id")?;
        if account_tenant != current_view.tenant_id.to_string() {
            return Err(AppError::Forbidden);
        }
        let duplicate = sqlx::query(
            "SELECT id FROM model_routes WHERE tenant_id = $1 AND public_model = $2 AND protocol = $3 AND priority = $4 AND id <> $5",
        )
        .bind(current_view.tenant_id.to_string())
        .bind(public_model)
        .bind(&input.protocol)
        .bind(input.priority)
        .bind(route_id.to_string())
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
        if duplicate {
            return Err(AppError::Conflict(
                "another route already uses this public model, protocol, and priority".into(),
            ));
        }
        let updated_at = unix_millis().max(current_view.updated_at.saturating_add(1));
        let changed = sqlx::query(
            "UPDATE model_routes SET public_model = $1, upstream_account_id = $2, upstream_model = $3, protocol = $4, priority = $5, updated_at = $6 WHERE id = $7 AND tenant_id = $8 AND updated_at = $9",
        )
        .bind(public_model)
        .bind(input.upstream_account_id.to_string())
        .bind(upstream_model)
        .bind(&input.protocol)
        .bind(input.priority)
        .bind(updated_at)
        .bind(route_id.to_string())
        .bind(current_view.tenant_id.to_string())
        .bind(input.expected_updated_at)
        .execute(&mut *tx)
        .await?;
        if changed.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "reload the model route before saving it again".into(),
            ));
        }
        tx.commit().await?;
        Ok(ModelRouteView {
            id: route_id,
            tenant_id: current_view.tenant_id,
            tenant_external_id: current_view.tenant_external_id,
            public_model: public_model.to_owned(),
            upstream_account_id: input.upstream_account_id,
            upstream_model: upstream_model.to_owned(),
            protocol: input.protocol,
            priority: input.priority,
            enabled: current_view.enabled,
            created_at: current_view.created_at,
            updated_at,
        })
    }

    pub async fn set_model_route_enabled(
        &self,
        route_id: Uuid,
        tenant_external_id: &str,
        enabled: bool,
        expected_updated_at: i64,
    ) -> Result<ModelRouteView, AppError> {
        let mut tx = self.pool.begin().await?;
        let current = sqlx::query(
            "SELECT r.id, r.tenant_id, t.external_id AS tenant_external_id, r.public_model, r.upstream_account_id, r.upstream_model, r.protocol, r.priority, r.enabled, r.created_at, r.updated_at FROM model_routes r JOIN tenants t ON t.id = r.tenant_id WHERE r.id = $1 AND t.external_id = $2",
        )
        .bind(route_id.to_string())
        .bind(tenant_external_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AppError::NotFound)?;
        let mut route = model_route_view(current)?;
        if route.enabled == enabled {
            tx.commit().await?;
            return Ok(route);
        }
        if route.updated_at != expected_updated_at {
            return Err(AppError::Conflict(
                "reload the model route before changing its status".into(),
            ));
        }
        let updated_at = unix_millis().max(route.updated_at.saturating_add(1));
        let changed = sqlx::query(
            "UPDATE model_routes SET enabled = $1, updated_at = $2 WHERE id = $3 AND tenant_id = $4 AND updated_at = $5",
        )
        .bind(i64::from(enabled))
        .bind(updated_at)
        .bind(route_id.to_string())
        .bind(route.tenant_id.to_string())
        .bind(expected_updated_at)
        .execute(&mut *tx)
        .await?;
        if changed.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "reload the model route before changing its status".into(),
            ));
        }
        tx.commit().await?;
        route.enabled = enabled;
        route.updated_at = updated_at;
        Ok(route)
    }

    pub async fn delete_model_route(
        &self,
        route_id: Uuid,
        tenant_external_id: &str,
        expected_updated_at: i64,
    ) -> Result<(), AppError> {
        let mut tx = self.pool.begin().await?;
        let route = sqlx::query(
            "SELECT r.tenant_id, r.enabled, r.updated_at FROM model_routes r JOIN tenants t ON t.id = r.tenant_id WHERE r.id = $1 AND t.external_id = $2",
        )
        .bind(route_id.to_string())
        .bind(tenant_external_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(route) = route else {
            tx.commit().await?;
            return Ok(());
        };
        let enabled = route.try_get::<i64, _>("enabled")? != 0;
        let updated_at: i64 = route.try_get("updated_at")?;
        if enabled {
            return Err(AppError::Conflict(
                "disable the model route before deleting it".into(),
            ));
        }
        if updated_at != expected_updated_at {
            return Err(AppError::Conflict(
                "reload the model route before deleting it".into(),
            ));
        }
        let referenced =
            sqlx::query("SELECT id FROM request_records WHERE model_route_id = $1 LIMIT 1")
                .bind(route_id.to_string())
                .fetch_optional(&mut *tx)
                .await?
                .is_some();
        if referenced {
            return Err(AppError::Conflict(
                "the route has request history and must be retained in a disabled state".into(),
            ));
        }
        let changed = sqlx::query(
            "DELETE FROM model_routes WHERE id = $1 AND tenant_id = $2 AND enabled = 0 AND updated_at = $3",
        )
        .bind(route_id.to_string())
        .bind(route.try_get::<String, _>("tenant_id")?)
        .bind(expected_updated_at)
        .execute(&mut *tx)
        .await?;
        if changed.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "reload the model route before deleting it".into(),
            ));
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn list_upstream_accounts(
        &self,
        tenant_external_id: &str,
    ) -> Result<Vec<UpstreamAccountView>, AppError> {
        let rows = sqlx::query(
            "SELECT a.id, a.tenant_id, t.external_id AS tenant_external_id, a.name, a.driver, a.auth_kind, a.config_json, a.status, a.credential_generation, a.created_at, a.updated_at, c.expires_at, (SELECT COUNT(*) FROM model_routes r WHERE r.upstream_account_id = a.id) AS route_count FROM upstream_accounts a JOIN tenants t ON t.id = a.tenant_id LEFT JOIN upstream_credentials c ON c.upstream_account_id = a.id AND c.generation = a.credential_generation AND c.revoked_at IS NULL WHERE t.external_id = $1 ORDER BY a.created_at DESC, a.id DESC",
        )
        .bind(tenant_external_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(upstream_account_view).collect()
    }

    /// Lists every upstream account for a global operator. Tenant-scoped
    /// operators must use `list_upstream_accounts` so the authorization scope
    /// remains visible at the call site.
    pub async fn list_all_upstream_accounts(&self) -> Result<Vec<UpstreamAccountView>, AppError> {
        let rows = sqlx::query(
            "SELECT a.id, a.tenant_id, t.external_id AS tenant_external_id, a.name, a.driver, a.auth_kind, a.config_json, a.status, a.credential_generation, a.created_at, a.updated_at, c.expires_at, (SELECT COUNT(*) FROM model_routes r WHERE r.upstream_account_id = a.id) AS route_count FROM upstream_accounts a JOIN tenants t ON t.id = a.tenant_id LEFT JOIN upstream_credentials c ON c.upstream_account_id = a.id AND c.generation = a.credential_generation AND c.revoked_at IS NULL ORDER BY a.created_at DESC, a.id DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(upstream_account_view).collect()
    }

    pub async fn resolve_upstream(
        &self,
        tenant_id: Uuid,
        public_model: &str,
        protocol: &str,
        key_material: &[u8],
    ) -> Result<Option<ResolvedUpstream>, AppError> {
        self.resolve_upstream_with_hint(tenant_id, public_model, protocol, None, key_material)
            .await
    }

    pub async fn resolve_upstream_with_hint(
        &self,
        tenant_id: Uuid,
        public_model: &str,
        protocol: &str,
        upstream_account_id: Option<Uuid>,
        key_material: &[u8],
    ) -> Result<Option<ResolvedUpstream>, AppError> {
        let sql = if upstream_account_id.is_some() {
            "SELECT r.id AS route_id, r.upstream_model, a.id AS account_id, a.driver, a.config_json, c.credential_ciphertext FROM model_routes r JOIN upstream_accounts a ON a.id = r.upstream_account_id JOIN upstream_credentials c ON c.upstream_account_id = a.id AND c.generation = a.credential_generation AND c.revoked_at IS NULL WHERE r.tenant_id = $1 AND r.public_model = $2 AND r.protocol = $3 AND a.id = $4 AND r.enabled = 1 AND a.status = 'active' ORDER BY r.priority ASC, r.id ASC LIMIT 1"
        } else {
            "SELECT r.id AS route_id, r.upstream_model, a.id AS account_id, a.driver, a.config_json, c.credential_ciphertext FROM model_routes r JOIN upstream_accounts a ON a.id = r.upstream_account_id JOIN upstream_credentials c ON c.upstream_account_id = a.id AND c.generation = a.credential_generation AND c.revoked_at IS NULL WHERE r.tenant_id = $1 AND r.public_model = $2 AND r.protocol = $3 AND r.enabled = 1 AND a.status = 'active' ORDER BY r.priority ASC, r.id ASC LIMIT 1"
        };
        let query = sqlx::query(sql)
            .bind(tenant_id.to_string())
            .bind(public_model)
            .bind(protocol);
        let query = if let Some(account_id) = upstream_account_id {
            query.bind(account_id.to_string())
        } else {
            query
        };
        let row = query.fetch_optional(&self.pool).await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let config_json: String = row.try_get("config_json")?;
        let config: serde_json::Value =
            serde_json::from_str(&config_json).map_err(|_| AppError::Internal)?;
        let base_url = validate_config(&config)?;
        let ciphertext: String = row.try_get("credential_ciphertext")?;
        Ok(Some(ResolvedUpstream {
            route_id: parse_uuid(row.try_get("route_id")?)?,
            account_id: parse_uuid(row.try_get("account_id")?)?,
            driver: row.try_get("driver")?,
            base_url,
            config,
            upstream_model: row.try_get("upstream_model")?,
            credential: open_credential(&ciphertext, key_material)?,
        }))
    }

    pub async fn authenticate_key(
        &self,
        value: &str,
        pepper: &[u8],
    ) -> Result<AuthenticatedKey, AppError> {
        let row = if let Some(parsed) = crypto::parse_credential(value) {
            sqlx::query(
                "SELECT k.id AS key_id, k.tenant_id, k.principal_id, k.account_id, k.alias, k.currency, k.policy_json, k.status, c.generation, c.secret_hash FROM key_records k JOIN key_credentials c ON c.key_id = k.id AND c.generation = k.credential_generation WHERE k.id = $1 AND c.revoked_at IS NULL",
            )
            .bind(parsed.key_id.to_string())
            .fetch_optional(&self.pool)
            .await?
        } else {
            if value.len() < 16 || value.len() > 512 || value.contains(['\r', '\n']) {
                return Err(AppError::Unauthorized);
            }
            let (secret_hash, _) = crypto::hash_credential(value, pepper);
            sqlx::query(
                "SELECT k.id AS key_id, k.tenant_id, k.principal_id, k.account_id, k.alias, k.currency, k.policy_json, k.status, c.generation, c.secret_hash FROM key_records k JOIN legacy_key_credentials c ON c.key_id = k.id AND c.generation = k.credential_generation WHERE c.secret_hash = $1 AND c.revoked_at IS NULL",
            )
            .bind(secret_hash)
            .fetch_optional(&self.pool)
            .await?
        }
        .ok_or(AppError::Unauthorized)?;
        let status: String = row.try_get("status")?;
        let expected: Vec<u8> = row.try_get("secret_hash")?;
        if status != "active" || !crypto::verify_credential(value, pepper, &expected) {
            return Err(AppError::Unauthorized);
        }

        let policy_json: String = row.try_get("policy_json")?;
        Ok(AuthenticatedKey {
            key_id: parse_uuid(row.try_get("key_id")?)?,
            tenant_id: parse_uuid(row.try_get("tenant_id")?)?,
            principal_id: parse_uuid(row.try_get("principal_id")?)?,
            account_id: parse_uuid(row.try_get("account_id")?)?,
            alias: row.try_get("alias")?,
            currency: row.try_get("currency")?,
            credential_generation: row.try_get("generation")?,
            policy: serde_json::from_str(&policy_json).map_err(|_| AppError::Internal)?,
        })
    }

    pub async fn register_legacy_key_credential(
        &self,
        key_id: Uuid,
        credential: &str,
        source_hash: &str,
        pepper: &[u8],
    ) -> Result<LegacyCredentialView, AppError> {
        let source_hash = source_hash.trim().to_ascii_lowercase();
        if credential.len() < 16
            || credential.len() > 512
            || credential.contains(['\r', '\n'])
            || source_hash.len() != 64
            || !source_hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(AppError::BadRequest(
                "legacy credential or source_hash is invalid".into(),
            ));
        }
        let actual_source_hash = format!("{:x}", Sha256::digest(credential.trim().as_bytes()));
        if actual_source_hash != source_hash {
            return Err(AppError::BadRequest(
                "legacy credential does not match source_hash".into(),
            ));
        }
        let mut transaction = self.pool.begin().await?;
        if matches!(self.backend, DatabaseBackend::PostgreSql) {
            sqlx::query("SELECT id FROM key_records WHERE id = $1 FOR UPDATE")
                .bind(key_id.to_string())
                .fetch_optional(&mut *transaction)
                .await?
                .ok_or(AppError::NotFound)?;
        }
        let existing = sqlx::query(
            "SELECT key_id, generation, fingerprint FROM legacy_key_credentials WHERE source_hash = $1 AND revoked_at IS NULL",
        )
        .bind(&source_hash)
        .fetch_optional(&mut *transaction)
        .await?;
        if let Some(row) = existing {
            let existing_key_id = parse_uuid(row.try_get("key_id")?)?;
            if existing_key_id != key_id {
                return Err(AppError::Forbidden);
            }
            transaction.commit().await?;
            return Ok(LegacyCredentialView {
                key_id,
                generation: row.try_get("generation")?,
                fingerprint: row.try_get("fingerprint")?,
                source_hash,
            });
        }
        let generation = sqlx::query(
            "SELECT credential_generation FROM key_records WHERE id = $1 AND status = 'active'",
        )
        .bind(key_id.to_string())
        .fetch_optional(&mut *transaction)
        .await?
        .ok_or(AppError::NotFound)?
        .try_get("credential_generation")?;
        let (secret_hash, fingerprint) = crypto::hash_credential(credential.trim(), pepper);
        sqlx::query(
            "INSERT INTO legacy_key_credentials (id, key_id, generation, secret_hash, fingerprint, source_hash, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(key_id.to_string())
        .bind(generation)
        .bind(secret_hash)
        .bind(&fingerprint)
        .bind(&source_hash)
        .bind(unix_millis())
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(LegacyCredentialView {
            key_id,
            generation,
            fingerprint,
            source_hash,
        })
    }

    pub async fn key_view(&self, key: &AuthenticatedKey) -> Result<KeyView, AppError> {
        let row = sqlx::query(
            "SELECT k.created_at, a.available_micros FROM key_records k JOIN credit_accounts a ON a.id = k.account_id WHERE k.id = $1",
        )
        .bind(key.key_id.to_string())
        .fetch_one(&self.pool)
        .await?;
        Ok(KeyView {
            key_id: key.key_id,
            alias: key.alias.clone(),
            currency: key.currency.clone(),
            credential_generation: key.credential_generation,
            created_at: row.try_get("created_at")?,
            policy: key.policy.clone(),
            available_balance: micros_to_decimal_string(row.try_get("available_micros")?),
        })
    }

    pub async fn require_key_tenant(
        &self,
        key_id: Uuid,
        tenant_external_id: &str,
    ) -> Result<(), AppError> {
        let exists = sqlx::query(
            "SELECT k.id FROM key_records k JOIN tenants t ON t.id = k.tenant_id WHERE k.id = $1 AND t.external_id = $2",
        )
        .bind(key_id.to_string())
        .bind(tenant_external_id)
        .fetch_optional(&self.pool)
        .await?
        .is_some();
        exists.then_some(()).ok_or(AppError::Forbidden)
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

    pub async fn require_upstream_tenant(
        &self,
        account_id: Uuid,
        tenant_external_id: &str,
    ) -> Result<(), AppError> {
        let exists = sqlx::query(
            "SELECT a.id FROM upstream_accounts a JOIN tenants t ON t.id = a.tenant_id WHERE a.id = $1 AND t.external_id = $2",
        )
        .bind(account_id.to_string())
        .bind(tenant_external_id)
        .fetch_optional(&self.pool)
        .await?
        .is_some();
        exists.then_some(()).ok_or(AppError::Forbidden)
    }

    pub async fn upsert_model_price(
        &self,
        model: &str,
        currency: &str,
        input_per_million: Decimal,
        output_per_million: Decimal,
    ) -> Result<ModelPrice, AppError> {
        self.upsert_model_price_tier(
            model,
            currency,
            "default",
            input_per_million,
            input_per_million,
            input_per_million,
            output_per_million,
            true,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_model_price_tier(
        &self,
        model: &str,
        currency: &str,
        service_tier: &str,
        input_per_million: Decimal,
        cached_input_per_million: Decimal,
        cache_write_per_million: Decimal,
        output_per_million: Decimal,
        cache_price_estimated: bool,
    ) -> Result<ModelPrice, AppError> {
        validate_currency(currency)?;
        validate_service_tier(service_tier)?;
        let input_micros = decimal_to_micros(input_per_million)?;
        let cached_input_micros = decimal_to_micros(cached_input_per_million)?;
        let cache_write_micros = decimal_to_micros(cache_write_per_million)?;
        let output_micros = decimal_to_micros(output_per_million)?;
        if [
            input_micros,
            cached_input_micros,
            cache_write_micros,
            output_micros,
        ]
        .into_iter()
        .any(|price| price < 0)
        {
            return Err(AppError::BadRequest(
                "model prices cannot be negative".into(),
            ));
        }
        let currency = currency.to_uppercase();
        let now = unix_millis();
        let mut tx = self.pool.begin().await?;
        if service_tier == "default" {
            sqlx::query(
                "INSERT INTO model_prices (id, model, currency, input_micros_per_million, output_micros_per_million, source, updated_at) VALUES ($1, $2, $3, $4, $5, 'manual', $6) ON CONFLICT(model, currency) DO UPDATE SET input_micros_per_million = excluded.input_micros_per_million, output_micros_per_million = excluded.output_micros_per_million, source = excluded.source, updated_at = excluded.updated_at",
            )
            .bind(Uuid::now_v7().to_string())
            .bind(model)
            .bind(&currency)
            .bind(input_micros)
            .bind(output_micros)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        } else if sqlx::query("SELECT id FROM model_prices WHERE model = $1 AND currency = $2")
            .bind(model)
            .bind(&currency)
            .fetch_optional(&mut *tx)
            .await?
            .is_none()
        {
            return Err(AppError::BadRequest(
                "create the default service tier before an additional tier".into(),
            ));
        }
        upsert_price_tier(
            &mut tx,
            model,
            &currency,
            service_tier,
            input_micros,
            cached_input_micros,
            cache_write_micros,
            output_micros,
            "manual",
            now,
            cache_price_estimated,
        )
        .await?;
        tx.commit().await?;
        self.model_price(model, &currency).await
    }

    pub async fn upsert_synced_model_price(
        &self,
        model: &str,
        currency: &str,
        input_per_million: Decimal,
        output_per_million: Decimal,
        source: &str,
    ) -> Result<ModelPriceView, AppError> {
        self.upsert_synced_model_price_tier(
            model,
            currency,
            "default",
            input_per_million,
            input_per_million,
            input_per_million,
            output_per_million,
            source,
            true,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_synced_model_price_tier(
        &self,
        model: &str,
        currency: &str,
        service_tier: &str,
        input_per_million: Decimal,
        cached_input_per_million: Decimal,
        cache_write_per_million: Decimal,
        output_per_million: Decimal,
        source: &str,
        cache_price_estimated: bool,
    ) -> Result<ModelPriceView, AppError> {
        validate_currency(currency)?;
        validate_service_tier(service_tier)?;
        if !matches!(source, "models.dev" | "litellm" | "openrouter") {
            return Err(AppError::BadRequest("unsupported price source".into()));
        }
        let input_micros = decimal_to_micros(input_per_million)?;
        let cached_input_micros = decimal_to_micros(cached_input_per_million)?;
        let cache_write_micros = decimal_to_micros(cache_write_per_million)?;
        let output_micros = decimal_to_micros(output_per_million)?;
        if [
            input_micros,
            cached_input_micros,
            cache_write_micros,
            output_micros,
        ]
        .into_iter()
        .any(|price| price < 0)
        {
            return Err(AppError::BadRequest(
                "model prices cannot be negative".into(),
            ));
        }
        let currency = currency.to_uppercase();
        let now = unix_millis();
        let mut tx = self.pool.begin().await?;
        if service_tier == "default" {
            sqlx::query(
                "INSERT INTO model_prices (id, model, currency, input_micros_per_million, output_micros_per_million, source, updated_at) VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT(model, currency) DO UPDATE SET input_micros_per_million = excluded.input_micros_per_million, output_micros_per_million = excluded.output_micros_per_million, source = excluded.source, updated_at = excluded.updated_at",
            )
            .bind(Uuid::now_v7().to_string())
            .bind(model)
            .bind(&currency)
            .bind(input_micros)
            .bind(output_micros)
            .bind(source)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        } else if sqlx::query("SELECT id FROM model_prices WHERE model = $1 AND currency = $2")
            .bind(model)
            .bind(&currency)
            .fetch_optional(&mut *tx)
            .await?
            .is_none()
        {
            sqlx::query(
                "INSERT INTO model_prices (id, model, currency, input_micros_per_million, output_micros_per_million, source, updated_at) VALUES ($1, $2, $3, $4, $5, $6, $7)",
            )
            .bind(Uuid::now_v7().to_string())
            .bind(model)
            .bind(&currency)
            .bind(input_micros)
            .bind(output_micros)
            .bind(source)
            .bind(now)
            .execute(&mut *tx)
            .await?;
            upsert_price_tier(
                &mut tx,
                model,
                &currency,
                "default",
                input_micros,
                cached_input_micros,
                cache_write_micros,
                output_micros,
                source,
                now,
                true,
            )
            .await?;
        }
        upsert_price_tier(
            &mut tx,
            model,
            &currency,
            service_tier,
            input_micros,
            cached_input_micros,
            cache_write_micros,
            output_micros,
            source,
            now,
            cache_price_estimated,
        )
        .await?;
        tx.commit().await?;
        self.model_price_view(model, &currency).await
    }

    pub async fn list_model_prices(&self, currency: &str) -> Result<Vec<ModelPriceView>, AppError> {
        validate_currency(currency)?;
        let rows = sqlx::query(
            "SELECT model, currency, input_micros_per_million, output_micros_per_million, source, updated_at FROM model_prices WHERE currency = $1 ORDER BY model ASC",
        )
        .bind(currency.to_uppercase())
        .fetch_all(&self.pool)
        .await?;
        let mut prices = Vec::with_capacity(rows.len());
        for row in rows {
            prices.push(self.model_price_view_from_base_row(row).await?);
        }
        Ok(prices)
    }

    pub async fn model_price_view(
        &self,
        model: &str,
        currency: &str,
    ) -> Result<ModelPriceView, AppError> {
        let row = sqlx::query(
            "SELECT model, currency, input_micros_per_million, output_micros_per_million, source, updated_at FROM model_prices WHERE model = $1 AND currency = $2",
        )
        .bind(model)
        .bind(currency.to_uppercase())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::UnpricedModel)?;
        self.model_price_view_from_base_row(row).await
    }

    pub async fn pricing_models(
        &self,
        tenant_external_id: Option<&str>,
    ) -> Result<Vec<String>, AppError> {
        let rows = if let Some(tenant) = tenant_external_id {
            sqlx::query(
                "SELECT model FROM model_prices UNION SELECT a.model FROM usage_daily_aggregates a JOIN key_records k ON k.id = a.key_id JOIN tenants t ON t.id = k.tenant_id WHERE t.external_id = $1 UNION SELECT g.public_model AS model FROM generation_jobs g JOIN tenants t ON t.id = g.tenant_id WHERE t.external_id = $2 UNION SELECT r.public_model AS model FROM model_routes r JOIN tenants t ON t.id = r.tenant_id WHERE t.external_id = $3 ORDER BY model ASC",
            )
            .bind(tenant)
            .bind(tenant)
            .bind(tenant)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query(
                "SELECT model FROM model_prices UNION SELECT model FROM usage_daily_aggregates UNION SELECT public_model AS model FROM generation_jobs UNION SELECT public_model AS model FROM model_routes ORDER BY model ASC",
            )
            .fetch_all(&self.pool)
            .await?
        };
        rows.into_iter()
            .map(|row| row.try_get("model").map_err(AppError::from))
            .collect()
    }

    pub async fn model_price(&self, model: &str, currency: &str) -> Result<ModelPrice, AppError> {
        let row = sqlx::query(
            "SELECT id, input_micros_per_million, output_micros_per_million FROM model_prices WHERE model = $1 AND currency = $2",
        )
        .bind(model)
        .bind(currency.to_uppercase())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::UnpricedModel)?;
        let tiers = self.model_price_tiers(model, currency).await?;
        Ok(ModelPrice {
            id: parse_uuid(row.try_get("id")?)?,
            input_micros_per_million: row.try_get("input_micros_per_million")?,
            output_micros_per_million: row.try_get("output_micros_per_million")?,
            tiers,
        })
    }

    async fn model_price_tiers(
        &self,
        model: &str,
        currency: &str,
    ) -> Result<Vec<ModelPriceTier>, AppError> {
        let rows = sqlx::query(
            "SELECT service_tier, input_micros_per_million, cached_input_micros_per_million, cache_write_micros_per_million, output_micros_per_million, source FROM model_price_tiers WHERE model = $1 AND currency = $2 ORDER BY CASE WHEN service_tier = 'default' THEN 0 ELSE 1 END, service_tier",
        )
        .bind(model)
        .bind(currency.to_uppercase())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(ModelPriceTier {
                    service_tier: row.try_get("service_tier")?,
                    input_micros_per_million: row.try_get("input_micros_per_million")?,
                    cached_input_micros_per_million: row
                        .try_get("cached_input_micros_per_million")?,
                    cache_write_micros_per_million: row
                        .try_get("cache_write_micros_per_million")?,
                    output_micros_per_million: row.try_get("output_micros_per_million")?,
                    source: row.try_get("source")?,
                })
            })
            .collect()
    }

    async fn model_price_view_from_base_row(
        &self,
        row: AnyRow,
    ) -> Result<ModelPriceView, AppError> {
        let model: String = row.try_get("model")?;
        let currency: String = row.try_get("currency")?;
        let tier_rows = sqlx::query(
            "SELECT service_tier, input_micros_per_million, cached_input_micros_per_million, cache_write_micros_per_million, output_micros_per_million, source, updated_at, cache_price_estimated FROM model_price_tiers WHERE model = $1 AND currency = $2 ORDER BY CASE WHEN service_tier = 'default' THEN 0 ELSE 1 END, service_tier",
        )
        .bind(&model)
        .bind(&currency)
        .fetch_all(&self.pool)
        .await?;
        let tiers = tier_rows
            .into_iter()
            .map(|tier| {
                Ok(ModelPriceTierView {
                    service_tier: tier.try_get("service_tier")?,
                    input_per_million: micros_to_decimal_string(
                        tier.try_get("input_micros_per_million")?,
                    ),
                    cached_input_per_million: micros_to_decimal_string(
                        tier.try_get("cached_input_micros_per_million")?,
                    ),
                    cache_write_per_million: micros_to_decimal_string(
                        tier.try_get("cache_write_micros_per_million")?,
                    ),
                    output_per_million: micros_to_decimal_string(
                        tier.try_get("output_micros_per_million")?,
                    ),
                    source: tier.try_get("source")?,
                    updated_at: tier.try_get("updated_at")?,
                    cache_price_estimated: tier.try_get::<i64, _>("cache_price_estimated")? != 0,
                })
            })
            .collect::<Result<Vec<_>, AppError>>()?;
        Ok(ModelPriceView {
            model,
            currency,
            input_per_million: micros_to_decimal_string(row.try_get("input_micros_per_million")?),
            output_per_million: micros_to_decimal_string(row.try_get("output_micros_per_million")?),
            source: row.try_get("source")?,
            updated_at: row.try_get("updated_at")?,
            tiers,
        })
    }

    pub async fn upsert_generation_price(
        &self,
        model: &str,
        currency: &str,
        billing_unit: &str,
        price_per_unit: Decimal,
    ) -> Result<GenerationPrice, AppError> {
        validate_currency(currency)?;
        if !matches!(billing_unit, "job" | "second" | "image" | "megapixel") {
            return Err(AppError::BadRequest(
                "billing_unit must be job, second, image, or megapixel".into(),
            ));
        }
        let micros_per_unit = decimal_to_micros(price_per_unit)?;
        if micros_per_unit < 0 {
            return Err(AppError::BadRequest(
                "generation price cannot be negative".into(),
            ));
        }
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO generation_prices (id, model, currency, billing_unit, micros_per_unit, updated_at) VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT(model, currency) DO UPDATE SET billing_unit = excluded.billing_unit, micros_per_unit = excluded.micros_per_unit, updated_at = excluded.updated_at",
        )
        .bind(id.to_string())
        .bind(model)
        .bind(currency.to_uppercase())
        .bind(billing_unit)
        .bind(micros_per_unit)
        .bind(unix_millis())
        .execute(&self.pool)
        .await?;
        self.generation_price(model, currency).await
    }

    pub async fn generation_price(
        &self,
        model: &str,
        currency: &str,
    ) -> Result<GenerationPrice, AppError> {
        let row = sqlx::query(
            "SELECT id, model, currency, billing_unit, micros_per_unit FROM generation_prices WHERE model = $1 AND currency = $2",
        )
        .bind(model)
        .bind(currency.to_uppercase())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::UnpricedModel)?;
        let micros_per_unit: i64 = row.try_get("micros_per_unit")?;
        Ok(GenerationPrice {
            id: parse_uuid(row.try_get("id")?)?,
            model: row.try_get("model")?,
            currency: row.try_get("currency")?,
            billing_unit: row.try_get("billing_unit")?,
            price_per_unit: micros_to_decimal_string(micros_per_unit),
            micros_per_unit,
        })
    }

    pub async fn create_generation_job(
        &self,
        input: CreateGenerationJobInput,
    ) -> Result<GenerationJobView, AppError> {
        match self.create_generation_job_idempotent(input, None).await? {
            CreateGenerationJobResult::Created(job) => Ok(job),
            CreateGenerationJobResult::Replayed(_) => Err(AppError::Internal),
        }
    }

    pub async fn generation_job_by_idempotency(
        &self,
        key_id: Uuid,
        idempotency: &GenerationJobIdempotency,
    ) -> Result<Option<GenerationJobView>, AppError> {
        validate_generation_job_idempotency(idempotency)?;
        let row = sqlx::query(
            "SELECT id, created_at, updated_at, completed_at, public_model, driver, status, upstream_job_id, estimated_units, billed_units, cost_micros, error_code, result_json, request_hash FROM generation_jobs WHERE key_id = $1 AND client_idempotency_key = $2",
        )
        .bind(key_id.to_string())
        .bind(&idempotency.key)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let existing_hash: Option<String> = row.try_get("request_hash")?;
        if existing_hash.as_deref() != Some(idempotency.request_hash.as_str()) {
            return Err(AppError::BadRequest(
                "Idempotency-Key was already used for a different generation request".into(),
            ));
        }
        Ok(Some(generation_job_view(row)?))
    }

    pub async fn create_generation_job_idempotent(
        &self,
        input: CreateGenerationJobInput,
        idempotency: Option<&GenerationJobIdempotency>,
    ) -> Result<CreateGenerationJobResult, AppError> {
        if let Some(idempotency) = idempotency {
            validate_generation_job_idempotency(idempotency)?;
        }
        let now = unix_millis();
        let mut transaction = self.pool.begin().await?;
        let inserted = sqlx::query(
            "INSERT INTO generation_jobs (id, tenant_id, key_id, upstream_account_id, reservation_id, public_model, upstream_model, driver, status, request_object, estimated_units, billing_unit_snapshot, micros_per_unit_snapshot, client_idempotency_key, request_hash, next_attempt_at, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'queued', $9, $10, $11, $12, $13, $14, $15, $16, $17) ON CONFLICT(key_id, client_idempotency_key) DO NOTHING",
        )
        .bind(input.job_id.to_string())
        .bind(input.key.tenant_id.to_string())
        .bind(input.key.key_id.to_string())
        .bind(input.upstream_account_id.to_string())
        .bind(input.reservation.id.to_string())
        .bind(&input.public_model)
        .bind(&input.upstream_model)
        .bind(&input.driver)
        .bind(input.request_object)
        .bind(input.estimated_units)
        .bind(input.billing_unit)
        .bind(input.micros_per_unit)
        .bind(idempotency.map(|value| value.key.as_str()))
        .bind(idempotency.map(|value| value.request_hash.as_str()))
        .bind(now)
        .bind(now)
        .bind(now)
        .execute(&mut *transaction)
        .await?;
        if inserted.rows_affected() == 0 {
            let row = sqlx::query(
                "SELECT id, created_at, updated_at, completed_at, public_model, driver, status, upstream_job_id, estimated_units, billed_units, cost_micros, error_code, result_json, request_hash, reservation_id FROM generation_jobs WHERE key_id = $1 AND client_idempotency_key = $2",
            )
            .bind(input.key.key_id.to_string())
            .bind(idempotency.map(|value| value.key.as_str()))
            .fetch_one(&mut *transaction)
            .await?;
            let existing_hash: Option<String> = row.try_get("request_hash")?;
            let existing_reservation_id = parse_uuid(row.try_get("reservation_id")?)?;
            let replayed = generation_job_view(row)?;
            transaction.commit().await?;

            if existing_reservation_id != input.reservation.id {
                self.settle_usage(&input.reservation, 0, 0).await?;
            }
            if existing_hash.as_deref() != idempotency.map(|value| value.request_hash.as_str()) {
                return Err(AppError::BadRequest(
                    "Idempotency-Key was already used for a different generation request".into(),
                ));
            }
            return Ok(CreateGenerationJobResult::Replayed(replayed));
        }
        let event_id = Uuid::now_v7().to_string();
        let tenant_id = input.key.tenant_id.to_string();
        let key_id = input.key.key_id.to_string();
        let request_id = input.job_id.to_string();
        if claim_request_event_locator(
            &mut transaction,
            &event_id,
            now,
            &tenant_id,
            &key_id,
            &request_id,
        )
        .await?
        {
            sqlx::query(
                "INSERT INTO request_events (event_id, tenant_id, key_id, request_id, event_at, event_kind, protocol, model, input_tokens, output_tokens, cost_micros) VALUES ($1, $2, $3, $4, $5, 'started', 'generation', $6, 0, 0, 0)",
            )
            .bind(&event_id)
            .bind(&tenant_id)
            .bind(&key_id)
            .bind(&request_id)
            .bind(now)
            .bind(&input.public_model)
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        Ok(CreateGenerationJobResult::Created(GenerationJobView {
            job_id: input.job_id,
            created_at: now,
            updated_at: now,
            completed_at: None,
            model: input.public_model,
            driver: input.driver,
            status: "queued".to_owned(),
            upstream_job_id: None,
            estimated_units: input.estimated_units,
            billed_units: None,
            cost: "0".to_owned(),
            error_code: None,
            result: None,
        }))
    }

    pub async fn list_generation_jobs(
        &self,
        key_id: Uuid,
        limit: i64,
    ) -> Result<Vec<GenerationJobView>, AppError> {
        let rows = sqlx::query(
            "SELECT id, created_at, updated_at, completed_at, public_model, driver, status, upstream_job_id, estimated_units, billed_units, cost_micros, error_code, result_json FROM generation_jobs WHERE key_id = $1 ORDER BY created_at DESC, id DESC LIMIT $2",
        )
        .bind(key_id.to_string())
        .bind(limit.clamp(1, 200))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(generation_job_view).collect()
    }

    pub async fn generation_job(
        &self,
        key_id: Uuid,
        job_id: Uuid,
    ) -> Result<GenerationJobView, AppError> {
        let row = sqlx::query(
            "SELECT id, created_at, updated_at, completed_at, public_model, driver, status, upstream_job_id, estimated_units, billed_units, cost_micros, error_code, result_json FROM generation_jobs WHERE id = $1 AND key_id = $2",
        )
        .bind(job_id.to_string())
        .bind(key_id.to_string())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::NotFound)?;
        generation_job_view(row)
    }

    /// Atomically cancels and refunds a generation job that has not been submitted upstream.
    /// Running jobs deliberately require a driver-specific upstream cancellation flow.
    pub async fn cancel_queued_generation_job(
        &self,
        key_id: Uuid,
        job_id: Uuid,
    ) -> Result<GenerationJobView, AppError> {
        let now = unix_millis();
        let mut transaction = self.pool.begin().await?;
        let select = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT j.status, j.lease_owner, j.lease_expires_at, j.created_at, j.tenant_id, j.public_model, r.id AS reservation_id, r.account_id, r.reserved_micros, r.reserved_tokens, r.rate_window_start, r.status AS reservation_status, r.actual_micros FROM generation_jobs j JOIN usage_reservations r ON r.id = j.reservation_id WHERE j.id = $1 AND j.key_id = $2 FOR UPDATE"
            }
            DatabaseBackend::Sqlite => {
                "SELECT j.status, j.lease_owner, j.lease_expires_at, j.created_at, j.tenant_id, j.public_model, r.id AS reservation_id, r.account_id, r.reserved_micros, r.reserved_tokens, r.rate_window_start, r.status AS reservation_status, r.actual_micros FROM generation_jobs j JOIN usage_reservations r ON r.id = j.reservation_id WHERE j.id = $1 AND j.key_id = $2"
            }
        };
        let row = sqlx::query(select)
            .bind(job_id.to_string())
            .bind(key_id.to_string())
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(AppError::NotFound)?;
        let status: String = row.try_get("status")?;
        if status == "cancelled" {
            aggregate_terminal_generation_job(&mut transaction, &job_id.to_string(), now).await?;
            transaction.commit().await?;
            return self.generation_job(key_id, job_id).await;
        }
        if status != "queued" {
            return Err(AppError::BadRequest(
                "only a queued generation job can be cancelled".into(),
            ));
        }
        let lease_owner: Option<String> = row.try_get("lease_owner")?;
        let lease_expires_at: Option<i64> = row.try_get("lease_expires_at")?;
        if lease_owner.is_some() && lease_expires_at.is_some_and(|expires_at| expires_at >= now) {
            return Err(AppError::BadRequest(
                "generation job is currently being submitted upstream".into(),
            ));
        }

        let reservation_id: String = row.try_get("reservation_id")?;
        let account_id: String = row.try_get("account_id")?;
        let reserved_micros: i64 = row.try_get("reserved_micros")?;
        let reserved_tokens: i64 = row.try_get("reserved_tokens")?;
        let rate_window_start: i64 = row.try_get("rate_window_start")?;
        let reservation_status: String = row.try_get("reservation_status")?;
        let actual_micros: Option<i64> = row.try_get("actual_micros")?;
        let created_at: i64 = row.try_get("created_at")?;
        let tenant_id: String = row.try_get("tenant_id")?;
        let public_model: String = row.try_get("public_model")?;

        if reservation_status != "reserved" && actual_micros != Some(0) {
            return Err(AppError::BadRequest(
                "generation job usage has already been settled".into(),
            ));
        }
        if reservation_status == "reserved" {
            lock_key_budget_state(&mut transaction, key_id, now).await?;
            sqlx::query("UPDATE credit_accounts SET updated_at = updated_at WHERE id = $1")
                .bind(&account_id)
                .execute(&mut *transaction)
                .await?;
            let settled = sqlx::query(
                "UPDATE usage_reservations SET actual_micros = 0, status = 'settled', settled_at = $1 WHERE id = $2 AND status = 'reserved'",
            )
            .bind(now)
            .bind(&reservation_id)
            .execute(&mut *transaction)
            .await?;
            if settled.rows_affected() != 1 {
                return Err(AppError::Internal);
            }
            let budget_state = sqlx::query(
                "UPDATE key_budget_state SET reserved_micros = reserved_micros - $1, updated_at = $2 WHERE key_id = $3 AND reserved_micros >= $4",
            )
            .bind(reserved_micros)
            .bind(now)
            .bind(key_id.to_string())
            .bind(reserved_micros)
            .execute(&mut *transaction)
            .await?;
            if budget_state.rows_affected() != 1 {
                return Err(AppError::Internal);
            }
            sqlx::query(
                "UPDATE credit_accounts SET available_micros = available_micros + $1, reserved_micros = reserved_micros - $2, updated_at = $3 WHERE id = $4",
            )
            .bind(reserved_micros)
            .bind(reserved_micros)
            .bind(now)
            .bind(&account_id)
            .execute(&mut *transaction)
            .await?;
            sqlx::query(
                "UPDATE rate_limit_windows SET tokens = CASE WHEN tokens > $1 THEN tokens - $2 ELSE 0 END WHERE key_id = $3 AND window_start = $4",
            )
            .bind(reserved_tokens)
            .bind(reserved_tokens)
            .bind(key_id.to_string())
            .bind(rate_window_start)
            .execute(&mut *transaction)
            .await?;
            sqlx::query(
                "UPDATE key_runtime_state SET active_requests = CASE WHEN active_requests > 0 THEN active_requests - 1 ELSE 0 END, updated_at = $1 WHERE key_id = $2",
            )
            .bind(now)
            .bind(key_id.to_string())
            .execute(&mut *transaction)
            .await?;
            let usage_ledger_entry_id = Uuid::now_v7();
            let usage_ledger = sqlx::query(
                "INSERT INTO ledger_entries (id, account_id, key_id, kind, amount_micros, currency, source, created_at) SELECT $1, $2, $3, 'usage', 0, currency, $4, $5 FROM credit_accounts WHERE id = $6",
            )
            .bind(usage_ledger_entry_id.to_string())
            .bind(&account_id)
            .bind(key_id.to_string())
            .bind(&reservation_id)
            .bind(now)
            .bind(&account_id)
            .execute(&mut *transaction)
            .await?;
            if usage_ledger.rows_affected() != 1 {
                return Err(AppError::Internal);
            }
            sqlx::query(
                "INSERT INTO key_budget_usage_events (usage_entry_id, reservation_id, key_id, account_id, amount_micros, settled_at) VALUES ($1, $2, $3, $4, 0, $5)",
            )
            .bind(usage_ledger_entry_id.to_string())
            .bind(&reservation_id)
            .bind(key_id.to_string())
            .bind(&account_id)
            .bind(now)
            .execute(&mut *transaction)
            .await?;
        }

        let cancelled = sqlx::query(
            "UPDATE generation_jobs SET status = 'cancelled', billed_units = 0, cost_micros = 0, error_code = 'cancelled_by_user', completed_at = $1, updated_at = $2, lease_owner = NULL, lease_expires_at = NULL WHERE id = $3 AND key_id = $4 AND status = 'queued' AND (lease_expires_at IS NULL OR lease_expires_at < $5)",
        )
        .bind(now)
        .bind(now)
        .bind(job_id.to_string())
        .bind(key_id.to_string())
        .bind(now)
        .execute(&mut *transaction)
        .await?;
        if cancelled.rows_affected() != 1 {
            return Err(AppError::BadRequest(
                "generation job is currently being submitted upstream".into(),
            ));
        }
        aggregate_terminal_generation_job(&mut transaction, &job_id.to_string(), now).await?;
        let event_id = Uuid::now_v7().to_string();
        let key_id_string = key_id.to_string();
        let request_id = job_id.to_string();
        if claim_request_event_locator(
            &mut transaction,
            &event_id,
            now,
            &tenant_id,
            &key_id_string,
            &request_id,
        )
        .await?
        {
            sqlx::query(
                "INSERT INTO request_events (event_id, tenant_id, key_id, request_id, event_at, event_kind, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, error_code) VALUES ($1, $2, $3, $4, $5, 'finished', 'generation', $6, 499, $7, 0, 0, 0, 'cancelled_by_user')",
            )
            .bind(&event_id)
            .bind(&tenant_id)
            .bind(&key_id_string)
            .bind(&request_id)
            .bind(now)
            .bind(public_model)
            .bind(now.saturating_sub(created_at))
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        self.generation_job(key_id, job_id).await
    }

    pub async fn claim_generation_job(
        &self,
        worker_id: &str,
    ) -> Result<Option<GenerationJobWork>, AppError> {
        let now = unix_millis();
        let mut transaction = self.pool.begin().await?;
        let select = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT id FROM generation_jobs WHERE status IN ('queued', 'running') AND next_attempt_at <= $1 AND (lease_expires_at IS NULL OR lease_expires_at < $2) ORDER BY next_attempt_at, created_at, id FOR UPDATE SKIP LOCKED LIMIT 1"
            }
            DatabaseBackend::Sqlite => {
                "SELECT id FROM generation_jobs WHERE status IN ('queued', 'running') AND next_attempt_at <= $1 AND (lease_expires_at IS NULL OR lease_expires_at < $2) ORDER BY next_attempt_at, created_at, id LIMIT 1"
            }
        };
        let candidate = sqlx::query(select)
            .bind(now)
            .bind(now)
            .fetch_optional(&mut *transaction)
            .await?;
        let Some(candidate) = candidate else {
            transaction.commit().await?;
            return Ok(None);
        };
        let job_id: String = candidate.try_get("id")?;
        let claimed = sqlx::query(
            "UPDATE generation_jobs SET lease_owner = $1, lease_expires_at = $2, attempt_count = attempt_count + 1, updated_at = $3 WHERE id = $4 AND status IN ('queued', 'running') AND (lease_expires_at IS NULL OR lease_expires_at < $5)",
        )
        .bind(worker_id)
        .bind(now.saturating_add(60_000))
        .bind(now)
        .bind(&job_id)
        .bind(now)
        .execute(&mut *transaction)
        .await?;
        if claimed.rows_affected() == 0 {
            transaction.commit().await?;
            return Ok(None);
        }
        let row = sqlx::query(
            "SELECT j.id, j.created_at, j.tenant_id, j.key_id, j.upstream_account_id, j.public_model, j.upstream_model, j.driver, j.status, j.request_object, j.upstream_job_id, j.estimated_units, j.attempt_count, j.failure_count, r.id AS reservation_id, r.account_id, r.reserved_micros, r.reserved_tokens, r.rate_window_start, j.micros_per_unit_snapshot FROM generation_jobs j JOIN usage_reservations r ON r.id = j.reservation_id WHERE j.id = $1",
        )
        .bind(&job_id)
        .fetch_one(&mut *transaction)
        .await?;
        transaction.commit().await?;
        let micros_per_unit: i64 = row.try_get("micros_per_unit_snapshot")?;
        let key_id = parse_uuid(row.try_get("key_id")?)?;
        Ok(Some(GenerationJobWork {
            job_id: parse_uuid(row.try_get("id")?)?,
            created_at: row.try_get("created_at")?,
            tenant_id: parse_uuid(row.try_get("tenant_id")?)?,
            key_id,
            upstream_account_id: parse_uuid(row.try_get("upstream_account_id")?)?,
            reservation: UsageReservation {
                id: parse_uuid(row.try_get("reservation_id")?)?,
                account_id: parse_uuid(row.try_get("account_id")?)?,
                key_id,
                reserved_micros: row.try_get("reserved_micros")?,
                input_micros_per_million: 0,
                output_micros_per_million: micros_per_unit
                    .checked_mul(1_000_000)
                    .ok_or(AppError::Internal)?,
                price_tiers: Vec::new(),
                rate_window_start: row.try_get("rate_window_start")?,
                reserved_tokens: row.try_get("reserved_tokens")?,
            },
            public_model: row.try_get("public_model")?,
            upstream_model: row.try_get("upstream_model")?,
            driver: row.try_get("driver")?,
            status: row.try_get("status")?,
            request_object: row.try_get("request_object")?,
            upstream_job_id: row.try_get("upstream_job_id")?,
            estimated_units: row.try_get("estimated_units")?,
            attempt_count: row.try_get("attempt_count")?,
            failure_count: row.try_get("failure_count")?,
        }))
    }

    pub async fn mark_generation_submitted(
        &self,
        job_id: Uuid,
        worker_id: &str,
        upstream_job_id: &str,
    ) -> Result<(), AppError> {
        generation_update_claimed(
            sqlx::query("UPDATE generation_jobs SET status = 'running', upstream_job_id = $1, failure_count = 0, error_code = NULL, next_attempt_at = $2, lease_owner = NULL, lease_expires_at = NULL, updated_at = $3 WHERE id = $4 AND lease_owner = $5")
                .bind(upstream_job_id)
                .bind(unix_millis().saturating_add(2_000))
                .bind(unix_millis())
                .bind(job_id.to_string())
                .bind(worker_id)
                .execute(&self.pool)
                .await?,
        )
    }

    pub async fn reschedule_generation_job(
        &self,
        job_id: Uuid,
        worker_id: &str,
        delay_ms: i64,
        error_code: Option<&str>,
    ) -> Result<(), AppError> {
        let now = unix_millis();
        generation_update_claimed(
            sqlx::query("UPDATE generation_jobs SET next_attempt_at = $1, error_code = $2, failure_count = CASE WHEN $3 IS NULL THEN 0 ELSE failure_count + 1 END, lease_owner = NULL, lease_expires_at = NULL, updated_at = $4 WHERE id = $5 AND lease_owner = $6")
                .bind(now.saturating_add(delay_ms.max(500)))
                .bind(error_code)
                .bind(error_code)
                .bind(now)
                .bind(job_id.to_string())
                .bind(worker_id)
                .execute(&self.pool)
                .await?,
        )
    }

    pub async fn renew_generation_lease(
        &self,
        job_id: Uuid,
        worker_id: &str,
    ) -> Result<(), AppError> {
        let now = unix_millis();
        generation_update_claimed(
            sqlx::query(
                "UPDATE generation_jobs SET lease_expires_at = $1, updated_at = $2 WHERE id = $3 AND lease_owner = $4 AND status IN ('queued', 'running')",
            )
            .bind(now.saturating_add(60_000))
            .bind(now)
            .bind(job_id.to_string())
            .bind(worker_id)
            .execute(&self.pool)
            .await?,
        )
    }

    pub async fn delete_expired_rate_windows(&self, limit: i64) -> Result<u64, AppError> {
        let cutoff = unix_millis().saturating_sub(2 * 24 * 60 * 60 * 1_000);
        let rows = sqlx::query(
            "DELETE FROM rate_limit_windows WHERE (key_id, window_start) IN (SELECT key_id, window_start FROM rate_limit_windows WHERE window_start < $1 ORDER BY window_start ASC, key_id ASC LIMIT $2)",
        )
        .bind(cutoff)
        .bind(limit.clamp(1, 100_000))
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(rows)
    }

    pub async fn delete_expired_budget_rollups(&self, limit: i64) -> Result<u64, AppError> {
        let cutoff_day = unix_millis().saturating_sub(7 * 86_400_000) / 86_400_000;
        let cutoff_at = cutoff_day.saturating_mul(86_400_000);
        let limit = limit.clamp(1, 100_000);
        let mut tx = self.pool.begin().await?;
        let events = sqlx::query(
            "DELETE FROM key_budget_usage_events WHERE usage_entry_id IN (SELECT usage_entry_id FROM key_budget_usage_events WHERE settled_at < $1 ORDER BY settled_at ASC, usage_entry_id ASC LIMIT $2)",
        )
        .bind(cutoff_at)
        .bind(limit)
        .execute(&mut *tx)
        .await?;
        let daily = sqlx::query(
            "DELETE FROM key_budget_daily_rollups WHERE (key_id, day_bucket) IN (SELECT key_id, day_bucket FROM key_budget_daily_rollups WHERE day_bucket < $1 ORDER BY day_bucket ASC, key_id ASC LIMIT $2)",
        )
        .bind(cutoff_day)
        .bind(limit)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(events.rows_affected().saturating_add(daily.rows_affected()))
    }

    pub async fn finish_generation_job(
        &self,
        input: FinishGenerationJobInput<'_>,
    ) -> Result<(), AppError> {
        if !matches!(input.status, "succeeded" | "failed" | "cancelled") {
            return Err(AppError::Internal);
        }
        let now = unix_millis();
        let result_json = input
            .result
            .map(serde_json::to_string)
            .transpose()
            .map_err(|_| AppError::Internal)?;
        let mut transaction = self.pool.begin().await?;
        let updated = sqlx::query("UPDATE generation_jobs SET status = $1, billed_units = $2, cost_micros = $3, result_json = $4, error_code = $5, completed_at = $6, updated_at = $7, lease_owner = NULL, lease_expires_at = NULL WHERE id = $8 AND lease_owner = $9")
                .bind(input.status)
                .bind(input.billed_units)
                .bind(input.cost_micros)
                .bind(result_json)
                .bind(input.error_code)
                .bind(now)
                .bind(now)
                .bind(input.job_id.to_string())
                .bind(input.worker_id)
                .execute(&mut *transaction)
                .await?;
        if updated.rows_affected() != 1 {
            transaction.commit().await?;
            return Err(AppError::NotFound);
        }
        aggregate_terminal_generation_job(&mut transaction, &input.job_id.to_string(), now).await?;
        let job = sqlx::query("SELECT tenant_id, key_id FROM generation_jobs WHERE id = $1")
            .bind(input.job_id.to_string())
            .fetch_one(&mut *transaction)
            .await?;
        let tenant_id: String = job.try_get("tenant_id")?;
        let key_id: String = job.try_get("key_id")?;
        let request_id = input.job_id.to_string();
        let event_id = Uuid::now_v7().to_string();
        if claim_request_event_locator(
            &mut transaction,
            &event_id,
            now,
            &tenant_id,
            &key_id,
            &request_id,
        )
        .await?
        {
            sqlx::query(
                "INSERT INTO request_events (event_id, tenant_id, key_id, request_id, event_at, event_kind, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, error_code) SELECT $1, tenant_id, key_id, id, $2, 'finished', 'generation', public_model, CASE WHEN status = 'succeeded' THEN 200 ELSE 502 END, $3 - created_at, 0, 0, cost_micros, error_code FROM generation_jobs WHERE id = $4",
            )
            .bind(&event_id)
            .bind(now)
            .bind(now)
            .bind(&request_id)
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    pub async fn upstream_driver(&self, account_id: Uuid) -> Result<String, AppError> {
        sqlx::query("SELECT driver FROM upstream_accounts WHERE id = $1")
            .bind(account_id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .ok_or(AppError::NotFound)?
            .try_get("driver")
            .map_err(AppError::from)
    }

    pub async fn allowed_models(&self, key: &AuthenticatedKey) -> Result<Vec<String>, AppError> {
        let rows = sqlx::query("SELECT model FROM model_prices WHERE currency = $1 UNION SELECT model FROM generation_prices WHERE currency = $2 ORDER BY model")
            .bind(&key.currency)
            .bind(&key.currency)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|row| row.try_get::<String, _>("model").map_err(AppError::from))
            .filter(|result| {
                result
                    .as_ref()
                    .map(|model| key.policy.allows_model(model))
                    .unwrap_or(true)
            })
            .collect()
    }

    pub async fn reserve_usage(
        &self,
        key: &AuthenticatedKey,
        price: &ModelPrice,
        input_token_ceiling: i64,
        output_token_ceiling: i64,
    ) -> Result<UsageReservation, AppError> {
        let maximum_input_price = price
            .tiers
            .iter()
            .flat_map(|tier| {
                [
                    tier.input_micros_per_million,
                    tier.cached_input_micros_per_million,
                    tier.cache_write_micros_per_million,
                ]
            })
            .max()
            .unwrap_or(price.input_micros_per_million)
            .max(price.input_micros_per_million);
        let maximum_output_price = price
            .tiers
            .iter()
            .map(|tier| tier.output_micros_per_million)
            .max()
            .unwrap_or(price.output_micros_per_million)
            .max(price.output_micros_per_million);
        let reserved_micros = priced_tokens(input_token_ceiling, maximum_input_price)
            .checked_add(priced_tokens(output_token_ceiling, maximum_output_price))
            .ok_or(AppError::QuotaExceeded)?;
        let reserved_tokens = input_token_ceiling
            .checked_add(output_token_ceiling)
            .ok_or(AppError::RateLimited)?;
        if reserved_tokens > key.policy.tokens_per_minute as i64 {
            return Err(AppError::RateLimited);
        }
        let now = unix_millis();
        let window_start = now / 60_000 * 60_000;
        let mut tx = self.pool.begin().await?;
        let (settled_lifetime_micros, active_reserved) =
            lock_key_budget_state(&mut tx, key.key_id, now).await?;

        let daily_settled = if key.policy.daily_budget.is_some() {
            key_budget_daily_settled(&mut tx, key.key_id, now).await?
        } else {
            0
        };
        let weekly_settled = if key.policy.weekly_budget.is_some() {
            key_budget_rolling_weekly_settled(&mut tx, key.key_id, now).await?
        } else {
            0
        };
        for (configured_budget, settled) in [
            (key.policy.daily_budget.as_deref(), daily_settled),
            (key.policy.weekly_budget.as_deref(), weekly_settled),
            (
                key.policy.lifetime_budget.as_deref(),
                settled_lifetime_micros,
            ),
        ] {
            let Some(configured_budget) = configured_budget else {
                continue;
            };
            let budget_micros = decimal_to_micros(
                Decimal::from_str_exact(configured_budget).map_err(|_| AppError::Internal)?,
            )?;
            if settled
                .saturating_add(active_reserved)
                .saturating_add(reserved_micros)
                > budget_micros
            {
                return Err(AppError::QuotaExceeded);
            }
        }

        let rate_result = sqlx::query(
            "INSERT INTO rate_limit_windows (key_id, window_start, requests, tokens) VALUES ($1, $2, 1, $3) ON CONFLICT(key_id, window_start) DO UPDATE SET requests = rate_limit_windows.requests + 1, tokens = rate_limit_windows.tokens + $4 WHERE rate_limit_windows.requests < $5 AND rate_limit_windows.tokens + $6 <= $7",
        )
        .bind(key.key_id.to_string())
        .bind(window_start)
        .bind(reserved_tokens)
        .bind(reserved_tokens)
        .bind(i64::from(key.policy.requests_per_minute))
        .bind(reserved_tokens)
        .bind(key.policy.tokens_per_minute as i64)
        .execute(&mut *tx)
        .await?;
        if rate_result.rows_affected() == 0 {
            return Err(AppError::RateLimited);
        }

        let concurrency_result = sqlx::query(
            "INSERT INTO key_runtime_state (key_id, active_requests, updated_at) VALUES ($1, 1, $2) ON CONFLICT(key_id) DO UPDATE SET active_requests = CASE WHEN key_runtime_state.updated_at < $3 THEN 1 ELSE key_runtime_state.active_requests + 1 END, updated_at = excluded.updated_at WHERE key_runtime_state.updated_at < $4 OR key_runtime_state.active_requests < $5",
        )
        .bind(key.key_id.to_string())
        .bind(now)
        .bind(now.saturating_sub(30 * 60 * 1_000))
        .bind(now.saturating_sub(30 * 60 * 1_000))
        .bind(i64::from(key.policy.max_concurrency))
        .execute(&mut *tx)
        .await?;
        if concurrency_result.rows_affected() == 0 {
            return Err(AppError::RateLimited);
        }

        let balance_result = sqlx::query(
            "UPDATE credit_accounts SET available_micros = available_micros - $1, reserved_micros = reserved_micros + $2, updated_at = $3 WHERE id = $4 AND currency = $5 AND available_micros >= $6",
        )
        .bind(reserved_micros)
        .bind(reserved_micros)
        .bind(now)
        .bind(key.account_id.to_string())
        .bind(&key.currency)
        .bind(reserved_micros)
        .execute(&mut *tx)
        .await?;
        if balance_result.rows_affected() == 0 {
            return Err(AppError::QuotaExceeded);
        }
        sqlx::query(
            "UPDATE key_budget_state SET reserved_micros = reserved_micros + $1, updated_at = $2 WHERE key_id = $3",
        )
        .bind(reserved_micros)
        .bind(now)
        .bind(key.key_id.to_string())
        .execute(&mut *tx)
        .await?;

        let id = Uuid::now_v7();
        let price_snapshot_json = serde_json::to_string(price).map_err(|_| AppError::Internal)?;
        sqlx::query(
            "INSERT INTO usage_reservations (id, account_id, key_id, price_id, reserved_micros, reserved_tokens, rate_window_start, status, created_at, price_snapshot_json) VALUES ($1, $2, $3, $4, $5, $6, $7, 'reserved', $8, $9)",
        )
        .bind(id.to_string())
        .bind(key.account_id.to_string())
        .bind(key.key_id.to_string())
        .bind(price.id.to_string())
        .bind(reserved_micros)
        .bind(reserved_tokens)
        .bind(window_start)
        .bind(now)
        .bind(price_snapshot_json)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        Ok(UsageReservation {
            id,
            account_id: key.account_id,
            key_id: key.key_id,
            reserved_micros,
            input_micros_per_million: price.input_micros_per_million,
            output_micros_per_million: price.output_micros_per_million,
            price_tiers: price.tiers.clone(),
            rate_window_start: window_start,
            reserved_tokens,
        })
    }

    pub async fn settle_usage(
        &self,
        reservation: &UsageReservation,
        input_tokens: i64,
        output_tokens: i64,
    ) -> Result<i64, AppError> {
        self.settle_token_usage(
            reservation,
            &TokenUsage {
                input_tokens,
                output_tokens,
                ..TokenUsage::default()
            },
        )
        .await
    }

    pub async fn settle_token_usage(
        &self,
        reservation: &UsageReservation,
        usage: &TokenUsage,
    ) -> Result<i64, AppError> {
        validate_token_usage(usage)?;
        let calculated_micros = price_token_usage(reservation, usage)?;
        let now = unix_millis();
        let mut tx = self.pool.begin().await?;
        let (settled_lifetime_micros, reserved_micros) =
            lock_key_budget_state(&mut tx, reservation.key_id, now).await?;
        sqlx::query("UPDATE credit_accounts SET updated_at = updated_at WHERE id = $1")
            .bind(reservation.account_id.to_string())
            .execute(&mut *tx)
            .await?;
        let settlement_context =
            sqlx::query("SELECT a.available_micros, k.policy_json FROM credit_accounts a JOIN key_records k ON k.id = $1 AND k.account_id = a.id WHERE a.id = $2")
                .bind(reservation.key_id.to_string())
                .bind(reservation.account_id.to_string())
                .fetch_one(&mut *tx)
                .await?;
        let available_micros: i64 = settlement_context.try_get("available_micros")?;
        let policy_json: String = settlement_context.try_get("policy_json")?;
        let policy: KeyPolicy =
            serde_json::from_str(&policy_json).map_err(|_| AppError::Internal)?;
        let mut maximum_charge = available_micros
            .max(0)
            .saturating_add(reservation.reserved_micros);
        let other_active_reserved = reserved_micros
            .saturating_sub(reservation.reserved_micros)
            .max(0);
        let daily_settled = if policy.daily_budget.is_some() {
            key_budget_daily_settled(&mut tx, reservation.key_id, now).await?
        } else {
            0
        };
        let weekly_settled = if policy.weekly_budget.is_some() {
            key_budget_rolling_weekly_settled(&mut tx, reservation.key_id, now).await?
        } else {
            0
        };
        for (configured_budget, settled) in [
            (policy.daily_budget.as_deref(), daily_settled),
            (policy.weekly_budget.as_deref(), weekly_settled),
            (policy.lifetime_budget.as_deref(), settled_lifetime_micros),
        ] {
            let Some(configured_budget) = configured_budget else {
                continue;
            };
            let budget_micros = decimal_to_micros(
                Decimal::from_str_exact(configured_budget).map_err(|_| AppError::Internal)?,
            )?;
            maximum_charge = maximum_charge.min(
                budget_micros
                    .saturating_sub(settled)
                    .saturating_sub(other_active_reserved)
                    .max(0),
            );
        }
        let actual_micros = calculated_micros.min(maximum_charge);
        if actual_micros != calculated_micros {
            tracing::warn!(
                reservation_id = %reservation.id,
                calculated_micros,
                charged_micros = actual_micros,
                "upstream usage exceeded the account hard balance limit"
            );
        }
        let released = reservation
            .reserved_micros
            .saturating_sub(actual_micros)
            .max(0);
        let overage = actual_micros
            .saturating_sub(reservation.reserved_micros)
            .max(0);
        let claimed = sqlx::query(
            "UPDATE usage_reservations SET actual_micros = $1, status = 'settled', settled_at = $2 WHERE id = $3 AND status = 'reserved'",
        )
        .bind(actual_micros)
        .bind(now)
        .bind(reservation.id.to_string())
        .execute(&mut *tx)
        .await?;
        if claimed.rows_affected() == 0 {
            let existing: i64 = sqlx::query(
                "SELECT actual_micros FROM usage_reservations WHERE id = $1 AND status = 'settled'",
            )
            .bind(reservation.id.to_string())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AppError::NotFound)?
            .try_get("actual_micros")?;
            tx.commit().await?;
            return Ok(existing);
        }
        let budget_state = sqlx::query(
            "UPDATE key_budget_state SET settled_lifetime_micros = settled_lifetime_micros + $1, reserved_micros = reserved_micros - $2, updated_at = $3 WHERE key_id = $4 AND reserved_micros >= $5",
        )
        .bind(actual_micros)
        .bind(reservation.reserved_micros)
        .bind(now)
        .bind(reservation.key_id.to_string())
        .bind(reservation.reserved_micros)
        .execute(&mut *tx)
        .await?;
        if budget_state.rows_affected() != 1 {
            return Err(AppError::Internal);
        }
        sqlx::query(
            "INSERT INTO key_budget_daily_rollups (key_id, day_bucket, settled_micros) VALUES ($1, $2, $3) ON CONFLICT(key_id, day_bucket) DO UPDATE SET settled_micros = key_budget_daily_rollups.settled_micros + excluded.settled_micros",
        )
        .bind(reservation.key_id.to_string())
        .bind(now / 86_400_000)
        .bind(actual_micros)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE credit_accounts SET available_micros = available_micros + $1 - $2, reserved_micros = reserved_micros - $3, updated_at = $4 WHERE id = $5",
        )
        .bind(released)
        .bind(overage)
        .bind(reservation.reserved_micros)
        .bind(now)
        .bind(reservation.account_id.to_string())
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO account_usage_state (account_id, settled_lifetime_micros, updated_at) VALUES ($1, $2, $3) ON CONFLICT(account_id) DO UPDATE SET settled_lifetime_micros = account_usage_state.settled_lifetime_micros + excluded.settled_lifetime_micros, updated_at = excluded.updated_at",
        )
        .bind(reservation.account_id.to_string())
        .bind(actual_micros)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        let actual_tokens = usage.total_tokens();
        sqlx::query(
            "UPDATE rate_limit_windows SET tokens = CASE WHEN tokens - $1 + $2 < 0 THEN 0 ELSE tokens - $3 + $4 END WHERE key_id = $5 AND window_start = $6",
        )
        .bind(reservation.reserved_tokens)
        .bind(actual_tokens)
        .bind(reservation.reserved_tokens)
        .bind(actual_tokens)
        .bind(reservation.key_id.to_string())
        .bind(reservation.rate_window_start)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE key_runtime_state SET active_requests = CASE WHEN active_requests > 0 THEN active_requests - 1 ELSE 0 END, updated_at = $1 WHERE key_id = $2",
        )
        .bind(now)
        .bind(reservation.key_id.to_string())
        .execute(&mut *tx)
        .await?;
        let usage_ledger_entry_id = Uuid::now_v7();
        let usage_ledger = sqlx::query(
            "INSERT INTO ledger_entries (id, account_id, key_id, kind, amount_micros, currency, source, created_at) SELECT $1, $2, $3, 'usage', $4, currency, $5, $6 FROM credit_accounts WHERE id = $7",
        )
        .bind(usage_ledger_entry_id.to_string())
        .bind(reservation.account_id.to_string())
        .bind(reservation.key_id.to_string())
        .bind(-actual_micros)
        .bind(reservation.id.to_string())
        .bind(now)
        .bind(reservation.account_id.to_string())
        .execute(&mut *tx)
        .await?;
        if usage_ledger.rows_affected() != 1 {
            return Err(AppError::Internal);
        }
        sqlx::query(
            "INSERT INTO key_budget_usage_events (usage_entry_id, reservation_id, key_id, account_id, amount_micros, settled_at) VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(usage_ledger_entry_id.to_string())
        .bind(reservation.id.to_string())
        .bind(reservation.key_id.to_string())
        .bind(reservation.account_id.to_string())
        .bind(actual_micros)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        // Attribute spend FIFO to active subscription cycles. This keeps each
        // cycle's consumed and safely-revocable remainder exact even when the
        // account also contains one-off grants.
        let mut entitlement_usage = actual_micros;
        if entitlement_usage > 0 {
            let cycles = sqlx::query(
                "SELECT c.id, c.funded_micros - c.consumed_micros AS remaining_micros FROM entitlement_cycles c JOIN subscription_entitlements e ON e.id = c.entitlement_id WHERE e.account_id = $1 AND e.status = 'active' AND e.current_cycle_id = c.id AND c.status = 'active' AND c.period_start <= $2 AND c.period_end > $3 AND c.funded_micros > c.consumed_micros ORDER BY c.period_end ASC, c.id ASC",
            )
            .bind(reservation.account_id.to_string())
            .bind(now)
            .bind(now)
            .fetch_all(&mut *tx)
            .await?;
            for cycle in cycles {
                if entitlement_usage == 0 {
                    break;
                }
                let cycle_id: String = cycle.try_get("id")?;
                let remaining_micros: i64 = cycle.try_get("remaining_micros")?;
                let allocated = entitlement_usage.min(remaining_micros.max(0));
                if allocated == 0 {
                    continue;
                }
                sqlx::query(
                    "UPDATE entitlement_cycles SET consumed_micros = consumed_micros + $1, updated_at = $2 WHERE id = $3 AND funded_micros - consumed_micros >= $4",
                )
                .bind(allocated)
                .bind(now)
                .bind(&cycle_id)
                .bind(allocated)
                .execute(&mut *tx)
                .await?;
                sqlx::query(
                    "INSERT INTO entitlement_usage_allocations (id, entitlement_cycle_id, usage_ledger_entry_id, amount_micros, created_at) VALUES ($1, $2, $3, $4, $5)",
                )
                .bind(Uuid::now_v7().to_string())
                .bind(cycle_id)
                .bind(usage_ledger_entry_id.to_string())
                .bind(allocated)
                .bind(now)
                .execute(&mut *tx)
                .await?;
                entitlement_usage = entitlement_usage.saturating_sub(allocated);
            }
        }
        tx.commit().await?;
        Ok(actual_micros)
    }

    pub async fn release_orphaned_reservations(&self, limit: i64) -> Result<u64, AppError> {
        let cutoff = unix_millis().saturating_sub(30 * 60 * 1_000);
        let rows = sqlx::query(
            "SELECT r.id, r.account_id, r.key_id, r.reserved_micros, r.reserved_tokens, r.rate_window_start, q.id AS request_id, q.created_at AS request_created_at FROM usage_reservations r LEFT JOIN request_records q ON q.reservation_id = r.id WHERE r.status = 'reserved' AND r.created_at < $1 AND q.completed_at IS NULL AND NOT EXISTS (SELECT 1 FROM generation_jobs g WHERE g.reservation_id = r.id) ORDER BY r.created_at, r.id LIMIT $2",
        )
        .bind(cutoff)
        .bind(limit.clamp(1, 1_000))
        .fetch_all(&self.pool)
        .await?;
        let mut released = 0_u64;
        for row in rows {
            let request_id = row
                .try_get::<Option<String>, _>("request_id")?
                .map(parse_uuid)
                .transpose()?;
            let request_created_at = row.try_get::<Option<i64>, _>("request_created_at")?;
            let reservation = UsageReservation {
                id: parse_uuid(row.try_get("id")?)?,
                account_id: parse_uuid(row.try_get("account_id")?)?,
                key_id: parse_uuid(row.try_get("key_id")?)?,
                reserved_micros: row.try_get("reserved_micros")?,
                input_micros_per_million: 0,
                output_micros_per_million: 0,
                price_tiers: Vec::new(),
                rate_window_start: row.try_get("rate_window_start")?,
                reserved_tokens: row.try_get("reserved_tokens")?,
            };
            self.settle_usage(&reservation, 0, 0).await?;
            if let Some(request_id) = request_id {
                self.record_request_finished(FinishRequest {
                    request_id,
                    status_code: 504,
                    duration_ms: request_created_at
                        .map(|created_at| unix_millis().saturating_sub(created_at))
                        .unwrap_or_default(),
                    input_tokens: 0,
                    output_tokens: 0,
                    cost_micros: 0,
                    error_code: Some("request_expired".to_owned()),
                    response_object: format!("gap://{request_id}/response"),
                })
                .await?;
            }
            released = released.saturating_add(1);
        }
        Ok(released)
    }

    pub async fn expire_key_provisioning_responses(&self, limit: i64) -> Result<u64, AppError> {
        let cutoff = unix_millis().saturating_sub(24 * 60 * 60 * 1_000);
        let now = unix_millis();
        let limit = limit.clamp(1, 10_000);
        let mut transaction = self.pool.begin().await?;
        let provisioning = sqlx::query(
            "UPDATE key_records SET issued_key_ciphertext = NULL WHERE id IN (SELECT id FROM key_records WHERE issued_key_ciphertext IS NOT NULL AND created_at < $1 ORDER BY created_at, id LIMIT $2)",
        )
        .bind(cutoff)
        .bind(limit)
        .execute(&mut *transaction)
        .await?;
        let rotations = sqlx::query(
            "UPDATE credential_rotation_replays SET response_ciphertext = NULL WHERE idempotency_key IN (SELECT idempotency_key FROM credential_rotation_replays WHERE response_ciphertext IS NOT NULL AND expires_at <= $1 ORDER BY expires_at, idempotency_key LIMIT $2)",
        )
        .bind(now)
        .bind(limit)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(provisioning
            .rows_affected()
            .saturating_add(rotations.rows_affected()))
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

    pub async fn grant(
        &self,
        account_id: Uuid,
        amount: Decimal,
        source: &str,
        idempotency_key: &str,
    ) -> Result<String, AppError> {
        validate_idempotency_key(idempotency_key, "Idempotency-Key")?;
        let source = source.trim();
        if source.is_empty() || source.len() > 200 {
            return Err(AppError::BadRequest(
                "source must contain 1 to 200 characters".into(),
            ));
        }
        let amount_micros = decimal_to_micros(amount)?;
        if amount_micros <= 0 {
            return Err(AppError::BadRequest("grant amount must be positive".into()));
        }
        let now = unix_millis();
        let mut tx = self.pool.begin().await?;
        let account_lock =
            sqlx::query("UPDATE credit_accounts SET updated_at = updated_at WHERE id = $1")
                .bind(account_id.to_string())
                .execute(&mut *tx)
                .await?;
        if account_lock.rows_affected() != 1 {
            return Err(AppError::NotFound);
        }
        let row = sqlx::query("SELECT currency FROM credit_accounts WHERE id = $1")
            .bind(account_id.to_string())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AppError::NotFound)?;
        let currency: String = row.try_get("currency")?;
        let usage_snapshot: i64 = sqlx::query(
            "SELECT settled_lifetime_micros FROM account_usage_state WHERE account_id = $1",
        )
        .bind(account_id.to_string())
        .fetch_one(&mut *tx)
        .await?
        .try_get("settled_lifetime_micros")?;
        let inserted = sqlx::query(
            "INSERT INTO ledger_entries (id, account_id, kind, amount_micros, currency, source, idempotency_key, created_at, account_usage_micros_snapshot) VALUES ($1, $2, 'grant', $3, $4, $5, $6, $7, $8) ON CONFLICT DO NOTHING",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(account_id.to_string())
        .bind(amount_micros)
        .bind(&currency)
        .bind(source)
        .bind(idempotency_key)
        .bind(now)
        .bind(usage_snapshot)
        .execute(&mut *tx)
        .await?;
        if inserted.rows_affected() == 0 {
            let existing = sqlx::query(
                "SELECT amount_micros, source FROM ledger_entries WHERE idempotency_key = $1 AND account_id = $2 AND kind = 'grant'",
            )
            .bind(idempotency_key)
            .bind(account_id.to_string())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AppError::Forbidden)?;
            let existing_amount: i64 = existing.try_get("amount_micros")?;
            let existing_source: String = existing.try_get("source")?;
            if existing_amount != amount_micros || existing_source != source {
                return Err(AppError::BadRequest(
                    "Idempotency-Key was already used for a different grant".into(),
                ));
            }
            tx.commit().await?;
            return Ok(micros_to_decimal_string(existing_amount));
        }
        sqlx::query(
            "UPDATE credit_accounts SET available_micros = available_micros + $1, updated_at = $2 WHERE id = $3",
        )
        .bind(amount_micros)
        .bind(now)
        .bind(account_id.to_string())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(micros_to_decimal_string(amount_micros))
    }

    pub async fn reverse_grant(
        &self,
        account_id: Uuid,
        grant_idempotency_key: &str,
        source: &str,
        idempotency_key: &str,
    ) -> Result<String, AppError> {
        validate_idempotency_key(grant_idempotency_key, "grant_idempotency_key")?;
        validate_idempotency_key(idempotency_key, "Idempotency-Key")?;
        let source = source.trim();
        if source.is_empty() || source.len() > 200 {
            return Err(AppError::BadRequest(
                "source must contain 1 to 200 characters".into(),
            ));
        }

        let now = unix_millis();
        let mut tx = self.pool.begin().await?;
        let account_lock =
            sqlx::query("UPDATE credit_accounts SET updated_at = updated_at WHERE id = $1")
                .bind(account_id.to_string())
                .execute(&mut *tx)
                .await?;
        if account_lock.rows_affected() != 1 {
            return Err(AppError::NotFound);
        }
        let original = sqlx::query(
            "SELECT id, amount_micros, currency, account_usage_micros_snapshot FROM ledger_entries WHERE account_id = $1 AND kind = 'grant' AND idempotency_key = $2",
        )
        .bind(account_id.to_string())
        .bind(grant_idempotency_key)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AppError::NotFound)?;
        let original_id: String = original.try_get("id")?;
        let amount_micros: i64 = original.try_get("amount_micros")?;
        let currency: String = original.try_get("currency")?;
        let usage_snapshot: i64 = original.try_get("account_usage_micros_snapshot")?;
        let current_usage: i64 = sqlx::query(
            "SELECT settled_lifetime_micros FROM account_usage_state WHERE account_id = $1",
        )
        .bind(account_id.to_string())
        .fetch_one(&mut *tx)
        .await?
        .try_get("settled_lifetime_micros")?;
        if current_usage != usage_snapshot {
            return Err(AppError::BadRequest(
                "grant cannot be automatically reversed after account usage".into(),
            ));
        }

        let inserted = sqlx::query(
            "INSERT INTO ledger_entries (id, account_id, kind, amount_micros, currency, source, idempotency_key, reference_entry_id, created_at) VALUES ($1, $2, 'grant_reversal', $3, $4, $5, $6, $7, $8) ON CONFLICT DO NOTHING",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(account_id.to_string())
        .bind(-amount_micros)
        .bind(&currency)
        .bind(source)
        .bind(idempotency_key)
        .bind(&original_id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        if inserted.rows_affected() == 0 {
            let replay = sqlx::query(
                "SELECT amount_micros, reference_entry_id FROM ledger_entries WHERE account_id = $1 AND kind = 'grant_reversal' AND idempotency_key = $2",
            )
            .bind(account_id.to_string())
            .bind(idempotency_key)
            .fetch_optional(&mut *tx)
            .await?;
            if let Some(replay) = replay {
                let replay_reference: Option<String> = replay.try_get("reference_entry_id")?;
                if replay_reference.as_deref() != Some(original_id.as_str()) {
                    return Err(AppError::BadRequest(
                        "Idempotency-Key was already used for a different grant reversal".into(),
                    ));
                }
                let replay_amount: i64 = replay.try_get("amount_micros")?;
                tx.commit().await?;
                return Ok(micros_to_decimal_string(replay_amount.saturating_abs()));
            }
            let existing_idempotency = sqlx::query(
                "SELECT kind FROM ledger_entries WHERE account_id = $1 AND idempotency_key = $2",
            )
            .bind(account_id.to_string())
            .bind(idempotency_key)
            .fetch_optional(&mut *tx)
            .await?;
            if existing_idempotency.is_some() {
                return Err(AppError::BadRequest(
                    "Idempotency-Key was already used for a different ledger operation".into(),
                ));
            }
            return Err(AppError::BadRequest("grant was already reversed".into()));
        }

        let updated = sqlx::query(
            "UPDATE credit_accounts SET available_micros = available_micros - $1, updated_at = $2 WHERE id = $3 AND currency = $4 AND available_micros >= $5",
        )
        .bind(amount_micros)
        .bind(now)
        .bind(account_id.to_string())
        .bind(&currency)
        .bind(amount_micros)
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() != 1 {
            let exists = sqlx::query("SELECT id FROM credit_accounts WHERE id = $1")
                .bind(account_id.to_string())
                .fetch_optional(&mut *tx)
                .await?
                .is_some();
            return Err(if exists {
                AppError::QuotaExceeded
            } else {
                AppError::NotFound
            });
        }
        tx.commit().await?;
        Ok(micros_to_decimal_string(amount_micros))
    }

    pub async fn record_request_started(&self, request: NewRequest) -> Result<(), AppError> {
        let now = unix_millis();
        let mut transaction = self.pool.begin().await?;
        let request_id = request.request_id.to_string();
        let tenant_id = request.tenant_id.to_string();
        let key_id = request.key_id.to_string();
        let reservation_id = request.reservation_id.to_string();
        let upstream_account_id = request.upstream_account_id.map(|id| id.to_string());
        let model_route_id = request.model_route_id.map(|id| id.to_string());
        let claimed =
            claim_request_record_locator(&mut transaction, &request_id, now, &tenant_id, &key_id)
                .await?;
        if !claimed {
            let existing = sqlx::query(
                "SELECT tenant_id, key_id, protocol, model, request_object, reservation_id, upstream_account_id, model_route_id FROM request_records WHERE id = $1 AND created_at = $2",
            )
            .bind(&request_id)
            .bind(now)
            .fetch_optional(&mut *transaction)
            .await?;
            let Some(existing) = existing else {
                return Err(AppError::BadRequest(
                    "request locator exists without its request record".into(),
                ));
            };
            let replay_matches = existing.try_get::<String, _>("tenant_id")? == tenant_id
                && existing.try_get::<String, _>("key_id")? == key_id
                && existing.try_get::<String, _>("protocol")? == request.protocol
                && existing.try_get::<String, _>("model")? == request.model
                && existing.try_get::<String, _>("request_object")? == request.request_object
                && existing.try_get::<String, _>("reservation_id")? == reservation_id
                && existing.try_get::<Option<String>, _>("upstream_account_id")?
                    == upstream_account_id
                && existing.try_get::<Option<String>, _>("model_route_id")? == model_route_id;
            if !replay_matches {
                return Err(AppError::BadRequest(
                    "request id replay does not match the existing request".into(),
                ));
            }
            transaction.commit().await?;
            return Ok(());
        }
        sqlx::query(
            "INSERT INTO request_records (id, tenant_id, key_id, created_at, protocol, model, request_object, reservation_id, upstream_account_id, model_route_id, input_tokens, output_tokens, cost_micros) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, 0, 0, 0)",
        )
        .bind(&request_id)
        .bind(&tenant_id)
        .bind(&key_id)
        .bind(now)
        .bind(&request.protocol)
        .bind(&request.model)
        .bind(&request.request_object)
        .bind(&reservation_id)
        .bind(&upstream_account_id)
        .bind(&model_route_id)
        .execute(&mut *transaction)
        .await?;
        let event_id = Uuid::now_v7().to_string();
        if claim_request_event_locator(
            &mut transaction,
            &event_id,
            now,
            &tenant_id,
            &key_id,
            &request_id,
        )
        .await?
        {
            sqlx::query(
                "INSERT INTO request_events (event_id, tenant_id, key_id, request_id, event_at, event_kind, protocol, model, input_tokens, output_tokens, cost_micros) VALUES ($1, $2, $3, $4, $5, 'started', $6, $7, 0, 0, 0)",
            )
            .bind(&event_id)
            .bind(&tenant_id)
            .bind(&key_id)
            .bind(&request_id)
            .bind(now)
            .bind(&request.protocol)
            .bind(&request.model)
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    pub async fn match_session_archive_request(
        &self,
        input: SessionArchiveMatchInput<'_>,
    ) -> Result<SessionArchiveTarget, AppError> {
        let rows = sqlx::query(
            "SELECT t.id AS tenant_id, l.target_request_id, rl.created_at AS request_created_at, l.external_event_hash, l.source_created_at, l.source_model, l.source_key_hash, r.key_id, k.principal_id, k.account_id, k.alias, k.currency, k.credential_generation, k.policy_json FROM import_request_links l JOIN tenants t ON t.id = l.tenant_id JOIN request_record_locators rl ON rl.id = l.target_request_id AND rl.tenant_id = l.tenant_id JOIN request_records r ON r.id = rl.id AND r.created_at = rl.created_at AND r.tenant_id = rl.tenant_id JOIN key_records k ON k.id = r.key_id AND k.tenant_id = l.tenant_id WHERE t.external_id = $1 AND l.source = $2 AND l.external_request_id = $3 ORDER BY l.source_created_at, l.external_event_hash",
        )
        .bind(input.tenant_external_id)
        .bind(input.cpamp_source)
        .bind(input.external_request_id)
        .fetch_all(&self.pool)
        .await?;

        let archived_hashes = [input.credential_hash, input.legacy_key_id]
            .into_iter()
            .flatten()
            .map(str::trim)
            .filter(|value| is_sha256_hex(value))
            .map(|value| value.to_ascii_lowercase())
            .collect::<Vec<_>>();
        if archived_hashes.is_empty() {
            return Err(AppError::BadRequest(
                "archive request has no verified credential hash".into(),
            ));
        }

        let mut matches = Vec::new();
        for row in rows {
            let source_created_at: i64 = row.try_get("source_created_at")?;
            if source_created_at.abs_diff(input.started_at) > input.time_tolerance_ms.max(0) as u64
            {
                continue;
            }
            let source_model: String = row.try_get("source_model")?;
            let model_matches = [input.requested_model, input.resolved_model]
                .into_iter()
                .flatten()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .any(|value| value == source_model);
            if !model_matches {
                continue;
            }
            let source_key_hash: String = row.try_get("source_key_hash")?;
            if !archived_hashes
                .iter()
                .any(|value| value.eq_ignore_ascii_case(&source_key_hash))
            {
                continue;
            }
            matches.push((row, source_created_at, source_model));
        }
        if matches.len() != 1 {
            return Err(AppError::BadRequest(
                "archive request does not map uniquely to a CPAMP event".into(),
            ));
        }
        let (row, source_created_at, source_model) = matches.pop().expect("one match");
        let tenant_id = parse_uuid(row.try_get("tenant_id")?)?;
        let target_request_id = parse_uuid(row.try_get("target_request_id")?)?;
        let key_id = parse_uuid(row.try_get("key_id")?)?;
        let policy_json: String = row.try_get("policy_json")?;
        let policy = serde_json::from_str(&policy_json).map_err(|_| AppError::Internal)?;

        let imported = sqlx::query(
            "SELECT target_request_id, record_digest FROM session_archive_import_records WHERE tenant_id = $1 AND source = $2 AND external_request_id = $3",
        )
        .bind(tenant_id.to_string())
        .bind(input.archive_source)
        .bind(input.external_request_id)
        .fetch_optional(&self.pool)
        .await?;
        let replay = if let Some(existing) = imported {
            let existing_target: String = existing.try_get("target_request_id")?;
            let existing_digest: String = existing.try_get("record_digest")?;
            if existing_target != target_request_id.to_string()
                || existing_digest != input.record_digest
            {
                return Err(AppError::BadRequest(
                    "archive request changed after it was imported".into(),
                ));
            }
            true
        } else {
            false
        };

        Ok(SessionArchiveTarget {
            tenant_id,
            target_request_id,
            request_created_at: row.try_get("request_created_at")?,
            key: AuthenticatedKey {
                key_id,
                tenant_id,
                principal_id: parse_uuid(row.try_get("principal_id")?)?,
                account_id: parse_uuid(row.try_get("account_id")?)?,
                alias: row.try_get("alias")?,
                currency: row.try_get("currency")?,
                credential_generation: row.try_get("credential_generation")?,
                policy,
            },
            external_event_hash: row.try_get("external_event_hash")?,
            source_created_at,
            source_model,
            replay,
        })
    }

    pub async fn session_archive_lower_bound(
        &self,
        tenant_external_id: &str,
        archive_source: &str,
        overlap_ms: i64,
    ) -> Result<i64, AppError> {
        let row = sqlx::query(
            "SELECT c.watermark_ms FROM session_archive_import_checkpoints c JOIN tenants t ON t.id = c.tenant_id WHERE t.external_id = $1 AND c.source = $2",
        )
        .bind(tenant_external_id)
        .bind(archive_source)
        .fetch_optional(&self.pool)
        .await?;
        let watermark = row
            .map(|row| row.try_get::<i64, _>("watermark_ms"))
            .transpose()?
            .unwrap_or(0);
        Ok(watermark.saturating_sub(overlap_ms.max(0)).max(0))
    }

    pub async fn commit_session_archive_request(
        &self,
        input: SessionArchiveCommitInput<'_>,
    ) -> Result<bool, AppError> {
        if input.target.replay {
            return Ok(false);
        }

        // Refuse protected targets before creating semantic atoms or conversation
        // projections. The transactional check below is repeated to close the race.
        let protected = sqlx::query(
            "SELECT request_object, response_object FROM request_records WHERE id = $1 AND created_at = $2 AND tenant_id = $3",
        )
        .bind(input.target.target_request_id.to_string())
        .bind(input.target.request_created_at)
        .bind(input.target.tenant_id.to_string())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::NotFound)?;
        let protected_request: String = protected.try_get("request_object")?;
        let protected_response: Option<String> = protected.try_get("response_object")?;
        replacement_for_gap(&protected_request, input.request_object)?;
        if let Some(current) = protected_response.as_deref() {
            replacement_for_gap(current, input.response_object)?;
        }

        if let Some(request_json) = input.request_json {
            let existing = sqlx::query(
                "SELECT cluster_id FROM conversation_observations WHERE request_id = $1",
            )
            .bind(input.target.target_request_id.to_string())
            .fetch_optional(&self.pool)
            .await?;
            let cluster_id = if let Some(existing) = existing {
                parse_uuid(existing.try_get("cluster_id")?)?
            } else {
                self.record_conversation_observation(
                    &input.target.key,
                    input.target.target_request_id,
                    request_json,
                    input.conversation_hints,
                    input.client_name,
                )
                .await?
            };
            sqlx::query(
                "UPDATE conversation_observations SET created_at = $1 WHERE request_id = $2",
            )
            .bind(input.source_started_at)
            .bind(input.target.target_request_id.to_string())
            .execute(&self.pool)
            .await?;
            sqlx::query(
                "UPDATE conversation_edges SET created_at = $1 WHERE to_observation_id IN (SELECT id FROM conversation_observations WHERE request_id = $2)",
            )
            .bind(input.source_started_at)
            .bind(input.target.target_request_id.to_string())
            .execute(&self.pool)
            .await?;
            sqlx::query(
                "UPDATE conversation_clusters SET created_at = CASE WHEN created_at > $1 THEN $1 ELSE created_at END, updated_at = CASE WHEN updated_at < $1 THEN $1 ELSE updated_at END WHERE id = $2",
            )
            .bind(input.source_started_at)
            .bind(cluster_id.to_string())
            .execute(&self.pool)
            .await?;
        }

        let now = unix_millis();
        let mut tx = self.pool.begin().await?;
        let current = sqlx::query(
            "SELECT request_object, response_object FROM request_records WHERE id = $1 AND created_at = $2 AND tenant_id = $3",
        )
        .bind(input.target.target_request_id.to_string())
        .bind(input.target.request_created_at)
        .bind(input.target.tenant_id.to_string())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AppError::NotFound)?;
        let current_request: String = current.try_get("request_object")?;
        let current_response: Option<String> = current.try_get("response_object")?;
        let next_request = replacement_for_gap(&current_request, input.request_object)?;
        let next_response = match (current_response.as_deref(), input.response_object) {
            (Some(current), replacement) => replacement_for_gap(current, replacement)?,
            (None, Some(replacement)) => Some(replacement.to_owned()),
            (None, None) => None,
        };

        let inserted = sqlx::query(
            "INSERT INTO session_archive_import_records (tenant_id, source, external_request_id, target_request_id, external_event_hash, record_digest, request_digest, response_digest, request_object, response_object, source_started_at, imported_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) ON CONFLICT(tenant_id, source, external_request_id) DO NOTHING",
        )
        .bind(input.target.tenant_id.to_string())
        .bind(input.archive_source)
        .bind(input.external_request_id)
        .bind(input.target.target_request_id.to_string())
        .bind(&input.target.external_event_hash)
        .bind(input.record_digest)
        .bind(input.request_digest)
        .bind(input.response_digest)
        .bind(input.request_object)
        .bind(input.response_object)
        .bind(input.source_started_at)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        if inserted.rows_affected() == 0 {
            tx.rollback().await?;
            return Ok(false);
        }

        sqlx::query(
            "UPDATE request_records SET request_object = $1, response_object = $2 WHERE id = $3 AND created_at = $4 AND tenant_id = $5",
        )
        .bind(next_request.unwrap_or(current_request))
        .bind(next_response)
        .bind(input.target.target_request_id.to_string())
        .bind(input.target.request_created_at)
        .bind(input.target.tenant_id.to_string())
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO session_archive_import_checkpoints (tenant_id, source, watermark_ms, watermark_request_id, imported_records, updated_at) VALUES ($1, $2, $3, $4, 1, $5) ON CONFLICT(tenant_id, source) DO UPDATE SET watermark_ms = CASE WHEN excluded.watermark_ms > session_archive_import_checkpoints.watermark_ms THEN excluded.watermark_ms ELSE session_archive_import_checkpoints.watermark_ms END, watermark_request_id = CASE WHEN excluded.watermark_ms > session_archive_import_checkpoints.watermark_ms OR (excluded.watermark_ms = session_archive_import_checkpoints.watermark_ms AND excluded.watermark_request_id > session_archive_import_checkpoints.watermark_request_id) THEN excluded.watermark_request_id ELSE session_archive_import_checkpoints.watermark_request_id END, imported_records = session_archive_import_checkpoints.imported_records + 1, updated_at = excluded.updated_at",
        )
        .bind(input.target.tenant_id.to_string())
        .bind(input.archive_source)
        .bind(input.source_started_at)
        .bind(input.external_request_id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn record_conversation_observation(
        &self,
        key: &AuthenticatedKey,
        request_id: Uuid,
        request_json: &serde_json::Value,
        hints: &ConversationHints,
        client_name: Option<&str>,
    ) -> Result<Uuid, AppError> {
        let atoms = extract_atoms(request_json);
        let nodes = build_prefix(&atoms);
        let atom_hashes: Vec<_> = atoms.iter().map(|atom| atom.content_hash.clone()).collect();
        let atom_hashes_json =
            serde_json::to_string(&atom_hashes).map_err(|_| AppError::Internal)?;
        let leaf = nodes.last().map(|node| node.node_hash.clone());
        let now = unix_millis();
        let observation_id = Uuid::now_v7();
        let mut tx = self.pool.begin().await?;
        let request_id = request_id.to_string();
        let locator = sqlx::query(
            "SELECT created_at, tenant_id, key_id FROM request_record_locators WHERE id = $1",
        )
        .bind(&request_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AppError::NotFound)?;
        let request_created_at: i64 = locator.try_get("created_at")?;
        if locator.try_get::<String, _>("tenant_id")? != key.tenant_id.to_string()
            || locator.try_get::<String, _>("key_id")? != key.key_id.to_string()
        {
            return Err(AppError::NotFound);
        }

        for atom in &atoms {
            sqlx::query(
                "INSERT INTO semantic_atoms (tenant_id, content_hash, instance_hash, role, kind, content_json, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT(tenant_id, content_hash) DO NOTHING",
            )
            .bind(key.tenant_id.to_string())
            .bind(&atom.content_hash)
            .bind(&atom.instance_hash)
            .bind(&atom.role)
            .bind(&atom.kind)
            .bind(serde_json::to_string(&atom.content).map_err(|_| AppError::Internal)?)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }
        for node in &nodes {
            sqlx::query(
                "INSERT INTO context_nodes (tenant_id, node_hash, parent_hash, atom_hash, depth, created_at) VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT(tenant_id, node_hash) DO NOTHING",
            )
            .bind(key.tenant_id.to_string())
            .bind(&node.node_hash)
            .bind(&node.parent_hash)
            .bind(&node.atom_hash)
            .bind(node.depth as i64)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }

        let tenant_id = key.tenant_id.to_string();
        let principal_id = key.principal_id.to_string();
        let mut candidates = if hints.parent_turn_id.is_some()
            || hints.turn_id.is_some()
            || hints.session_id.is_some()
        {
            sqlx::query(
                "SELECT o.id, o.cluster_id, o.atom_hashes_json, o.explicit_session_id, o.turn_id, o.upstream_response_id, o.branch_id, o.client_name, o.created_at FROM conversation_observations o JOIN conversation_clusters c ON c.id = o.cluster_id WHERE c.tenant_id = $1 AND c.principal_id = $2 AND (($3 IS NOT NULL AND (o.turn_id = $3 OR o.upstream_response_id = $3)) OR ($4 IS NOT NULL AND o.turn_id = $4) OR ($5 IS NOT NULL AND o.explicit_session_id = $5)) ORDER BY CASE WHEN $3 IS NOT NULL AND (o.turn_id = $3 OR o.upstream_response_id = $3) THEN 0 WHEN $4 IS NOT NULL AND o.turn_id = $4 THEN 1 ELSE 2 END, o.created_at DESC LIMIT 50",
            )
            .bind(&tenant_id)
            .bind(&principal_id)
            .bind(hints.parent_turn_id.as_deref())
            .bind(hints.turn_id.as_deref())
            .bind(hints.session_id.as_deref())
            .fetch_all(&mut *tx)
            .await?
        } else {
            Vec::new()
        };
        let recent_candidates = sqlx::query(
            "SELECT o.id, o.cluster_id, o.atom_hashes_json, o.explicit_session_id, o.turn_id, o.upstream_response_id, o.branch_id, o.client_name, o.created_at FROM conversation_observations o JOIN conversation_clusters c ON c.id = o.cluster_id WHERE c.tenant_id = $1 AND c.principal_id = $2 ORDER BY o.created_at DESC LIMIT 50",
        )
        .bind(&tenant_id)
        .bind(&principal_id)
        .fetch_all(&mut *tx)
        .await?;
        for recent in recent_candidates {
            let recent_id: String = recent.try_get("id")?;
            let duplicate = candidates.iter().any(|candidate| {
                candidate
                    .try_get::<String, _>("id")
                    .is_ok_and(|candidate_id| candidate_id == recent_id)
            });
            if !duplicate {
                candidates.push(recent);
            }
        }

        let mut selected: Option<ConversationSelection> = None;
        for row in candidates {
            let candidate_session: Option<String> = row.try_get("explicit_session_id")?;
            let candidate_turn: Option<String> = row.try_get("turn_id")?;
            let candidate_response: Option<String> = row.try_get("upstream_response_id")?;
            let candidate_branch: Option<String> = row.try_get("branch_id")?;
            let candidate_client: Option<String> = row.try_get("client_name")?;
            let previous_hashes_json: String = row.try_get("atom_hashes_json")?;
            let previous_hashes: Vec<String> =
                serde_json::from_str(&previous_hashes_json).unwrap_or_default();
            let (relation, confidence) = infer_hash_relation(&previous_hashes, &atom_hashes);
            let created_at: i64 = row.try_get("created_at")?;
            let direct_parent = hints.parent_turn_id.is_some()
                && (hints.parent_turn_id.as_deref() == candidate_turn.as_deref()
                    || hints.parent_turn_id.as_deref() == candidate_response.as_deref());
            let same_turn =
                hints.turn_id.is_some() && hints.turn_id.as_deref() == candidate_turn.as_deref();
            let explicit_match = hints.session_id.is_some()
                && hints.session_id.as_deref() == candidate_session.as_deref();
            let conflicting_sessions =
                hints.session_id.is_some() && candidate_session.is_some() && !explicit_match;
            let exact_prefix = confidence >= 700;
            let _recent_candidate = now.saturating_sub(created_at) <= 30 * 60 * 1_000;
            let same_client = client_name.is_some() && client_name == candidate_client.as_deref();
            if direct_parent
                || same_turn
                || explicit_match
                || (exact_prefix && !conflicting_sessions)
            {
                let branch_changed = hints.branch_id.is_some()
                    && candidate_branch.is_some()
                    && hints.branch_id.as_deref() != candidate_branch.as_deref();
                let relation = if same_turn {
                    RelationKind::Retry
                } else if hints.compaction
                    || (explicit_match && atom_hashes.len() * 2 < previous_hashes.len())
                {
                    RelationKind::Compacts
                } else if direct_parent && branch_changed {
                    RelationKind::Branch
                } else if direct_parent {
                    RelationKind::Continues
                } else {
                    relation
                };
                let confidence = if direct_parent || same_turn {
                    995
                } else if explicit_match {
                    confidence.max(990)
                } else {
                    confidence
                };
                selected = Some(ConversationSelection {
                    observation_id: row.try_get("id")?,
                    cluster_id: row.try_get("cluster_id")?,
                    relation,
                    confidence,
                    direct_parent,
                    same_turn,
                    semantic_prefix: exact_prefix,
                    client_match: same_client,
                    // A durable session id is sufficient to place observations in one
                    // cluster, but it does not prove that two adjacent requests are a
                    // continuation. Persist a directed edge only when the protocol names
                    // the parent/turn, the payload establishes a Merkle-prefix relation,
                    // or the client explicitly marks a compaction.
                    write_edge: direct_parent || same_turn || exact_prefix || hints.compaction,
                });
                if direct_parent || same_turn || explicit_match || exact_prefix {
                    break;
                }
            }
        }

        let cluster_id = if let Some(selection) = &selected {
            parse_uuid(selection.cluster_id.clone())?
        } else {
            let id = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO conversation_clusters (id, tenant_id, principal_id, explicit_session_id, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(id.to_string())
            .bind(key.tenant_id.to_string())
            .bind(key.principal_id.to_string())
            .bind(hints.session_id.as_deref())
            .bind(now)
            .bind(now)
            .execute(&mut *tx)
            .await?;
            id
        };

        sqlx::query(
            "INSERT INTO conversation_observations (id, cluster_id, request_id, key_id, leaf_node_hash, atom_hashes_json, explicit_session_id, client_name, created_at, inference_version, turn_id, parent_turn_id, branch_id, compaction) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 2, $10, $11, $12, $13)",
        )
        .bind(observation_id.to_string())
        .bind(cluster_id.to_string())
        .bind(request_id.to_string())
        .bind(key.key_id.to_string())
        .bind(leaf)
        .bind(atom_hashes_json)
        .bind(hints.session_id.as_deref())
        .bind(client_name)
        .bind(now)
        .bind(hints.turn_id.as_deref())
        .bind(hints.parent_turn_id.as_deref())
        .bind(hints.branch_id.as_deref())
        .bind(i64::from(hints.compaction))
        .execute(&mut *tx)
        .await?;

        if let Some(selection) = selected.filter(|selection| selection.write_edge) {
            sqlx::query(
                "INSERT INTO conversation_edges (id, cluster_id, from_observation_id, to_observation_id, relation_kind, confidence_millis, evidence_json, pinned, inference_version, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7, 0, 2, $8)",
            )
            .bind(Uuid::now_v7().to_string())
            .bind(cluster_id.to_string())
            .bind(selection.observation_id)
            .bind(observation_id.to_string())
            .bind(relation_name(selection.relation))
            .bind(selection.confidence)
            .bind(serde_json::json!({
                "explicit_session": hints.session_id.is_some(),
                "explicit_parent": selection.direct_parent,
                "same_turn": selection.same_turn,
                "branch": hints.branch_id.is_some(),
                "compaction": hints.compaction,
                "semantic_prefix": selection.semantic_prefix,
                "client_match": selection.client_match,
                "inference_version": 2
            }).to_string())
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query("UPDATE conversation_clusters SET updated_at = $1, explicit_session_id = COALESCE(explicit_session_id, $2) WHERE id = $3")
            .bind(now)
            .bind(hints.session_id.as_deref())
            .bind(cluster_id.to_string())
            .execute(&mut *tx)
            .await?;
        let attached = sqlx::query(
            "UPDATE request_records SET conversation_cluster_id = $1 WHERE id = $2 AND created_at = $3",
        )
            .bind(cluster_id.to_string())
            .bind(&request_id)
            .bind(request_created_at)
            .execute(&mut *tx)
            .await?;
        if attached.rows_affected() != 1 {
            return Err(AppError::Internal);
        }
        tx.commit().await?;
        Ok(cluster_id)
    }

    pub async fn attach_conversation_upstream_response(
        &self,
        request_id: Uuid,
        upstream_response_id: &str,
    ) -> Result<(), AppError> {
        let upstream_response_id = upstream_response_id.trim();
        if upstream_response_id.is_empty()
            || upstream_response_id.len() > 256
            || upstream_response_id.chars().any(char::is_control)
        {
            return Err(AppError::BadRequest(
                "upstream response id must contain at most 256 non-control characters".into(),
            ));
        }
        sqlx::query(
            "UPDATE conversation_observations SET upstream_response_id = $1 WHERE request_id = $2",
        )
        .bind(upstream_response_id)
        .bind(request_id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn record_request_finished(&self, request: FinishRequest) -> Result<(), AppError> {
        let mut tx = self.pool.begin().await?;
        let completed_at = unix_millis();
        let request_id = request.request_id.to_string();
        let locator = sqlx::query(
            "SELECT created_at, tenant_id, key_id FROM request_record_locators WHERE id = $1",
        )
        .bind(&request_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(locator) = locator else {
            tx.commit().await?;
            return Ok(());
        };
        let created_at: i64 = locator.try_get("created_at")?;
        let tenant_id: String = locator.try_get("tenant_id")?;
        let key_id: String = locator.try_get("key_id")?;
        let updated = sqlx::query(
            "UPDATE request_records SET status_code = $1, duration_ms = $2, input_tokens = $3, output_tokens = $4, cost_micros = $5, error_code = $6, response_object = $7, completed_at = $8 WHERE id = $9 AND created_at = $10 AND completed_at IS NULL",
        )
        .bind(request.status_code)
        .bind(request.duration_ms)
        .bind(request.input_tokens)
        .bind(request.output_tokens)
        .bind(request.cost_micros)
        .bind(&request.error_code)
        .bind(&request.response_object)
        .bind(completed_at)
        .bind(&request_id)
        .bind(created_at)
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() == 0 {
            tx.commit().await?;
            return Ok(());
        }
        sqlx::query(
            "INSERT INTO usage_daily_aggregates (key_id, day_bucket, model, status_class, error_code, requests, input_tokens, output_tokens, cost_micros) SELECT key_id, created_at / 86400000, model, CASE WHEN status_code >= 200 AND status_code < 400 THEN 'success' ELSE 'failure' END, COALESCE(error_code, ''), 1, input_tokens, output_tokens, cost_micros FROM request_records WHERE id = $1 AND created_at = $2 ON CONFLICT(key_id, day_bucket, model, status_class, error_code) DO UPDATE SET requests = usage_daily_aggregates.requests + 1, input_tokens = usage_daily_aggregates.input_tokens + excluded.input_tokens, output_tokens = usage_daily_aggregates.output_tokens + excluded.output_tokens, cost_micros = usage_daily_aggregates.cost_micros + excluded.cost_micros",
        )
        .bind(&request_id)
        .bind(created_at)
        .execute(&mut *tx)
        .await?;
        let event_id = Uuid::now_v7().to_string();
        if claim_request_event_locator(
            &mut tx,
            &event_id,
            completed_at,
            &tenant_id,
            &key_id,
            &request_id,
        )
        .await?
        {
            sqlx::query(
                "INSERT INTO request_events (event_id, tenant_id, key_id, request_id, event_at, event_kind, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, error_code) SELECT $1, tenant_id, key_id, id, $2, 'finished', protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, error_code FROM request_records WHERE id = $3 AND created_at = $4",
            )
            .bind(&event_id)
            .bind(completed_at)
            .bind(&request_id)
            .bind(created_at)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn request_events_after(
        &self,
        tenant_external_id: &str,
        after_event_at: i64,
        after_event_id: Option<Uuid>,
        limit: i64,
    ) -> Result<Vec<RequestEventView>, AppError> {
        let after_event_id = after_event_id
            .map(|event_id| event_id.to_string())
            .unwrap_or_default();
        let rows = sqlx::query(
            "SELECT e.event_id, e.request_id, e.event_at, e.event_kind, e.key_id, e.protocol, e.model, e.status_code, e.duration_ms, e.input_tokens, e.output_tokens, e.cost_micros, e.error_code FROM request_events e JOIN tenants t ON t.id = e.tenant_id WHERE t.external_id = $1 AND (e.event_at > $2 OR (e.event_at = $3 AND e.event_id > $4)) ORDER BY e.event_at ASC, e.event_id ASC LIMIT $5",
        )
        .bind(tenant_external_id)
        .bind(after_event_at)
        .bind(after_event_at)
        .bind(after_event_id)
        .bind(limit.clamp(1, 500))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(RequestEventView {
                    event_id: parse_uuid(row.try_get("event_id")?)?,
                    request_id: parse_uuid(row.try_get("request_id")?)?,
                    event_at: row.try_get("event_at")?,
                    event_kind: row.try_get("event_kind")?,
                    key_id: parse_uuid(row.try_get("key_id")?)?,
                    protocol: row.try_get("protocol")?,
                    model: row.try_get("model")?,
                    status_code: row.try_get("status_code")?,
                    duration_ms: row.try_get("duration_ms")?,
                    input_tokens: row.try_get("input_tokens")?,
                    output_tokens: row.try_get("output_tokens")?,
                    cost: micros_to_decimal_string(row.try_get("cost_micros")?),
                    error_code: row.try_get("error_code")?,
                })
            })
            .collect()
    }

    pub async fn all_request_events_after(
        &self,
        after_event_at: i64,
        after_event_id: Option<Uuid>,
        limit: i64,
    ) -> Result<Vec<RequestEventView>, AppError> {
        let after_event_id = after_event_id
            .map(|event_id| event_id.to_string())
            .unwrap_or_default();
        let rows = sqlx::query(
            "SELECT event_id, request_id, event_at, event_kind, key_id, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, error_code FROM request_events WHERE (event_at > $1 OR (event_at = $2 AND event_id > $3)) ORDER BY event_at ASC, event_id ASC LIMIT $4",
        )
        .bind(after_event_at)
        .bind(after_event_at)
        .bind(after_event_id)
        .bind(limit.clamp(1, 500))
        .fetch_all(&self.pool)
        .await?;
        request_event_views(rows)
    }

    pub async fn list_requests(
        &self,
        key_id: Uuid,
        limit: i64,
    ) -> Result<Vec<RequestView>, AppError> {
        self.list_requests_filtered(
            key_id,
            RequestListFilter {
                limit,
                ..RequestListFilter::default()
            },
        )
        .await
    }

    pub async fn list_requests_filtered(
        &self,
        key_id: Uuid,
        filter: RequestListFilter,
    ) -> Result<Vec<RequestView>, AppError> {
        validate_request_filter(&filter)?;
        let rows = sqlx::query(
            "SELECT id, created_at, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, error_code FROM request_records WHERE key_id = $1 AND created_at >= $2 AND created_at <= $3 AND (created_at < $4 OR (created_at = $4 AND id < $5)) AND ($6 = '' OR model = $6) AND ($7 = '' OR protocol = $7) AND ($8 = '' OR ($8 = 'success' AND status_code BETWEEN 200 AND 399) OR ($8 = 'error' AND status_code >= 400) OR ($8 = 'pending' AND status_code IS NULL)) AND ($9 = '' OR error_code = $9) AND ($10 = '' OR upstream_account_id = $10) AND ($11 = '' OR model_route_id = $11) AND ($12 < 0 OR duration_ms >= $12) AND ($13 < 0 OR duration_ms <= $13) AND ($14 < 0 OR cost_micros >= $14) AND ($15 < 0 OR cost_micros <= $15) ORDER BY created_at DESC, id DESC LIMIT $16",
        )
        .bind(key_id.to_string())
        .bind(filter.from_created_at.unwrap_or(0))
        .bind(filter.to_created_at.unwrap_or(i64::MAX))
        .bind(filter.before_created_at.unwrap_or(i64::MAX))
        .bind(cursor_id(&filter))
        .bind(filter.model.as_deref().unwrap_or_default())
        .bind(filter.protocol.as_deref().unwrap_or_default())
        .bind(filter.status.as_deref().unwrap_or_default())
        .bind(filter.error_code.as_deref().unwrap_or_default())
        .bind(
            filter
                .upstream_account_id
                .map(|id| id.to_string())
                .unwrap_or_default(),
        )
        .bind(filter.route_id.map(|id| id.to_string()).unwrap_or_default())
        .bind(filter.min_duration_ms.unwrap_or(-1))
        .bind(filter.max_duration_ms.unwrap_or(-1))
        .bind(filter.min_cost_micros.unwrap_or(-1))
        .bind(filter.max_cost_micros.unwrap_or(-1))
        .bind(filter.limit.clamp(1, 100))
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|row| {
                Ok(RequestView {
                    request_id: parse_uuid(row.try_get("id")?)?,
                    created_at: row.try_get("created_at")?,
                    protocol: row.try_get("protocol")?,
                    model: row.try_get("model")?,
                    status_code: row.try_get("status_code")?,
                    duration_ms: row.try_get("duration_ms")?,
                    input_tokens: row.try_get("input_tokens")?,
                    output_tokens: row.try_get("output_tokens")?,
                    cost: micros_to_decimal_string(row.try_get("cost_micros")?),
                    error_code: row.try_get("error_code")?,
                })
            })
            .collect()
    }

    pub async fn list_all_requests(
        &self,
        tenant_external_id: &str,
        limit: i64,
    ) -> Result<Vec<RequestView>, AppError> {
        self.list_all_requests_filtered(
            tenant_external_id,
            RequestListFilter {
                limit,
                ..RequestListFilter::default()
            },
        )
        .await
    }

    pub async fn list_all_requests_filtered(
        &self,
        tenant_external_id: &str,
        filter: RequestListFilter,
    ) -> Result<Vec<RequestView>, AppError> {
        validate_request_filter(&filter)?;
        let rows = sqlx::query(
            "SELECT id, created_at, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, error_code FROM (SELECT r.id, r.created_at, r.protocol, r.model, r.status_code, r.duration_ms, r.input_tokens, r.output_tokens, r.cost_micros, r.error_code FROM request_records r JOIN key_records k ON k.id = r.key_id AND k.tenant_id = r.tenant_id JOIN principals p ON p.id = k.principal_id AND p.tenant_id = k.tenant_id JOIN tenants t ON t.id = r.tenant_id WHERE t.external_id = $1 AND r.created_at >= $2 AND r.created_at <= $3 AND (r.created_at < $4 OR (r.created_at = $4 AND r.id < $5)) AND ($6 = '' OR r.key_id = $6) AND ($7 = '' OR r.model = $7) AND ($8 = '' OR r.protocol = $8) AND ($9 = '' OR ($9 = 'success' AND r.status_code BETWEEN 200 AND 399) OR ($9 = 'error' AND r.status_code >= 400) OR ($9 = 'pending' AND r.status_code IS NULL)) AND ($10 = '' OR r.error_code = $10) AND ($11 = '' OR r.upstream_account_id = $11) AND ($12 = '' OR r.model_route_id = $12) AND ($13 < 0 OR r.duration_ms >= $13) AND ($14 < 0 OR r.duration_ms <= $14) AND ($15 < 0 OR r.cost_micros >= $15) AND ($16 < 0 OR r.cost_micros <= $16) AND ($17 = '' OR LOWER(k.alias) LIKE $17 ESCAPE '\\') AND ($18 = '' OR LOWER(p.external_id) LIKE $18 ESCAPE '\\') UNION ALL SELECT g.id, g.created_at, 'generation' AS protocol, g.public_model AS model, CASE WHEN g.status = 'succeeded' THEN 200 WHEN g.status IN ('failed', 'cancelled') THEN 502 ELSE NULL END AS status_code, CASE WHEN g.completed_at IS NULL THEN NULL ELSE g.completed_at - g.created_at END AS duration_ms, 0 AS input_tokens, 0 AS output_tokens, g.cost_micros, g.error_code FROM generation_jobs g JOIN key_records k ON k.id = g.key_id AND k.tenant_id = g.tenant_id JOIN principals p ON p.id = k.principal_id AND p.tenant_id = k.tenant_id JOIN tenants t ON t.id = g.tenant_id WHERE t.external_id = $1 AND g.created_at >= $2 AND g.created_at <= $3 AND (g.created_at < $4 OR (g.created_at = $4 AND g.id < $5)) AND ($6 = '' OR g.key_id = $6) AND ($7 = '' OR g.public_model = $7) AND ($8 = '' OR $8 = 'generation') AND ($9 = '' OR ($9 = 'success' AND g.status = 'succeeded') OR ($9 = 'error' AND g.status IN ('failed', 'cancelled')) OR ($9 = 'pending' AND g.status IN ('queued', 'running'))) AND ($10 = '' OR g.error_code = $10) AND ($11 = '' OR g.upstream_account_id = $11) AND $12 = '' AND ($13 < 0 OR (g.completed_at - g.created_at) >= $13) AND ($14 < 0 OR (g.completed_at - g.created_at) <= $14) AND ($15 < 0 OR g.cost_micros >= $15) AND ($16 < 0 OR g.cost_micros <= $16) AND ($17 = '' OR LOWER(k.alias) LIKE $17 ESCAPE '\\') AND ($18 = '' OR LOWER(p.external_id) LIKE $18 ESCAPE '\\')) AS all_requests ORDER BY created_at DESC, id DESC LIMIT $19",
        )
        .bind(tenant_external_id)
        .bind(filter.from_created_at.unwrap_or(0))
        .bind(filter.to_created_at.unwrap_or(i64::MAX))
        .bind(filter.before_created_at.unwrap_or(i64::MAX))
        .bind(cursor_id(&filter))
        .bind(filter.key_id.map(|id| id.to_string()).unwrap_or_default())
        .bind(filter.model.as_deref().unwrap_or_default())
        .bind(filter.protocol.as_deref().unwrap_or_default())
        .bind(filter.status.as_deref().unwrap_or_default())
        .bind(filter.error_code.as_deref().unwrap_or_default())
        .bind(
            filter
                .upstream_account_id
                .map(|id| id.to_string())
                .unwrap_or_default(),
        )
        .bind(filter.route_id.map(|id| id.to_string()).unwrap_or_default())
        .bind(filter.min_duration_ms.unwrap_or(-1))
        .bind(filter.max_duration_ms.unwrap_or(-1))
        .bind(filter.min_cost_micros.unwrap_or(-1))
        .bind(filter.max_cost_micros.unwrap_or(-1))
        .bind(search_prefix(filter.key_alias.as_deref()))
        .bind(search_prefix(filter.principal.as_deref()))
        .bind(filter.limit.clamp(1, 500))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(RequestView {
                    request_id: parse_uuid(row.try_get("id")?)?,
                    created_at: row.try_get("created_at")?,
                    protocol: row.try_get("protocol")?,
                    model: row.try_get("model")?,
                    status_code: row.try_get("status_code")?,
                    duration_ms: row.try_get("duration_ms")?,
                    input_tokens: row.try_get("input_tokens")?,
                    output_tokens: row.try_get("output_tokens")?,
                    cost: micros_to_decimal_string(row.try_get("cost_micros")?),
                    error_code: row.try_get("error_code")?,
                })
            })
            .collect()
    }

    pub async fn list_global_requests(&self, limit: i64) -> Result<Vec<RequestView>, AppError> {
        self.list_global_requests_filtered(RequestListFilter {
            limit,
            ..RequestListFilter::default()
        })
        .await
    }

    pub async fn list_global_requests_filtered(
        &self,
        filter: RequestListFilter,
    ) -> Result<Vec<RequestView>, AppError> {
        validate_request_filter(&filter)?;
        let rows = sqlx::query(
            "SELECT id, created_at, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, error_code FROM (SELECT r.id, r.created_at, r.protocol, r.model, r.status_code, r.duration_ms, r.input_tokens, r.output_tokens, r.cost_micros, r.error_code FROM request_records r JOIN key_records k ON k.id = r.key_id AND k.tenant_id = r.tenant_id JOIN principals p ON p.id = k.principal_id AND p.tenant_id = k.tenant_id WHERE r.created_at >= $1 AND r.created_at <= $2 AND (r.created_at < $3 OR (r.created_at = $3 AND r.id < $4)) AND ($5 = '' OR r.key_id = $5) AND ($6 = '' OR r.model = $6) AND ($7 = '' OR r.protocol = $7) AND ($8 = '' OR ($8 = 'success' AND r.status_code BETWEEN 200 AND 399) OR ($8 = 'error' AND r.status_code >= 400) OR ($8 = 'pending' AND r.status_code IS NULL)) AND ($9 = '' OR r.error_code = $9) AND ($10 = '' OR r.upstream_account_id = $10) AND ($11 = '' OR r.model_route_id = $11) AND ($12 < 0 OR r.duration_ms >= $12) AND ($13 < 0 OR r.duration_ms <= $13) AND ($14 < 0 OR r.cost_micros >= $14) AND ($15 < 0 OR r.cost_micros <= $15) AND ($16 = '' OR LOWER(k.alias) LIKE $16 ESCAPE '\\') AND ($17 = '' OR LOWER(p.external_id) LIKE $17 ESCAPE '\\') UNION ALL SELECT g.id, g.created_at, 'generation' AS protocol, g.public_model AS model, CASE WHEN g.status = 'succeeded' THEN 200 WHEN g.status IN ('failed', 'cancelled') THEN 502 ELSE NULL END AS status_code, CASE WHEN g.completed_at IS NULL THEN NULL ELSE g.completed_at - g.created_at END AS duration_ms, 0 AS input_tokens, 0 AS output_tokens, g.cost_micros, g.error_code FROM generation_jobs g JOIN key_records k ON k.id = g.key_id AND k.tenant_id = g.tenant_id JOIN principals p ON p.id = k.principal_id AND p.tenant_id = k.tenant_id WHERE g.created_at >= $1 AND g.created_at <= $2 AND (g.created_at < $3 OR (g.created_at = $3 AND g.id < $4)) AND ($5 = '' OR g.key_id = $5) AND ($6 = '' OR g.public_model = $6) AND ($7 = '' OR $7 = 'generation') AND ($8 = '' OR ($8 = 'success' AND g.status = 'succeeded') OR ($8 = 'error' AND g.status IN ('failed', 'cancelled')) OR ($8 = 'pending' AND g.status IN ('queued', 'running'))) AND ($9 = '' OR g.error_code = $9) AND ($10 = '' OR g.upstream_account_id = $10) AND $11 = '' AND ($12 < 0 OR (g.completed_at - g.created_at) >= $12) AND ($13 < 0 OR (g.completed_at - g.created_at) <= $13) AND ($14 < 0 OR g.cost_micros >= $14) AND ($15 < 0 OR g.cost_micros <= $15) AND ($16 = '' OR LOWER(k.alias) LIKE $16 ESCAPE '\\') AND ($17 = '' OR LOWER(p.external_id) LIKE $17 ESCAPE '\\')) AS all_requests ORDER BY created_at DESC, id DESC LIMIT $18",
        )
        .bind(filter.from_created_at.unwrap_or(0))
        .bind(filter.to_created_at.unwrap_or(i64::MAX))
        .bind(filter.before_created_at.unwrap_or(i64::MAX))
        .bind(cursor_id(&filter))
        .bind(filter.key_id.map(|id| id.to_string()).unwrap_or_default())
        .bind(filter.model.as_deref().unwrap_or_default())
        .bind(filter.protocol.as_deref().unwrap_or_default())
        .bind(filter.status.as_deref().unwrap_or_default())
        .bind(filter.error_code.as_deref().unwrap_or_default())
        .bind(
            filter
                .upstream_account_id
                .map(|id| id.to_string())
                .unwrap_or_default(),
        )
        .bind(filter.route_id.map(|id| id.to_string()).unwrap_or_default())
        .bind(filter.min_duration_ms.unwrap_or(-1))
        .bind(filter.max_duration_ms.unwrap_or(-1))
        .bind(filter.min_cost_micros.unwrap_or(-1))
        .bind(filter.max_cost_micros.unwrap_or(-1))
        .bind(search_prefix(filter.key_alias.as_deref()))
        .bind(search_prefix(filter.principal.as_deref()))
        .bind(filter.limit.clamp(1, 500))
        .fetch_all(&self.pool)
        .await?;
        request_views(rows)
    }

    pub async fn request_archive_refs(
        &self,
        key_id: Uuid,
        request_id: Uuid,
    ) -> Result<RequestArchiveRefs, AppError> {
        let request_id = request_id.to_string();
        let locator = self.request_record_locator(&request_id).await?;
        if let Some(locator) = locator.filter(|locator| locator.key_id == key_id.to_string()) {
            let row = sqlx::query(
                "SELECT id, created_at, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, error_code, request_object, response_object FROM request_records WHERE id = $1 AND created_at = $2 AND key_id = $3",
            )
            .bind(&request_id)
            .bind(locator.created_at)
            .bind(&locator.key_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(AppError::Internal)?;
            request_archive_refs_from_row(row)
        } else {
            self.generation_archive_refs(key_id, parse_uuid(request_id)?)
                .await
        }
    }

    pub async fn request_archive_refs_for_tenant(
        &self,
        tenant_external_id: &str,
        request_id: Uuid,
    ) -> Result<RequestArchiveRefs, AppError> {
        let request_id_string = request_id.to_string();
        let locator = self.request_record_locator(&request_id_string).await?;
        if let Some(locator) = locator {
            let row = sqlx::query(
                "SELECT r.id, r.created_at, r.protocol, r.model, r.status_code, r.duration_ms, r.input_tokens, r.output_tokens, r.cost_micros, r.error_code, r.request_object, r.response_object FROM request_records r JOIN tenants t ON t.id = $3 WHERE r.id = $1 AND r.created_at = $2 AND r.tenant_id = $3 AND t.external_id = $4",
            )
            .bind(&request_id_string)
            .bind(locator.created_at)
            .bind(&locator.tenant_id)
            .bind(tenant_external_id)
            .fetch_optional(&self.pool)
            .await?;
            if let Some(row) = row {
                return request_archive_refs_from_row(row);
            }
        }
        let row = sqlx::query(
            "SELECT g.id, g.created_at, g.completed_at, g.public_model, g.status, g.cost_micros, g.error_code, g.request_object, g.result_json FROM generation_jobs g JOIN tenants t ON t.id = g.tenant_id WHERE g.id = $1 AND t.external_id = $2",
        )
        .bind(&request_id_string)
        .bind(tenant_external_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::NotFound)?;
        generation_archive_refs_from_row(row)
    }

    pub async fn request_archive_refs_global(
        &self,
        request_id: Uuid,
    ) -> Result<RequestArchiveRefs, AppError> {
        let request_id = request_id.to_string();
        if let Some(locator) = self.request_record_locator(&request_id).await? {
            let row = sqlx::query(
                "SELECT id, created_at, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, error_code, request_object, response_object FROM request_records WHERE id = $1 AND created_at = $2",
            )
            .bind(&request_id)
            .bind(locator.created_at)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(AppError::Internal)?;
            return request_archive_refs_from_row(row);
        }
        let row = sqlx::query(
            "SELECT id, created_at, completed_at, public_model, status, cost_micros, error_code, request_object, result_json FROM generation_jobs WHERE id = $1",
        )
        .bind(&request_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::NotFound)?;
        generation_archive_refs_from_row(row)
    }

    async fn request_record_locator(
        &self,
        request_id: &str,
    ) -> Result<Option<RequestRecordLocator>, AppError> {
        sqlx::query(
            "SELECT created_at, tenant_id, key_id FROM request_record_locators WHERE id = $1",
        )
        .bind(request_id)
        .fetch_optional(&self.pool)
        .await?
        .map(|row| {
            Ok(RequestRecordLocator {
                created_at: row.try_get("created_at")?,
                tenant_id: row.try_get("tenant_id")?,
                key_id: row.try_get("key_id")?,
            })
        })
        .transpose()
    }

    async fn generation_archive_refs(
        &self,
        key_id: Uuid,
        request_id: Uuid,
    ) -> Result<RequestArchiveRefs, AppError> {
        let row = sqlx::query(
            "SELECT id, created_at, completed_at, public_model, status, cost_micros, error_code, request_object, result_json FROM generation_jobs WHERE id = $1 AND key_id = $2",
        )
        .bind(request_id.to_string())
        .bind(key_id.to_string())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::NotFound)?;
        generation_archive_refs_from_row(row)
    }

    pub async fn conversation_clusters(
        &self,
        key_id: Uuid,
    ) -> Result<Vec<ConversationClusterView>, AppError> {
        let rows = sqlx::query(
            "SELECT c.id, c.explicit_session_id, c.updated_at, (SELECT COUNT(*) FROM conversation_observations count_o WHERE count_o.cluster_id = c.id AND count_o.key_id = $1) AS request_count, (SELECT COUNT(*) FROM conversation_edges e JOIN conversation_observations target_o ON target_o.id = e.to_observation_id WHERE e.cluster_id = c.id AND target_o.key_id = $2 AND e.relation_kind = 'candidate') AS candidate_edge_count FROM conversation_clusters c WHERE EXISTS (SELECT 1 FROM conversation_observations own_o WHERE own_o.cluster_id = c.id AND own_o.key_id = $3) ORDER BY c.updated_at DESC",
        )
        .bind(key_id.to_string())
        .bind(key_id.to_string())
        .bind(key_id.to_string())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(ConversationClusterView {
                    cluster_id: parse_uuid(row.try_get("id")?)?,
                    explicit_session_id: row.try_get("explicit_session_id")?,
                    updated_at: row.try_get("updated_at")?,
                    request_count: row.try_get("request_count")?,
                    candidate_edge_count: row.try_get("candidate_edge_count")?,
                })
            })
            .collect()
    }

    pub async fn conversation_cluster_detail(
        &self,
        key_id: Uuid,
        cluster_id: Uuid,
    ) -> Result<ConversationClusterDetail, AppError> {
        let cluster = self
            .conversation_clusters(key_id)
            .await?
            .into_iter()
            .find(|cluster| cluster.cluster_id == cluster_id)
            .ok_or(AppError::NotFound)?;
        let request_rows = sqlx::query(
            "SELECT id, created_at, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, error_code FROM request_records WHERE key_id = $1 AND conversation_cluster_id = $2 ORDER BY created_at ASC, id ASC",
        )
        .bind(key_id.to_string())
        .bind(cluster_id.to_string())
        .fetch_all(&self.pool)
        .await?;
        let requests = request_rows
            .into_iter()
            .map(|row| {
                Ok(RequestView {
                    request_id: parse_uuid(row.try_get("id")?)?,
                    created_at: row.try_get("created_at")?,
                    protocol: row.try_get("protocol")?,
                    model: row.try_get("model")?,
                    status_code: row.try_get("status_code")?,
                    duration_ms: row.try_get("duration_ms")?,
                    input_tokens: row.try_get("input_tokens")?,
                    output_tokens: row.try_get("output_tokens")?,
                    cost: micros_to_decimal_string(row.try_get("cost_micros")?),
                    error_code: row.try_get("error_code")?,
                })
            })
            .collect::<Result<Vec<_>, AppError>>()?;
        let edge_rows = sqlx::query(
            "SELECT source_o.request_id AS from_request_id, target_o.request_id AS to_request_id, e.relation_kind, e.confidence_millis, e.evidence_json FROM conversation_edges e JOIN conversation_observations target_o ON target_o.id = e.to_observation_id LEFT JOIN conversation_observations source_o ON source_o.id = e.from_observation_id WHERE e.cluster_id = $1 AND target_o.key_id = $2 AND (source_o.key_id = $3 OR source_o.id IS NULL) ORDER BY target_o.created_at ASC",
        )
        .bind(cluster_id.to_string())
        .bind(key_id.to_string())
        .bind(key_id.to_string())
        .fetch_all(&self.pool)
        .await?;
        let edges = edge_rows
            .into_iter()
            .map(|row| {
                let from_request_id: Option<String> = row.try_get("from_request_id")?;
                let evidence: String = row.try_get("evidence_json")?;
                let confidence: i64 = row.try_get("confidence_millis")?;
                Ok(ConversationEdgeView {
                    from_request_id: from_request_id.map(parse_uuid).transpose()?,
                    to_request_id: parse_uuid(row.try_get("to_request_id")?)?,
                    relation: row.try_get("relation_kind")?,
                    confidence: confidence as f64 / 1_000.0,
                    evidence: serde_json::from_str(&evidence).unwrap_or(serde_json::Value::Null),
                })
            })
            .collect::<Result<Vec<_>, AppError>>()?;
        Ok(ConversationClusterDetail {
            cluster,
            requests,
            edges,
        })
    }

    pub async fn stats_filtered(
        &self,
        key_id: Uuid,
        mut filter: StatsFilter,
    ) -> Result<SelfStats, AppError> {
        // A downstream credential can never widen its view by supplying a different key_id.
        filter.key_id = Some(key_id);
        let stats = self.aggregate_filtered_stats(None, &filter).await?;
        Ok(SelfStats {
            key_id,
            summary: stats.summary,
            by_model: stats.by_model,
            by_day: stats.by_day,
            errors: stats.errors,
        })
    }

    pub async fn operator_stats_filtered(
        &self,
        tenant_external_id: &str,
        filter: StatsFilter,
    ) -> Result<OperatorStats, AppError> {
        self.aggregate_filtered_stats(Some(tenant_external_id), &filter)
            .await
    }

    pub async fn global_operator_stats_filtered(
        &self,
        filter: StatsFilter,
    ) -> Result<OperatorStats, AppError> {
        self.aggregate_filtered_stats(None, &filter).await
    }

    async fn aggregate_filtered_stats(
        &self,
        tenant_external_id: Option<&str>,
        filter: &StatsFilter,
    ) -> Result<OperatorStats, AppError> {
        validate_stats_filter(filter)?;
        let tenant_external_id = tenant_external_id.unwrap_or_default();
        let key_id = filter.key_id.map(|id| id.to_string()).unwrap_or_default();
        let from_created_at = filter.from_created_at.ok_or_else(|| {
            AppError::BadRequest("from_created_at is required for statistics".into())
        })?;
        let to_created_at = filter.to_created_at.ok_or_else(|| {
            AppError::BadRequest("to_created_at is required for statistics".into())
        })?;
        let upstream_account_id = filter
            .upstream_account_id
            .map(|id| id.to_string())
            .unwrap_or_default();
        let route_id = filter.route_id.map(|id| id.to_string()).unwrap_or_default();
        let key_alias = search_prefix(filter.key_alias.as_deref());
        let principal = search_prefix(filter.principal.as_deref());
        // Common generation statistics never touch the high-volume job table. Exact per-job
        // duration/cost filters and the transient pending state deliberately opt into the
        // indexed raw fallback; validate_stats_filter keeps that fallback bounded to 93 days.
        let use_generation_facts = filter.min_duration_ms.is_some()
            || filter.max_duration_ms.is_some()
            || filter.min_cost_micros.is_some()
            || filter.max_cost_micros.is_some();
        let activity_source = if filter.status.as_deref() == Some("pending") {
            FILTERED_ACTIVITY_SOURCE_PENDING_GENERATION
        } else if use_generation_facts {
            FILTERED_ACTIVITY_SOURCE_GENERATION_FACTS
        } else {
            FILTERED_ACTIVITY_SOURCE_AGGREGATED
        };
        const DAY_MILLIS: i64 = 86_400_000;
        let full_day_from = from_created_at
            .saturating_add(DAY_MILLIS - 1)
            .div_euclid(DAY_MILLIS)
            .saturating_mul(DAY_MILLIS);
        let full_day_to_exclusive = to_created_at
            .saturating_add(1)
            .div_euclid(DAY_MILLIS)
            .saturating_mul(DAY_MILLIS);

        macro_rules! bind_activity_filter {
            ($query:expr) => {
                $query
                    .bind(tenant_external_id)
                    .bind(&key_id)
                    .bind(from_created_at)
                    .bind(to_created_at)
                    .bind(filter.model.as_deref().unwrap_or_default())
                    .bind(filter.protocol.as_deref().unwrap_or_default())
                    .bind(filter.status.as_deref().unwrap_or_default())
                    .bind(filter.error_code.as_deref().unwrap_or_default())
                    .bind(&upstream_account_id)
                    .bind(&route_id)
                    .bind(filter.min_duration_ms.unwrap_or(-1))
                    .bind(filter.max_duration_ms.unwrap_or(-1))
                    .bind(filter.min_cost_micros.unwrap_or(-1))
                    .bind(filter.max_cost_micros.unwrap_or(-1))
                    .bind(&key_alias)
                    .bind(&principal)
                    .bind(full_day_from)
                    .bind(full_day_to_exclusive)
            };
        }

        let summary_sql = format!(
            "SELECT CAST(COALESCE(SUM(requests), 0) AS BIGINT) AS total_requests, CAST(COALESCE(SUM(CASE WHEN status_class = 'success' THEN requests ELSE 0 END), 0) AS BIGINT) AS successful_requests, CAST(COALESCE(SUM(CASE WHEN status_class = 'failure' THEN requests ELSE 0 END), 0) AS BIGINT) AS failed_requests, CAST(COALESCE(SUM(input_tokens), 0) AS BIGINT) AS input_tokens, CAST(COALESCE(SUM(output_tokens), 0) AS BIGINT) AS output_tokens, CAST(COALESCE(SUM(cost_micros), 0) AS BIGINT) AS cost_micros FROM ({activity_source}) AS filtered_activity"
        );
        let summary_row = bind_activity_filter!(sqlx::query(&summary_sql))
            .fetch_one(&self.pool)
            .await?;
        let summary = StatsSummary {
            total_requests: summary_row.try_get("total_requests")?,
            successful_requests: summary_row.try_get("successful_requests")?,
            failed_requests: summary_row.try_get("failed_requests")?,
            input_tokens: summary_row.try_get("input_tokens")?,
            output_tokens: summary_row.try_get("output_tokens")?,
            total_cost: micros_to_decimal_string(summary_row.try_get("cost_micros")?),
        };

        let models_sql = format!(
            "SELECT model AS name, CAST(COALESCE(SUM(requests), 0) AS BIGINT) AS requests, CAST(COALESCE(SUM(input_tokens), 0) AS BIGINT) AS input_tokens, CAST(COALESCE(SUM(output_tokens), 0) AS BIGINT) AS output_tokens, CAST(COALESCE(SUM(cost_micros), 0) AS BIGINT) AS cost_micros FROM ({activity_source}) AS filtered_activity GROUP BY model ORDER BY requests DESC, model ASC LIMIT 100"
        );
        let by_model = aggregate_buckets(
            bind_activity_filter!(sqlx::query(&models_sql))
                .fetch_all(&self.pool)
                .await?,
        )?;

        let days_sql = format!(
            "SELECT created_at / 86400000 AS day_bucket, CAST(COALESCE(SUM(requests), 0) AS BIGINT) AS requests, CAST(COALESCE(SUM(input_tokens), 0) AS BIGINT) AS input_tokens, CAST(COALESCE(SUM(output_tokens), 0) AS BIGINT) AS output_tokens, CAST(COALESCE(SUM(cost_micros), 0) AS BIGINT) AS cost_micros FROM ({activity_source}) AS filtered_activity GROUP BY created_at / 86400000 ORDER BY day_bucket ASC"
        );
        let by_day = bind_activity_filter!(sqlx::query(&days_sql))
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|row| {
                let day_bucket: i64 = row.try_get("day_bucket")?;
                let name = chrono::DateTime::from_timestamp(day_bucket.saturating_mul(86_400), 0)
                    .map(|value| value.format("%Y-%m-%d").to_string())
                    .unwrap_or_else(|| "unknown".to_owned());
                aggregate_bucket(row, name)
            })
            .collect::<Result<Vec<_>, AppError>>()?;

        let errors_sql = format!(
            "SELECT error_code AS name, CAST(COALESCE(SUM(requests), 0) AS BIGINT) AS requests, CAST(COALESCE(SUM(input_tokens), 0) AS BIGINT) AS input_tokens, CAST(COALESCE(SUM(output_tokens), 0) AS BIGINT) AS output_tokens, CAST(COALESCE(SUM(cost_micros), 0) AS BIGINT) AS cost_micros FROM ({activity_source}) AS filtered_activity WHERE error_code <> '' GROUP BY error_code ORDER BY requests DESC, error_code ASC LIMIT 100"
        );
        let errors = aggregate_buckets(
            bind_activity_filter!(sqlx::query(&errors_sql))
                .fetch_all(&self.pool)
                .await?,
        )?;
        Ok(OperatorStats {
            summary,
            by_model,
            by_day,
            errors,
        })
    }

    pub async fn stats(&self, key_id: Uuid) -> Result<SelfStats, AppError> {
        let key_id = key_id.to_string();
        let summary_row = sqlx::query(
            "SELECT CAST(COALESCE(SUM(total_requests), 0) AS BIGINT) AS total_requests, CAST(COALESCE(SUM(successful_requests), 0) AS BIGINT) AS successful_requests, CAST(COALESCE(SUM(failed_requests), 0) AS BIGINT) AS failed_requests, CAST(COALESCE(SUM(input_tokens), 0) AS BIGINT) AS input_tokens, CAST(COALESCE(SUM(output_tokens), 0) AS BIGINT) AS output_tokens, CAST(COALESCE(SUM(cost_micros), 0) AS BIGINT) AS cost_micros FROM (SELECT COALESCE(SUM(requests), 0) AS total_requests, COALESCE(SUM(CASE WHEN status_class = 'success' THEN requests ELSE 0 END), 0) AS successful_requests, COALESCE(SUM(CASE WHEN status_class = 'failure' THEN requests ELSE 0 END), 0) AS failed_requests, COALESCE(SUM(input_tokens), 0) AS input_tokens, COALESCE(SUM(output_tokens), 0) AS output_tokens, COALESCE(SUM(cost_micros), 0) AS cost_micros FROM usage_daily_aggregates WHERE key_id = $1 UNION ALL SELECT COALESCE(SUM(requests), 0) AS total_requests, COALESCE(SUM(CASE WHEN status_class = 'success' THEN requests ELSE 0 END), 0) AS successful_requests, COALESCE(SUM(CASE WHEN status_class = 'failure' THEN requests ELSE 0 END), 0) AS failed_requests, 0 AS input_tokens, 0 AS output_tokens, COALESCE(SUM(cost_micros), 0) AS cost_micros FROM generation_daily_aggregates WHERE key_id = $2) AS totals",
        )
        .bind(&key_id)
        .bind(&key_id)
        .fetch_one(&self.pool)
        .await?;
        let summary = StatsSummary {
            total_requests: summary_row.try_get("total_requests")?,
            successful_requests: summary_row.try_get("successful_requests")?,
            failed_requests: summary_row.try_get("failed_requests")?,
            input_tokens: summary_row.try_get("input_tokens")?,
            output_tokens: summary_row.try_get("output_tokens")?,
            total_cost: micros_to_decimal_string(summary_row.try_get("cost_micros")?),
        };

        let model_rows = sqlx::query(
            "SELECT name, CAST(SUM(requests) AS BIGINT) AS requests, CAST(SUM(input_tokens) AS BIGINT) AS input_tokens, CAST(SUM(output_tokens) AS BIGINT) AS output_tokens, CAST(SUM(cost_micros) AS BIGINT) AS cost_micros FROM (SELECT model AS name, requests, input_tokens, output_tokens, cost_micros FROM usage_daily_aggregates WHERE key_id = $1 UNION ALL SELECT model AS name, requests, 0 AS input_tokens, 0 AS output_tokens, cost_micros FROM generation_daily_aggregates WHERE key_id = $2) AS model_totals GROUP BY name ORDER BY requests DESC, name ASC",
        )
        .bind(&key_id)
        .bind(&key_id)
        .fetch_all(&self.pool)
        .await?;
        let by_model = aggregate_buckets(model_rows)?;

        let day_rows = sqlx::query(
            "SELECT day_bucket, CAST(SUM(requests) AS BIGINT) AS requests, CAST(SUM(input_tokens) AS BIGINT) AS input_tokens, CAST(SUM(output_tokens) AS BIGINT) AS output_tokens, CAST(SUM(cost_micros) AS BIGINT) AS cost_micros FROM (SELECT day_bucket, requests, input_tokens, output_tokens, cost_micros FROM usage_daily_aggregates WHERE key_id = $1 UNION ALL SELECT day_bucket, requests, 0 AS input_tokens, 0 AS output_tokens, cost_micros FROM generation_daily_aggregates WHERE key_id = $2) AS day_totals GROUP BY day_bucket ORDER BY day_bucket ASC",
        )
        .bind(&key_id)
        .bind(&key_id)
        .fetch_all(&self.pool)
        .await?;
        let by_day = day_rows
            .into_iter()
            .map(|row| {
                let day_bucket: i64 = row.try_get("day_bucket")?;
                let name = chrono::DateTime::from_timestamp(day_bucket.saturating_mul(86_400), 0)
                    .map(|value| value.format("%Y-%m-%d").to_string())
                    .unwrap_or_else(|| "unknown".to_owned());
                aggregate_bucket(row, name)
            })
            .collect::<Result<Vec<_>, AppError>>()?;

        let error_rows = sqlx::query(
            "SELECT name, CAST(SUM(requests) AS BIGINT) AS requests, CAST(SUM(input_tokens) AS BIGINT) AS input_tokens, CAST(SUM(output_tokens) AS BIGINT) AS output_tokens, CAST(SUM(cost_micros) AS BIGINT) AS cost_micros FROM (SELECT error_code AS name, requests, input_tokens, output_tokens, cost_micros FROM usage_daily_aggregates WHERE key_id = $1 AND error_code <> '' UNION ALL SELECT error_code AS name, requests, 0 AS input_tokens, 0 AS output_tokens, cost_micros FROM generation_daily_aggregates WHERE key_id = $2 AND status_class = 'failure' AND error_code <> '') AS error_totals GROUP BY name ORDER BY requests DESC, name ASC",
        )
        .bind(&key_id)
        .bind(&key_id)
        .fetch_all(&self.pool)
        .await?;
        let errors = aggregate_buckets(error_rows)?;

        Ok(SelfStats {
            key_id: parse_uuid(key_id)?,
            summary,
            by_model,
            by_day,
            errors,
        })
    }

    pub async fn operator_stats(
        &self,
        tenant_external_id: &str,
    ) -> Result<OperatorStats, AppError> {
        let summary_row = sqlx::query(
            "SELECT CAST(COALESCE(SUM(total_requests), 0) AS BIGINT) AS total_requests, CAST(COALESCE(SUM(successful_requests), 0) AS BIGINT) AS successful_requests, CAST(COALESCE(SUM(failed_requests), 0) AS BIGINT) AS failed_requests, CAST(COALESCE(SUM(input_tokens), 0) AS BIGINT) AS input_tokens, CAST(COALESCE(SUM(output_tokens), 0) AS BIGINT) AS output_tokens, CAST(COALESCE(SUM(cost_micros), 0) AS BIGINT) AS cost_micros FROM (SELECT COALESCE(SUM(a.requests), 0) AS total_requests, COALESCE(SUM(CASE WHEN a.status_class = 'success' THEN a.requests ELSE 0 END), 0) AS successful_requests, COALESCE(SUM(CASE WHEN a.status_class = 'failure' THEN a.requests ELSE 0 END), 0) AS failed_requests, COALESCE(SUM(a.input_tokens), 0) AS input_tokens, COALESCE(SUM(a.output_tokens), 0) AS output_tokens, COALESCE(SUM(a.cost_micros), 0) AS cost_micros FROM usage_daily_aggregates a JOIN key_records k ON k.id = a.key_id JOIN tenants t ON t.id = k.tenant_id WHERE t.external_id = $1 UNION ALL SELECT COALESCE(SUM(a.requests), 0) AS total_requests, COALESCE(SUM(CASE WHEN a.status_class = 'success' THEN a.requests ELSE 0 END), 0) AS successful_requests, COALESCE(SUM(CASE WHEN a.status_class = 'failure' THEN a.requests ELSE 0 END), 0) AS failed_requests, 0 AS input_tokens, 0 AS output_tokens, COALESCE(SUM(a.cost_micros), 0) AS cost_micros FROM generation_daily_aggregates a JOIN tenants t ON t.id = a.tenant_id WHERE t.external_id = $2) AS totals",
        )
        .bind(tenant_external_id)
        .bind(tenant_external_id)
        .fetch_one(&self.pool)
        .await?;
        let summary = StatsSummary {
            total_requests: summary_row.try_get("total_requests")?,
            successful_requests: summary_row.try_get("successful_requests")?,
            failed_requests: summary_row.try_get("failed_requests")?,
            input_tokens: summary_row.try_get("input_tokens")?,
            output_tokens: summary_row.try_get("output_tokens")?,
            total_cost: micros_to_decimal_string(summary_row.try_get("cost_micros")?),
        };
        let model_rows = sqlx::query(
            "SELECT name, CAST(SUM(requests) AS BIGINT) AS requests, CAST(SUM(input_tokens) AS BIGINT) AS input_tokens, CAST(SUM(output_tokens) AS BIGINT) AS output_tokens, CAST(SUM(cost_micros) AS BIGINT) AS cost_micros FROM (SELECT a.model AS name, a.requests, a.input_tokens, a.output_tokens, a.cost_micros FROM usage_daily_aggregates a JOIN key_records k ON k.id = a.key_id JOIN tenants t ON t.id = k.tenant_id WHERE t.external_id = $1 UNION ALL SELECT a.model AS name, a.requests, 0 AS input_tokens, 0 AS output_tokens, a.cost_micros FROM generation_daily_aggregates a JOIN tenants t ON t.id = a.tenant_id WHERE t.external_id = $2) AS model_totals GROUP BY name ORDER BY requests DESC, name ASC",
        )
        .bind(tenant_external_id)
        .bind(tenant_external_id)
        .fetch_all(&self.pool)
        .await?;
        let by_model = aggregate_buckets(model_rows)?;
        let day_rows = sqlx::query(
            "SELECT day_bucket, CAST(SUM(requests) AS BIGINT) AS requests, CAST(SUM(input_tokens) AS BIGINT) AS input_tokens, CAST(SUM(output_tokens) AS BIGINT) AS output_tokens, CAST(SUM(cost_micros) AS BIGINT) AS cost_micros FROM (SELECT a.day_bucket, a.requests, a.input_tokens, a.output_tokens, a.cost_micros FROM usage_daily_aggregates a JOIN key_records k ON k.id = a.key_id JOIN tenants t ON t.id = k.tenant_id WHERE t.external_id = $1 UNION ALL SELECT a.day_bucket, a.requests, 0 AS input_tokens, 0 AS output_tokens, a.cost_micros FROM generation_daily_aggregates a JOIN tenants t ON t.id = a.tenant_id WHERE t.external_id = $2) AS day_totals GROUP BY day_bucket ORDER BY day_bucket ASC",
        )
        .bind(tenant_external_id)
        .bind(tenant_external_id)
        .fetch_all(&self.pool)
        .await?;
        let by_day = day_rows
            .into_iter()
            .map(|row| {
                let day_bucket: i64 = row.try_get("day_bucket")?;
                let name = chrono::DateTime::from_timestamp(day_bucket.saturating_mul(86_400), 0)
                    .map(|value| value.format("%Y-%m-%d").to_string())
                    .unwrap_or_else(|| "unknown".to_owned());
                aggregate_bucket(row, name)
            })
            .collect::<Result<Vec<_>, AppError>>()?;
        let error_rows = sqlx::query(
            "SELECT name, CAST(SUM(requests) AS BIGINT) AS requests, CAST(SUM(input_tokens) AS BIGINT) AS input_tokens, CAST(SUM(output_tokens) AS BIGINT) AS output_tokens, CAST(SUM(cost_micros) AS BIGINT) AS cost_micros FROM (SELECT a.error_code AS name, a.requests, a.input_tokens, a.output_tokens, a.cost_micros FROM usage_daily_aggregates a JOIN key_records k ON k.id = a.key_id JOIN tenants t ON t.id = k.tenant_id WHERE t.external_id = $1 AND a.error_code <> '' UNION ALL SELECT a.error_code AS name, a.requests, 0 AS input_tokens, 0 AS output_tokens, a.cost_micros FROM generation_daily_aggregates a JOIN tenants t ON t.id = a.tenant_id WHERE t.external_id = $2 AND a.status_class = 'failure' AND a.error_code <> '') AS error_totals GROUP BY name ORDER BY requests DESC, name ASC",
        )
        .bind(tenant_external_id)
        .bind(tenant_external_id)
        .fetch_all(&self.pool)
        .await?;
        let errors = aggregate_buckets(error_rows)?;
        Ok(OperatorStats {
            summary,
            by_model,
            by_day,
            errors,
        })
    }

    pub async fn global_operator_stats(&self) -> Result<OperatorStats, AppError> {
        let summary_row = sqlx::query(
            "SELECT CAST(COALESCE(SUM(total_requests), 0) AS BIGINT) AS total_requests, CAST(COALESCE(SUM(successful_requests), 0) AS BIGINT) AS successful_requests, CAST(COALESCE(SUM(failed_requests), 0) AS BIGINT) AS failed_requests, CAST(COALESCE(SUM(input_tokens), 0) AS BIGINT) AS input_tokens, CAST(COALESCE(SUM(output_tokens), 0) AS BIGINT) AS output_tokens, CAST(COALESCE(SUM(cost_micros), 0) AS BIGINT) AS cost_micros FROM (SELECT COALESCE(SUM(requests), 0) AS total_requests, COALESCE(SUM(CASE WHEN status_class = 'success' THEN requests ELSE 0 END), 0) AS successful_requests, COALESCE(SUM(CASE WHEN status_class = 'failure' THEN requests ELSE 0 END), 0) AS failed_requests, COALESCE(SUM(input_tokens), 0) AS input_tokens, COALESCE(SUM(output_tokens), 0) AS output_tokens, COALESCE(SUM(cost_micros), 0) AS cost_micros FROM usage_daily_aggregates UNION ALL SELECT COALESCE(SUM(requests), 0) AS total_requests, COALESCE(SUM(CASE WHEN status_class = 'success' THEN requests ELSE 0 END), 0) AS successful_requests, COALESCE(SUM(CASE WHEN status_class = 'failure' THEN requests ELSE 0 END), 0) AS failed_requests, 0 AS input_tokens, 0 AS output_tokens, COALESCE(SUM(cost_micros), 0) AS cost_micros FROM generation_daily_aggregates) AS totals",
        )
        .fetch_one(&self.pool)
        .await?;
        let summary = StatsSummary {
            total_requests: summary_row.try_get("total_requests")?,
            successful_requests: summary_row.try_get("successful_requests")?,
            failed_requests: summary_row.try_get("failed_requests")?,
            input_tokens: summary_row.try_get("input_tokens")?,
            output_tokens: summary_row.try_get("output_tokens")?,
            total_cost: micros_to_decimal_string(summary_row.try_get("cost_micros")?),
        };
        let model_rows = sqlx::query(
            "SELECT name, CAST(SUM(requests) AS BIGINT) AS requests, CAST(SUM(input_tokens) AS BIGINT) AS input_tokens, CAST(SUM(output_tokens) AS BIGINT) AS output_tokens, CAST(SUM(cost_micros) AS BIGINT) AS cost_micros FROM (SELECT model AS name, requests, input_tokens, output_tokens, cost_micros FROM usage_daily_aggregates UNION ALL SELECT model AS name, requests, 0 AS input_tokens, 0 AS output_tokens, cost_micros FROM generation_daily_aggregates) AS model_totals GROUP BY name ORDER BY requests DESC, name ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        let by_model = aggregate_buckets(model_rows)?;
        let day_rows = sqlx::query(
            "SELECT day_bucket, CAST(SUM(requests) AS BIGINT) AS requests, CAST(SUM(input_tokens) AS BIGINT) AS input_tokens, CAST(SUM(output_tokens) AS BIGINT) AS output_tokens, CAST(SUM(cost_micros) AS BIGINT) AS cost_micros FROM (SELECT day_bucket, requests, input_tokens, output_tokens, cost_micros FROM usage_daily_aggregates UNION ALL SELECT day_bucket, requests, 0 AS input_tokens, 0 AS output_tokens, cost_micros FROM generation_daily_aggregates) AS day_totals GROUP BY day_bucket ORDER BY day_bucket ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        let by_day = day_rows
            .into_iter()
            .map(|row| {
                let day_bucket: i64 = row.try_get("day_bucket")?;
                let name = chrono::DateTime::from_timestamp(day_bucket.saturating_mul(86_400), 0)
                    .map(|value| value.format("%Y-%m-%d").to_string())
                    .unwrap_or_else(|| "unknown".to_owned());
                aggregate_bucket(row, name)
            })
            .collect::<Result<Vec<_>, AppError>>()?;
        let error_rows = sqlx::query(
            "SELECT name, CAST(SUM(requests) AS BIGINT) AS requests, CAST(SUM(input_tokens) AS BIGINT) AS input_tokens, CAST(SUM(output_tokens) AS BIGINT) AS output_tokens, CAST(SUM(cost_micros) AS BIGINT) AS cost_micros FROM (SELECT error_code AS name, requests, input_tokens, output_tokens, cost_micros FROM usage_daily_aggregates WHERE error_code <> '' UNION ALL SELECT error_code AS name, requests, 0 AS input_tokens, 0 AS output_tokens, cost_micros FROM generation_daily_aggregates WHERE status_class = 'failure' AND error_code <> '') AS error_totals GROUP BY name ORDER BY requests DESC, name ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(OperatorStats {
            summary,
            by_model,
            by_day,
            errors: aggregate_buckets(error_rows)?,
        })
    }

    /// Bounded, process-level and active-queue gauges for the Prometheus
    /// endpoint. No tenant, credential, model or request identifiers are read.
    pub async fn runtime_metrics(
        &self,
    ) -> Result<crate::metrics::DatabaseRuntimeMetrics, AppError> {
        let row = sqlx::query(
            "SELECT (SELECT COUNT(*) FROM generation_jobs WHERE status = 'queued') AS queued_jobs, (SELECT COUNT(*) FROM generation_jobs WHERE status = 'running') AS running_jobs",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(crate::metrics::DatabaseRuntimeMetrics {
            pool_size: self.pool.size(),
            pool_idle: self.pool.num_idle(),
            queued_jobs: row.try_get("queued_jobs")?,
            running_jobs: row.try_get("running_jobs")?,
        })
    }
}

async fn apply_migration_range(
    transaction: &mut sqlx::Transaction<'_, sqlx::Any>,
    migrations: &[Migration],
    first: i64,
    last: i64,
) -> Result<(), sqlx::Error> {
    for migration in migrations
        .iter()
        .filter(|migration| (first..=last).contains(&migration.version))
    {
        let applied = sqlx::query("SELECT version FROM schema_migrations WHERE version = $1")
            .bind(migration.version)
            .fetch_optional(&mut **transaction)
            .await?
            .is_some();
        if applied {
            continue;
        }
        for statement in migration
            .sql
            .split(';')
            .map(str::trim)
            .filter(|statement| !statement.is_empty())
        {
            sqlx::query(statement)
                .execute(&mut **transaction)
                .await
                .map_err(|error| {
                    sqlx::Error::Protocol(format!(
                        "migration {} ({}) failed at statement `{statement}`: {error}",
                        migration.version, migration.name
                    ))
                })?;
        }
        sqlx::query(
            "INSERT INTO schema_migrations (version, name, applied_at) VALUES ($1, $2, $3)",
        )
        .bind(migration.version)
        .bind(migration.name)
        .bind(unix_millis())
        .execute(&mut **transaction)
        .await?;
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RequestRecordLocator {
    created_at: i64,
    tenant_id: String,
    key_id: String,
}

async fn claim_request_record_locator(
    transaction: &mut Transaction<'_, Any>,
    id: &str,
    created_at: i64,
    tenant_id: &str,
    key_id: &str,
) -> Result<bool, AppError> {
    let claimed = sqlx::query(
        "INSERT INTO request_record_locators (id, created_at, tenant_id, key_id) VALUES ($1, $2, $3, $4) ON CONFLICT(id) DO NOTHING",
    )
    .bind(id)
    .bind(created_at)
    .bind(tenant_id)
    .bind(key_id)
    .execute(&mut **transaction)
    .await?;
    if claimed.rows_affected() == 1 {
        return Ok(true);
    }
    let existing = sqlx::query(
        "SELECT created_at, tenant_id, key_id FROM request_record_locators WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&mut **transaction)
    .await?;
    let matches = existing.try_get::<i64, _>("created_at")? == created_at
        && existing.try_get::<String, _>("tenant_id")? == tenant_id
        && existing.try_get::<String, _>("key_id")? == key_id;
    if matches {
        Ok(false)
    } else {
        Err(AppError::BadRequest(
            "request id is already owned by a different request locator".into(),
        ))
    }
}

async fn claim_request_event_locator(
    transaction: &mut Transaction<'_, Any>,
    id: &str,
    created_at: i64,
    tenant_id: &str,
    key_id: &str,
    request_id: &str,
) -> Result<bool, AppError> {
    let claimed = sqlx::query(
        "INSERT INTO request_event_locators (id, created_at, tenant_id, key_id, request_id) VALUES ($1, $2, $3, $4, $5) ON CONFLICT(id) DO NOTHING",
    )
    .bind(id)
    .bind(created_at)
    .bind(tenant_id)
    .bind(key_id)
    .bind(request_id)
    .execute(&mut **transaction)
    .await?;
    if claimed.rows_affected() == 1 {
        return Ok(true);
    }
    let existing = sqlx::query(
        "SELECT created_at, tenant_id, key_id, request_id FROM request_event_locators WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&mut **transaction)
    .await?;
    let matches = existing.try_get::<i64, _>("created_at")? == created_at
        && existing.try_get::<String, _>("tenant_id")? == tenant_id
        && existing.try_get::<String, _>("key_id")? == key_id
        && existing.try_get::<String, _>("request_id")? == request_id;
    if matches {
        Ok(false)
    } else {
        Err(AppError::BadRequest(
            "request event id is already owned by a different event locator".into(),
        ))
    }
}

fn credential_rotation_request_hash(resource_kind: &str, resource_id: Uuid) -> String {
    let canonical = format!(
        "memeloop-token-center/credential-rotation-request/v1\0{resource_kind}\0{resource_id}"
    );
    format!("{:x}", Sha256::digest(canonical.as_bytes()))
}

fn upstream_credential_rotation_request_hash(
    account_id: Uuid,
    credential: &UpstreamCredential,
) -> Result<String, AppError> {
    let encoded = serde_json::to_vec(credential).map_err(|_| AppError::Internal)?;
    let mut hash = Sha256::new();
    hash.update(b"memeloop-token-center/upstream-credential-rotation-request/v1\0");
    hash.update(account_id.as_bytes());
    hash.update([0]);
    hash.update(encoded);
    Ok(format!("{:x}", hash.finalize()))
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

async fn insert_credential(
    tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    issued: &crypto::IssuedCredential,
    generation: i64,
    now: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO key_credentials (id, key_id, generation, secret_hash, fingerprint, created_at) VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(issued.credential_id.to_string())
    .bind(issued.key_id.to_string())
    .bind(generation)
    .bind(issued.secret_hash.clone())
    .bind(&issued.fingerprint)
    .bind(now)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Adds a terminal generation job to the daily aggregate exactly once.
///
/// The marker and aggregate update deliberately share the caller's transaction with the terminal
/// state change. A retry therefore observes either both writes or neither write.
async fn aggregate_terminal_generation_job(
    transaction: &mut Transaction<'_, Any>,
    job_id: &str,
    now: i64,
) -> Result<(), AppError> {
    let claimed = sqlx::query(
        "UPDATE generation_jobs SET stats_aggregated_at = $1 WHERE id = $2 AND stats_aggregated_at IS NULL AND status IN ('succeeded', 'failed', 'cancelled')",
    )
    .bind(now)
    .bind(job_id)
    .execute(&mut **transaction)
    .await?;
    if claimed.rows_affected() == 0 {
        return Ok(());
    }

    let fact = sqlx::query(
        "INSERT INTO generation_stats_facts (job_id, tenant_id, key_id, created_at, model, status_class, error_code, upstream_account_id, duration_ms, cost_micros, billed_units) SELECT id, tenant_id, key_id, created_at, public_model, CASE WHEN status = 'succeeded' THEN 'success' ELSE 'failure' END, COALESCE(error_code, ''), COALESCE(upstream_account_id, ''), CASE WHEN completed_at IS NULL OR completed_at < created_at THEN 0 ELSE completed_at - created_at END, cost_micros, COALESCE(billed_units, 0) FROM generation_jobs WHERE id = $1 AND status IN ('succeeded', 'failed', 'cancelled') ON CONFLICT (job_id) DO NOTHING",
    )
    .bind(job_id)
    .execute(&mut **transaction)
    .await?;
    if fact.rows_affected() != 1 {
        return Err(AppError::Internal);
    }

    let aggregated = sqlx::query(
        "INSERT INTO generation_daily_aggregates (tenant_id, key_id, day_bucket, model, status_class, error_code, upstream_account_id, requests, billed_units, cost_micros) SELECT tenant_id, key_id, created_at / 86400000, public_model, CASE WHEN status = 'succeeded' THEN 'success' ELSE 'failure' END, COALESCE(error_code, ''), COALESCE(upstream_account_id, ''), 1, COALESCE(billed_units, 0), cost_micros FROM generation_jobs WHERE id = $1 AND status IN ('succeeded', 'failed', 'cancelled') ON CONFLICT (tenant_id, key_id, day_bucket, model, status_class, error_code, upstream_account_id) DO UPDATE SET requests = generation_daily_aggregates.requests + excluded.requests, billed_units = generation_daily_aggregates.billed_units + excluded.billed_units, cost_micros = generation_daily_aggregates.cost_micros + excluded.cost_micros",
    )
    .bind(job_id)
    .execute(&mut **transaction)
    .await?;
    if aggregated.rows_affected() != 1 {
        return Err(AppError::Internal);
    }
    Ok(())
}

fn aggregate_buckets(rows: Vec<sqlx::any::AnyRow>) -> Result<Vec<StatsBucket>, AppError> {
    rows.into_iter()
        .map(|row| {
            let name: String = row.try_get("name")?;
            aggregate_bucket(row, name)
        })
        .collect()
}

fn request_views(rows: Vec<AnyRow>) -> Result<Vec<RequestView>, AppError> {
    rows.into_iter()
        .map(|row| {
            Ok(RequestView {
                request_id: parse_uuid(row.try_get("id")?)?,
                created_at: row.try_get("created_at")?,
                protocol: row.try_get("protocol")?,
                model: row.try_get("model")?,
                status_code: row.try_get("status_code")?,
                duration_ms: row.try_get("duration_ms")?,
                input_tokens: row.try_get("input_tokens")?,
                output_tokens: row.try_get("output_tokens")?,
                cost: micros_to_decimal_string(row.try_get("cost_micros")?),
                error_code: row.try_get("error_code")?,
            })
        })
        .collect()
}

fn request_event_views(rows: Vec<AnyRow>) -> Result<Vec<RequestEventView>, AppError> {
    rows.into_iter()
        .map(|row| {
            Ok(RequestEventView {
                event_id: parse_uuid(row.try_get("event_id")?)?,
                request_id: parse_uuid(row.try_get("request_id")?)?,
                event_at: row.try_get("event_at")?,
                event_kind: row.try_get("event_kind")?,
                key_id: parse_uuid(row.try_get("key_id")?)?,
                protocol: row.try_get("protocol")?,
                model: row.try_get("model")?,
                status_code: row.try_get("status_code")?,
                duration_ms: row.try_get("duration_ms")?,
                input_tokens: row.try_get("input_tokens")?,
                output_tokens: row.try_get("output_tokens")?,
                cost: micros_to_decimal_string(row.try_get("cost_micros")?),
                error_code: row.try_get("error_code")?,
            })
        })
        .collect()
}

fn upstream_account_view(row: sqlx::any::AnyRow) -> Result<UpstreamAccountView, AppError> {
    let config_json: String = row.try_get("config_json")?;
    let driver: String = row.try_get("driver")?;
    let auth_kind: String = row.try_get("auth_kind")?;
    Ok(UpstreamAccountView {
        id: parse_uuid(row.try_get("id")?)?,
        tenant_id: parse_uuid(row.try_get("tenant_id")?)?,
        tenant_external_id: row.try_get("tenant_external_id").ok(),
        name: row.try_get("name")?,
        connection_method: upstream_connection_method(&driver, &auth_kind),
        driver,
        auth_kind,
        credential_generation: row.try_get("credential_generation")?,
        status: row.try_get("status")?,
        config: serde_json::from_str(&config_json).map_err(|_| AppError::Internal)?,
        credential_expires_at: row.try_get("expires_at")?,
        route_count: row.try_get("route_count")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

fn upstream_connection_method(driver: &str, auth_kind: &str) -> String {
    if driver == "cpa-subscription-bridge" {
        "subscription_bridge".to_owned()
    } else {
        auth_kind.to_owned()
    }
}

fn managed_key_view(row: AnyRow) -> Result<ManagedKeyView, AppError> {
    let policy_json: String = row.try_get("policy_json")?;
    Ok(ManagedKeyView {
        key_id: parse_uuid(row.try_get("id")?)?,
        account_id: parse_uuid(row.try_get("account_id")?)?,
        tenant_external_id: row.try_get("tenant_external_id")?,
        principal_external_id: row.try_get("principal_external_id")?,
        alias: row.try_get("alias")?,
        currency: row.try_get("currency")?,
        status: row.try_get("status")?,
        credential_generation: row.try_get("credential_generation")?,
        fingerprint: row.try_get("fingerprint")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
        policy: serde_json::from_str(&policy_json).map_err(|_| AppError::Internal)?,
        available_balance: micros_to_decimal_string(row.try_get("available_micros")?),
        reserved_balance: micros_to_decimal_string(row.try_get("reserved_micros")?),
    })
}

fn service_token_view(row: AnyRow) -> Result<ServiceTokenView, AppError> {
    let scopes_json: String = row.try_get("scopes_json")?;
    Ok(ServiceTokenView {
        service_id: parse_uuid(row.try_get("id")?)?,
        name: row.try_get("name")?,
        status: row.try_get("status")?,
        credential_generation: row.try_get("credential_generation")?,
        fingerprint: row.try_get("fingerprint")?,
        scopes: serde_json::from_str(&scopes_json).map_err(|_| AppError::Internal)?,
        tenant_external_id: row.try_get("tenant_external_id")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

fn model_route_view(row: AnyRow) -> Result<ModelRouteView, AppError> {
    Ok(ModelRouteView {
        id: parse_uuid(row.try_get("id")?)?,
        tenant_id: parse_uuid(row.try_get("tenant_id")?)?,
        tenant_external_id: row.try_get("tenant_external_id").ok(),
        public_model: row.try_get("public_model")?,
        upstream_account_id: parse_uuid(row.try_get("upstream_account_id")?)?,
        upstream_model: row.try_get("upstream_model")?,
        protocol: row.try_get("protocol")?,
        priority: row.try_get("priority")?,
        enabled: row.try_get::<i64, _>("enabled")? != 0,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

fn validate_model_route_fields(
    public_model: &str,
    upstream_model: &str,
    protocol: &str,
    priority: i64,
) -> Result<(), AppError> {
    let public_model = public_model.trim();
    let upstream_model = upstream_model.trim();
    if public_model.is_empty() || upstream_model.is_empty() {
        return Err(AppError::BadRequest(
            "public_model and upstream_model are required".into(),
        ));
    }
    if public_model.len() > 200 || upstream_model.len() > 500 {
        return Err(AppError::BadRequest(
            "public_model and upstream_model exceed their length limit".into(),
        ));
    }
    if public_model.chars().any(char::is_control) || upstream_model.chars().any(char::is_control) {
        return Err(AppError::BadRequest(
            "model names must not contain control characters".into(),
        ));
    }
    if !matches!(protocol, "openai" | "anthropic" | "generation") {
        return Err(AppError::BadRequest(
            "route protocol must be openai, anthropic, or generation".into(),
        ));
    }
    if !(-1_000_000..=1_000_000).contains(&priority) {
        return Err(AppError::BadRequest(
            "route priority must be between -1000000 and 1000000".into(),
        ));
    }
    Ok(())
}

fn generation_price_view(row: AnyRow) -> Result<GenerationPrice, AppError> {
    let micros_per_unit: i64 = row.try_get("micros_per_unit")?;
    Ok(GenerationPrice {
        id: parse_uuid(row.try_get("id")?)?,
        model: row.try_get("model")?,
        currency: row.try_get("currency")?,
        billing_unit: row.try_get("billing_unit")?,
        price_per_unit: micros_to_decimal_string(micros_per_unit),
        micros_per_unit,
    })
}

fn aggregate_bucket(row: sqlx::any::AnyRow, name: String) -> Result<StatsBucket, AppError> {
    Ok(StatsBucket {
        name,
        requests: row.try_get("requests")?,
        input_tokens: row.try_get("input_tokens")?,
        output_tokens: row.try_get("output_tokens")?,
        cost: micros_to_decimal_string(row.try_get("cost_micros")?),
    })
}

fn request_archive_refs_from_row(row: AnyRow) -> Result<RequestArchiveRefs, AppError> {
    Ok(RequestArchiveRefs {
        view: RequestView {
            request_id: parse_uuid(row.try_get("id")?)?,
            created_at: row.try_get("created_at")?,
            protocol: row.try_get("protocol")?,
            model: row.try_get("model")?,
            status_code: row.try_get("status_code")?,
            duration_ms: row.try_get("duration_ms")?,
            input_tokens: row.try_get("input_tokens")?,
            output_tokens: row.try_get("output_tokens")?,
            cost: micros_to_decimal_string(row.try_get("cost_micros")?),
            error_code: row.try_get("error_code")?,
        },
        request_object: row.try_get("request_object")?,
        response_object: row.try_get("response_object")?,
        response_json: None,
    })
}

fn generation_archive_refs_from_row(row: AnyRow) -> Result<RequestArchiveRefs, AppError> {
    let created_at: i64 = row.try_get("created_at")?;
    let completed_at: Option<i64> = row.try_get("completed_at")?;
    let status: String = row.try_get("status")?;
    let result_json: Option<String> = row.try_get("result_json")?;
    Ok(RequestArchiveRefs {
        view: RequestView {
            request_id: parse_uuid(row.try_get("id")?)?,
            created_at,
            protocol: "generation".to_owned(),
            model: row.try_get("public_model")?,
            status_code: match status.as_str() {
                "succeeded" => Some(200),
                "failed" | "cancelled" => Some(502),
                _ => None,
            },
            duration_ms: completed_at.map(|value| value - created_at),
            input_tokens: 0,
            output_tokens: 0,
            cost: micros_to_decimal_string(row.try_get("cost_micros")?),
            error_code: row.try_get("error_code")?,
        },
        request_object: row.try_get("request_object")?,
        response_object: None,
        response_json: result_json
            .map(|value| serde_json::from_str(&value).map_err(|_| AppError::Internal))
            .transpose()?,
    })
}

fn infer_hash_relation(previous: &[String], current: &[String]) -> (RelationKind, i64) {
    let shared = previous
        .iter()
        .zip(current)
        .take_while(|(left, right)| left == right)
        .count();
    if shared == previous.len() && shared == current.len() {
        (RelationKind::Retry, 980)
    } else if shared == previous.len() && current.len() > previous.len() {
        (RelationKind::Continues, 950)
    } else if shared > 0 && shared + 1 >= previous.len().min(current.len()) {
        (RelationKind::Edit, 820)
    } else if shared >= 2 {
        (RelationKind::Branch, 720)
    } else {
        (RelationKind::Candidate, 350)
    }
}

fn relation_name(relation: RelationKind) -> &'static str {
    match relation {
        RelationKind::Continues => "continues",
        RelationKind::Retry => "retry",
        RelationKind::Edit => "edit",
        RelationKind::Branch => "branch",
        RelationKind::Compacts => "compacts",
        RelationKind::Subagent => "subagent",
        RelationKind::Candidate => "candidate",
    }
}

async fn lock_entitlement_account(
    tx: &mut Transaction<'_, Any>,
    account_id: Uuid,
    tenant_id: &str,
    currency: Option<&str>,
) -> Result<String, AppError> {
    let locked = sqlx::query(
        "UPDATE credit_accounts SET updated_at = updated_at WHERE id = $1 AND tenant_id = $2",
    )
    .bind(account_id.to_string())
    .bind(tenant_id)
    .execute(&mut **tx)
    .await?;
    if locked.rows_affected() != 1 {
        return Err(AppError::Forbidden);
    }
    let row = sqlx::query("SELECT currency FROM credit_accounts WHERE id = $1")
        .bind(account_id.to_string())
        .fetch_one(&mut **tx)
        .await?;
    let account_currency: String = row.try_get("currency")?;
    if currency.is_some_and(|value| !account_currency.eq_ignore_ascii_case(value)) {
        return Err(AppError::Conflict(
            "entitlement currency does not match the stable credit account".into(),
        ));
    }
    Ok(account_currency)
}

async fn entitlement_result_view(
    tx: &mut Transaction<'_, Any>,
    entitlement_id: &str,
) -> Result<EntitlementView, AppError> {
    let row = sqlx::query(
        "SELECT e.id AS entitlement_id, c.id AS cycle_id, t.external_id AS tenant_external_id, e.account_id, e.provider, e.external_subscription_id, c.external_cycle_id, c.period_start, c.period_end, c.currency, c.desired_micros, c.consumed_micros, c.funded_micros, e.status, e.version, e.replaced_by_entitlement_id, e.created_at, e.updated_at FROM subscription_entitlements e JOIN tenants t ON t.id = e.tenant_id JOIN entitlement_cycles c ON c.id = e.current_cycle_id WHERE e.id = $1",
    )
    .bind(entitlement_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(AppError::NotFound)?;
    entitlement_view(row)
}

#[allow(clippy::too_many_arguments)]
async fn adjust_entitlement_cycle(
    tx: &mut Transaction<'_, Any>,
    account_id: Uuid,
    cycle_id: &str,
    desired_micros: i64,
    status: &str,
    revoke_remaining: bool,
    proration_json: Option<&str>,
    source: &str,
    reconciliation_id: Uuid,
    now: i64,
) -> Result<i64, AppError> {
    let row = sqlx::query(
        "SELECT currency, funded_micros, consumed_micros FROM entitlement_cycles WHERE id = $1",
    )
    .bind(cycle_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(AppError::NotFound)?;
    let currency: String = row.try_get("currency")?;
    let funded_micros: i64 = row.try_get("funded_micros")?;
    let consumed_micros: i64 = row.try_get("consumed_micros")?;
    let target_funded = if revoke_remaining {
        consumed_micros
    } else {
        desired_micros.max(consumed_micros)
    };
    let delta = target_funded.saturating_sub(funded_micros);
    let account_update = if delta < 0 {
        sqlx::query(
            "UPDATE credit_accounts SET available_micros = available_micros + $1, updated_at = $2 WHERE id = $3 AND available_micros >= $4",
        )
        .bind(delta)
        .bind(now)
        .bind(account_id.to_string())
        .bind(delta.saturating_abs())
        .execute(&mut **tx)
        .await?
    } else {
        sqlx::query(
            "UPDATE credit_accounts SET available_micros = available_micros + $1, updated_at = $2 WHERE id = $3",
        )
        .bind(delta)
        .bind(now)
        .bind(account_id.to_string())
        .execute(&mut **tx)
        .await?
    };
    if account_update.rows_affected() != 1 {
        return Err(AppError::Conflict(
            "entitlement remaining credit is no longer revocable".into(),
        ));
    }
    sqlx::query(
        "UPDATE entitlement_cycles SET desired_micros = $1, funded_micros = $2, status = $3, proration_json = $4, updated_at = $5 WHERE id = $6",
    )
    .bind(desired_micros)
    .bind(target_funded)
    .bind(status)
    .bind(proration_json)
    .bind(now)
    .bind(cycle_id)
    .execute(&mut **tx)
    .await?;
    if delta != 0 {
        sqlx::query(
            "INSERT INTO ledger_entries (id, account_id, kind, amount_micros, currency, source, idempotency_key, entitlement_cycle_id, created_at) VALUES ($1, $2, 'entitlement_adjustment', $3, $4, $5, $6, $7, $8)",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(account_id.to_string())
        .bind(delta)
        .bind(currency)
        .bind(source)
        .bind(format!("entitlement:{reconciliation_id}:{cycle_id}"))
        .bind(cycle_id)
        .bind(now)
        .execute(&mut **tx)
        .await?;
    }
    Ok(delta)
}

async fn reconcile_entitlement_snapshot(
    tx: &mut Transaction<'_, Any>,
    tenant_id: &str,
    input: &ReconcileEntitlementInput,
    reconciliation_id: Uuid,
    now: i64,
) -> Result<EntitlementReconcileResult, AppError> {
    lock_entitlement_account(tx, input.account_id, tenant_id, Some(&input.currency)).await?;
    let existing = sqlx::query(
        "SELECT id, account_id, status, version, current_cycle_id FROM subscription_entitlements WHERE tenant_id = $1 AND provider = $2 AND external_subscription_id = $3",
    )
    .bind(tenant_id)
    .bind(&input.provider)
    .bind(&input.external_subscription_id)
    .fetch_optional(&mut **tx)
    .await?;
    let entitlement_id = if let Some(row) = existing {
        let existing_id: String = row.try_get("id")?;
        let existing_account: String = row.try_get("account_id")?;
        let status: String = row.try_get("status")?;
        let version: i64 = row.try_get("version")?;
        if parse_uuid(existing_account)? != input.account_id {
            return Err(AppError::Conflict(
                "a stable subscription cannot be moved to another credit account; use replace"
                    .into(),
            ));
        }
        if status == "replaced" {
            return Err(AppError::Conflict(
                "a replaced subscription cannot be reactivated".into(),
            ));
        }
        if input.version <= version {
            return Err(AppError::Conflict(format!(
                "entitlement version must be greater than {version}"
            )));
        }
        existing_id
    } else {
        let id = Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO subscription_entitlements (id, tenant_id, account_id, provider, external_subscription_id, status, version, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, 'active', $6, $7, $8)",
        )
        .bind(&id)
        .bind(tenant_id)
        .bind(input.account_id.to_string())
        .bind(&input.provider)
        .bind(&input.external_subscription_id)
        .bind(input.version)
        .bind(now)
        .bind(now)
        .execute(&mut **tx)
        .await?;
        id
    };

    let existing_cycle = sqlx::query(
        "SELECT id, period_start, period_end, currency FROM entitlement_cycles WHERE entitlement_id = $1 AND external_cycle_id = $2",
    )
    .bind(&entitlement_id)
    .bind(&input.external_cycle_id)
    .fetch_optional(&mut **tx)
    .await?;
    let cycle_id = if let Some(row) = existing_cycle {
        if row.try_get::<i64, _>("period_start")? != input.period_start
            || row.try_get::<i64, _>("period_end")? != input.period_end
            || !row
                .try_get::<String, _>("currency")?
                .eq_ignore_ascii_case(&input.currency)
        {
            return Err(AppError::Conflict(
                "a stable billing-cycle identity cannot change period or currency".into(),
            ));
        }
        row.try_get("id")?
    } else {
        let id = Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO entitlement_cycles (id, entitlement_id, external_cycle_id, period_start, period_end, currency, desired_micros, funded_micros, consumed_micros, status, proration_json, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, $6, 0, 0, 0, 'active', $7, $8, $9)",
        )
        .bind(&id)
        .bind(&entitlement_id)
        .bind(&input.external_cycle_id)
        .bind(input.period_start)
        .bind(input.period_end)
        .bind(input.currency.to_uppercase())
        .bind(input.proration_json.as_deref())
        .bind(now)
        .bind(now)
        .execute(&mut **tx)
        .await?;
        id
    };

    let stale_cycles = sqlx::query(
        "SELECT id, desired_micros, proration_json FROM entitlement_cycles WHERE entitlement_id = $1 AND id <> $2 AND status = 'active'",
    )
    .bind(&entitlement_id)
    .bind(&cycle_id)
    .fetch_all(&mut **tx)
    .await?;
    let mut ledger_delta = 0_i64;
    for stale in stale_cycles {
        let stale_id: String = stale.try_get("id")?;
        let desired: i64 = stale.try_get("desired_micros")?;
        let proration: Option<String> = stale.try_get("proration_json")?;
        ledger_delta = ledger_delta.saturating_add(
            adjust_entitlement_cycle(
                tx,
                input.account_id,
                &stale_id,
                desired,
                "expired",
                true,
                proration.as_deref(),
                &input.source,
                reconciliation_id,
                now,
            )
            .await?,
        );
    }
    ledger_delta = ledger_delta.saturating_add(
        adjust_entitlement_cycle(
            tx,
            input.account_id,
            &cycle_id,
            input.desired_micros,
            "active",
            false,
            input.proration_json.as_deref(),
            &input.source,
            reconciliation_id,
            now,
        )
        .await?,
    );
    sqlx::query(
        "UPDATE subscription_entitlements SET status = 'active', version = $1, current_cycle_id = $2, replaced_by_entitlement_id = NULL, updated_at = $3 WHERE id = $4",
    )
    .bind(input.version)
    .bind(&cycle_id)
    .bind(now)
    .bind(&entitlement_id)
    .execute(&mut **tx)
    .await?;
    Ok(EntitlementReconcileResult {
        entitlement: entitlement_result_view(tx, &entitlement_id).await?,
        ledger_delta: micros_to_decimal_string(ledger_delta),
        replaced_entitlement_id: None,
    })
}

async fn cancel_entitlement_snapshot(
    tx: &mut Transaction<'_, Any>,
    tenant_id: &str,
    input: &CancelEntitlementInput,
    reconciliation_id: Uuid,
    now: i64,
) -> Result<EntitlementReconcileResult, AppError> {
    let row = sqlx::query(
        "SELECT id, account_id, status, version, current_cycle_id FROM subscription_entitlements WHERE tenant_id = $1 AND provider = $2 AND external_subscription_id = $3",
    )
    .bind(tenant_id)
    .bind(&input.provider)
    .bind(&input.external_subscription_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(AppError::NotFound)?;
    let entitlement_id: String = row.try_get("id")?;
    let account_id = parse_uuid(row.try_get("account_id")?)?;
    lock_entitlement_account(tx, account_id, tenant_id, None).await?;
    let row = sqlx::query(
        "SELECT status, version, current_cycle_id FROM subscription_entitlements WHERE id = $1",
    )
    .bind(&entitlement_id)
    .fetch_one(&mut **tx)
    .await?;
    let version: i64 = row.try_get("version")?;
    let status: String = row.try_get("status")?;
    let cycle_id: String = row.try_get("current_cycle_id")?;
    if status == "replaced" || input.version <= version {
        return Err(AppError::Conflict(format!(
            "cancellation version must be greater than {version}"
        )));
    }
    let cycle = sqlx::query(
        "SELECT external_cycle_id, desired_micros, proration_json FROM entitlement_cycles WHERE id = $1",
    )
    .bind(&cycle_id)
    .fetch_one(&mut **tx)
    .await?;
    let external_cycle_id: String = cycle.try_get("external_cycle_id")?;
    if input
        .external_cycle_id
        .as_deref()
        .is_some_and(|expected| expected != external_cycle_id)
    {
        return Err(AppError::Conflict(
            "cancellation billing-cycle identity is stale".into(),
        ));
    }
    let desired: i64 = cycle.try_get("desired_micros")?;
    let proration: Option<String> = cycle.try_get("proration_json")?;
    let delta = adjust_entitlement_cycle(
        tx,
        account_id,
        &cycle_id,
        desired,
        "cancelled",
        true,
        proration.as_deref(),
        &input.source,
        reconciliation_id,
        now,
    )
    .await?;
    sqlx::query(
        "UPDATE subscription_entitlements SET status = 'cancelled', version = $1, updated_at = $2 WHERE id = $3",
    )
    .bind(input.version)
    .bind(now)
    .bind(&entitlement_id)
    .execute(&mut **tx)
    .await?;
    Ok(EntitlementReconcileResult {
        entitlement: entitlement_result_view(tx, &entitlement_id).await?,
        ledger_delta: micros_to_decimal_string(delta),
        replaced_entitlement_id: None,
    })
}

async fn replace_entitlement_snapshot(
    tx: &mut Transaction<'_, Any>,
    tenant_id: &str,
    input: &ReplaceEntitlementInput,
    reconciliation_id: Uuid,
    now: i64,
) -> Result<EntitlementReconcileResult, AppError> {
    let old = sqlx::query(
        "SELECT id, account_id, status, version, current_cycle_id FROM subscription_entitlements WHERE tenant_id = $1 AND provider = $2 AND external_subscription_id = $3",
    )
    .bind(tenant_id)
    .bind(&input.provider)
    .bind(&input.external_subscription_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(AppError::NotFound)?;
    let old_id: String = old.try_get("id")?;
    let account_id = parse_uuid(old.try_get("account_id")?)?;
    lock_entitlement_account(tx, account_id, tenant_id, Some(&input.replacement.currency)).await?;
    let old = sqlx::query(
        "SELECT status, version, current_cycle_id FROM subscription_entitlements WHERE id = $1",
    )
    .bind(&old_id)
    .fetch_one(&mut **tx)
    .await?;
    let old_status: String = old.try_get("status")?;
    let old_version: i64 = old.try_get("version")?;
    let old_cycle_id: String = old.try_get("current_cycle_id")?;
    if old_status == "replaced" || input.version <= old_version {
        return Err(AppError::Conflict(format!(
            "replacement version must be greater than {old_version}"
        )));
    }
    if input.replacement.tenant_external_id != input.tenant_external_id
        || input.replacement.account_id != account_id
    {
        return Err(AppError::Conflict(
            "replacement must retain the tenant and stable credit account".into(),
        ));
    }
    if input.replacement.provider == input.provider
        && input.replacement.external_subscription_id == input.external_subscription_id
    {
        return Err(AppError::Conflict(
            "replacement subscription identity must be different".into(),
        ));
    }
    if sqlx::query(
        "SELECT id FROM subscription_entitlements WHERE tenant_id = $1 AND provider = $2 AND external_subscription_id = $3",
    )
    .bind(tenant_id)
    .bind(&input.replacement.provider)
    .bind(&input.replacement.external_subscription_id)
    .fetch_optional(&mut **tx)
    .await?
    .is_some()
    {
        return Err(AppError::Conflict(
            "replacement subscription identity already exists".into(),
        ));
    }
    let old_cycle =
        sqlx::query("SELECT desired_micros, proration_json FROM entitlement_cycles WHERE id = $1")
            .bind(&old_cycle_id)
            .fetch_one(&mut **tx)
            .await?;
    let old_desired: i64 = old_cycle.try_get("desired_micros")?;
    let old_proration: Option<String> = old_cycle.try_get("proration_json")?;
    let mut ledger_delta = adjust_entitlement_cycle(
        tx,
        account_id,
        &old_cycle_id,
        old_desired,
        "replaced",
        true,
        old_proration.as_deref(),
        &input.source,
        reconciliation_id,
        now,
    )
    .await?;

    let new_id = Uuid::now_v7().to_string();
    let new_cycle_id = Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO subscription_entitlements (id, tenant_id, account_id, provider, external_subscription_id, status, version, current_cycle_id, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, 'active', $6, $7, $8, $9)",
    )
    .bind(&new_id)
    .bind(tenant_id)
    .bind(account_id.to_string())
    .bind(&input.replacement.provider)
    .bind(&input.replacement.external_subscription_id)
    .bind(input.replacement.version)
    .bind(&new_cycle_id)
    .bind(now)
    .bind(now)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "INSERT INTO entitlement_cycles (id, entitlement_id, external_cycle_id, period_start, period_end, currency, desired_micros, funded_micros, consumed_micros, status, proration_json, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, $6, 0, 0, 0, 'active', $7, $8, $9)",
    )
    .bind(&new_cycle_id)
    .bind(&new_id)
    .bind(&input.replacement.external_cycle_id)
    .bind(input.replacement.period_start)
    .bind(input.replacement.period_end)
    .bind(input.replacement.currency.to_uppercase())
    .bind(input.replacement.proration_json.as_deref())
    .bind(now)
    .bind(now)
    .execute(&mut **tx)
    .await?;
    ledger_delta = ledger_delta.saturating_add(
        adjust_entitlement_cycle(
            tx,
            account_id,
            &new_cycle_id,
            input.replacement.desired_micros,
            "active",
            false,
            input.replacement.proration_json.as_deref(),
            &input.source,
            reconciliation_id,
            now,
        )
        .await?,
    );
    sqlx::query(
        "UPDATE subscription_entitlements SET status = 'replaced', version = $1, replaced_by_entitlement_id = $2, updated_at = $3 WHERE id = $4",
    )
    .bind(input.version)
    .bind(&new_id)
    .bind(now)
    .bind(&old_id)
    .execute(&mut **tx)
    .await?;
    Ok(EntitlementReconcileResult {
        entitlement: entitlement_result_view(tx, &new_id).await?,
        ledger_delta: micros_to_decimal_string(ledger_delta),
        replaced_entitlement_id: Some(parse_uuid(old_id)?),
    })
}

fn entitlement_view(row: AnyRow) -> Result<EntitlementView, AppError> {
    let funded_micros: i64 = row.try_get("funded_micros")?;
    let consumed_micros: i64 = row.try_get("consumed_micros")?;
    Ok(EntitlementView {
        entitlement_id: parse_uuid(row.try_get("entitlement_id")?)?,
        cycle_id: parse_uuid(row.try_get("cycle_id")?)?,
        tenant_external_id: row.try_get("tenant_external_id")?,
        account_id: parse_uuid(row.try_get("account_id")?)?,
        provider: row.try_get("provider")?,
        external_subscription_id: row.try_get("external_subscription_id")?,
        external_cycle_id: row.try_get("external_cycle_id")?,
        period_start: row.try_get("period_start")?,
        period_end: row.try_get("period_end")?,
        currency: row.try_get("currency")?,
        desired: micros_to_decimal_string(row.try_get("desired_micros")?),
        consumed: micros_to_decimal_string(consumed_micros),
        remaining: micros_to_decimal_string(funded_micros.saturating_sub(consumed_micros).max(0)),
        status: row.try_get("status")?,
        version: row.try_get("version")?,
        replaced_by_entitlement_id: row
            .try_get::<Option<String>, _>("replaced_by_entitlement_id")?
            .map(parse_uuid)
            .transpose()?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

fn validate_entitlement_identity(value: &str, field: &str) -> Result<(), AppError> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 200
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':'))
    {
        return Err(AppError::BadRequest(format!(
            "{field} must contain 1 to 200 safe ASCII characters"
        )));
    }
    Ok(())
}

fn validate_reconcile_entitlement(input: &ReconcileEntitlementInput) -> Result<(), AppError> {
    validate_entitlement_identity(&input.tenant_external_id, "tenant_external_id")?;
    validate_entitlement_identity(&input.provider, "provider")?;
    validate_entitlement_identity(&input.external_subscription_id, "external_subscription_id")?;
    validate_entitlement_identity(&input.external_cycle_id, "external_cycle_id")?;
    validate_currency(&input.currency)?;
    if input.period_start < 0 || input.period_end <= input.period_start {
        return Err(AppError::BadRequest(
            "billing period_end must be after a non-negative period_start".into(),
        ));
    }
    if input.desired_micros < 0 || input.version <= 0 {
        return Err(AppError::BadRequest(
            "desired entitlement cannot be negative and version must be positive".into(),
        ));
    }
    let source = input.source.trim();
    if source.is_empty() || source.len() > 200 || source.chars().any(char::is_control) {
        return Err(AppError::BadRequest(
            "source must contain 1 to 200 non-control characters".into(),
        ));
    }
    if input
        .proration_json
        .as_deref()
        .is_some_and(|value| value.len() > 4_096)
    {
        return Err(AppError::BadRequest(
            "proration metadata exceeds 4 KiB".into(),
        ));
    }
    Ok(())
}

fn validate_entitlement_operation(operation: &EntitlementOperation) -> Result<(), AppError> {
    match operation {
        EntitlementOperation::Reconcile(input) => validate_reconcile_entitlement(input),
        EntitlementOperation::Cancel(input) => {
            validate_entitlement_identity(&input.tenant_external_id, "tenant_external_id")?;
            validate_entitlement_identity(&input.provider, "provider")?;
            validate_entitlement_identity(
                &input.external_subscription_id,
                "external_subscription_id",
            )?;
            if let Some(cycle) = &input.external_cycle_id {
                validate_entitlement_identity(cycle, "external_cycle_id")?;
            }
            if input.version <= 0
                || input.source.trim().is_empty()
                || input.source.len() > 200
                || input.source.chars().any(char::is_control)
            {
                return Err(AppError::BadRequest(
                    "cancellation requires a positive version and valid source".into(),
                ));
            }
            Ok(())
        }
        EntitlementOperation::Replace(input) => {
            validate_entitlement_identity(&input.tenant_external_id, "tenant_external_id")?;
            validate_entitlement_identity(&input.provider, "provider")?;
            validate_entitlement_identity(
                &input.external_subscription_id,
                "external_subscription_id",
            )?;
            if input.version <= 0
                || input.source.trim().is_empty()
                || input.source.len() > 200
                || input.source.chars().any(char::is_control)
            {
                return Err(AppError::BadRequest(
                    "replacement requires a positive version and valid source".into(),
                ));
            }
            validate_reconcile_entitlement(&input.replacement)
        }
    }
}

fn canonicalize_entitlement_operation(
    operation: &mut EntitlementOperation,
) -> Result<(), AppError> {
    fn canonicalize(input: &mut ReconcileEntitlementInput) -> Result<(), AppError> {
        if let Some(raw) = input.proration_json.as_mut() {
            let value: serde_json::Value = serde_json::from_str(raw).map_err(|_| {
                AppError::BadRequest("proration metadata must be valid JSON".into())
            })?;
            *raw = serde_json::to_string(&value).map_err(|_| AppError::Internal)?;
        }
        Ok(())
    }
    match operation {
        EntitlementOperation::Reconcile(input) => canonicalize(input),
        EntitlementOperation::Cancel(_) => Ok(()),
        EntitlementOperation::Replace(input) => canonicalize(&mut input.replacement),
    }
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
        || plugin_id.len() > 120
        || !plugin_id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(AppError::BadRequest("invalid plugin id for KV".into()));
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

fn validate_request_filter(filter: &RequestListFilter) -> Result<(), AppError> {
    if filter
        .status
        .as_deref()
        .is_some_and(|value| !matches!(value, "success" | "error" | "pending"))
    {
        return Err(AppError::BadRequest(
            "status must be success, error, or pending".into(),
        ));
    }
    if filter
        .from_created_at
        .zip(filter.to_created_at)
        .is_some_and(|(from, to)| from > to)
    {
        return Err(AppError::BadRequest(
            "from_created_at must not be after to_created_at".into(),
        ));
    }
    validate_numeric_range(
        "duration_ms",
        filter.min_duration_ms,
        filter.max_duration_ms,
    )?;
    validate_numeric_range("cost", filter.min_cost_micros, filter.max_cost_micros)?;
    for (name, value) in [
        ("model", filter.model.as_deref()),
        ("protocol", filter.protocol.as_deref()),
        ("error_code", filter.error_code.as_deref()),
        ("key_alias", filter.key_alias.as_deref()),
        ("principal", filter.principal.as_deref()),
    ] {
        if value.is_some_and(|value| {
            value.is_empty() || value.len() > 200 || value.chars().any(char::is_control)
        }) {
            return Err(AppError::BadRequest(format!(
                "{name} must contain 1 to 200 non-control characters"
            )));
        }
    }
    Ok(())
}

fn validate_stats_filter(filter: &StatsFilter) -> Result<(), AppError> {
    let from = filter
        .from_created_at
        .ok_or_else(|| AppError::BadRequest("from_created_at is required for statistics".into()))?;
    let to = filter
        .to_created_at
        .ok_or_else(|| AppError::BadRequest("to_created_at is required for statistics".into()))?;
    if from < 0 || to < 0 || from > to {
        return Err(AppError::BadRequest(
            "statistics require a valid non-negative from_created_at/to_created_at range".into(),
        ));
    }
    if to.saturating_sub(from) > MAX_STATS_RANGE_MILLIS {
        return Err(AppError::BadRequest(
            "statistics range must not exceed 93 days".into(),
        ));
    }
    if filter
        .status
        .as_deref()
        .is_some_and(|value| !matches!(value, "success" | "error" | "pending"))
    {
        return Err(AppError::BadRequest(
            "status must be success, error, or pending".into(),
        ));
    }
    validate_numeric_range(
        "duration_ms",
        filter.min_duration_ms,
        filter.max_duration_ms,
    )?;
    validate_numeric_range("cost", filter.min_cost_micros, filter.max_cost_micros)?;
    for (name, value) in [
        ("model", filter.model.as_deref()),
        ("protocol", filter.protocol.as_deref()),
        ("error_code", filter.error_code.as_deref()),
        ("key_alias", filter.key_alias.as_deref()),
        ("principal", filter.principal.as_deref()),
    ] {
        if value.is_some_and(|value| {
            value.trim().is_empty() || value.len() > 200 || value.chars().any(char::is_control)
        }) {
            return Err(AppError::BadRequest(format!(
                "{name} must contain 1 to 200 non-control characters"
            )));
        }
    }
    Ok(())
}

fn validate_numeric_range(
    name: &str,
    minimum: Option<i64>,
    maximum: Option<i64>,
) -> Result<(), AppError> {
    if minimum.is_some_and(|value| value < 0) || maximum.is_some_and(|value| value < 0) {
        return Err(AppError::BadRequest(format!(
            "{name} bounds must not be negative"
        )));
    }
    if minimum
        .zip(maximum)
        .is_some_and(|(minimum, maximum)| minimum > maximum)
    {
        return Err(AppError::BadRequest(format!(
            "minimum {name} must not exceed maximum {name}"
        )));
    }
    Ok(())
}

fn search_prefix(value: Option<&str>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    let mut escaped = String::with_capacity(value.len() + 1);
    for character in value.trim().to_lowercase().chars() {
        if matches!(character, '%' | '_' | '\\') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped.push('%');
    escaped
}

fn cursor_id(filter: &RequestListFilter) -> String {
    filter
        .before_id
        .map(|id| id.to_string())
        .unwrap_or_else(|| "ffffffff-ffff-ffff-ffff-ffffffffffff".to_owned())
}

fn validate_service_token_input(input: &CreateServiceTokenInput) -> Result<(), AppError> {
    if input.name.trim().is_empty() || input.name.len() > 120 {
        return Err(AppError::BadRequest(
            "service token name must contain 1 to 120 characters".into(),
        ));
    }
    if input.scopes.is_empty() || input.scopes.len() > 32 {
        return Err(AppError::BadRequest(
            "service token must contain 1 to 32 scopes".into(),
        ));
    }
    if input.scopes.iter().any(|scope| {
        scope.is_empty()
            || scope.len() > 80
            || !scope.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'_' | b'-' | b'*')
            })
    }) {
        return Err(AppError::BadRequest(
            "service token scopes contain unsupported characters".into(),
        ));
    }
    if input
        .tenant_external_id
        .as_deref()
        .is_some_and(|tenant| tenant.trim().is_empty() || tenant.len() > 200)
    {
        return Err(AppError::BadRequest(
            "tenant_external_id must contain 1 to 200 characters".into(),
        ));
    }
    Ok(())
}

fn validate_upstream_account_name(name: &str) -> Result<(), AppError> {
    let name = name.trim();
    if name.is_empty() || name.len() > 200 || name.chars().any(char::is_control) {
        return Err(AppError::BadRequest(
            "upstream provider name must contain 1 to 200 non-control characters".into(),
        ));
    }
    Ok(())
}

fn validate_policy_budgets(policy: &KeyPolicy) -> Result<(), AppError> {
    if policy.requests_per_minute == 0
        || policy.tokens_per_minute == 0
        || policy.max_concurrency == 0
    {
        return Err(AppError::BadRequest(
            "RPM, TPM, and maximum concurrency must be positive".into(),
        ));
    }
    if policy.allowed_models.len() > 500
        || policy.allowed_models.iter().any(|model| {
            model.trim().is_empty() || model.len() > 200 || model.chars().any(char::is_control)
        })
    {
        return Err(AppError::BadRequest(
            "allowed models must contain at most 500 non-empty model names".into(),
        ));
    }
    for value in [
        policy.daily_budget.as_deref(),
        policy.weekly_budget.as_deref(),
        policy.lifetime_budget.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        let decimal = Decimal::from_str_exact(value)
            .map_err(|_| AppError::BadRequest("key budgets must be decimal strings".into()))?;
        if decimal.is_sign_negative() {
            return Err(AppError::BadRequest(
                "key budgets cannot be negative".into(),
            ));
        }
        decimal_to_micros(decimal)?;
    }
    Ok(())
}

fn validate_key_input(input: &CreateKeyInput) -> Result<(), AppError> {
    for (field, value) in [
        ("tenant_external_id", input.tenant_external_id.as_str()),
        (
            "principal_external_id",
            input.principal_external_id.as_str(),
        ),
        ("alias", input.alias.as_str()),
    ] {
        let value = value.trim();
        if value.is_empty() || value.len() > 200 || value.chars().any(char::is_control) {
            return Err(AppError::BadRequest(format!(
                "{field} must contain 1 to 200 non-control characters"
            )));
        }
    }
    validate_policy_budgets(&input.policy)
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

fn validate_token_usage(usage: &TokenUsage) -> Result<(), AppError> {
    let values = [
        usage.input_tokens,
        usage.cached_input_tokens,
        usage.cache_write_tokens,
        usage.output_tokens,
    ];
    if values
        .into_iter()
        .any(|tokens| !(0..=1_000_000_000).contains(&tokens))
    {
        return Err(AppError::BadRequest(
            "upstream token usage is outside the supported range".into(),
        ));
    }
    if let Some(tier) = usage.service_tier.as_deref() {
        validate_service_tier(tier)?;
    }
    Ok(())
}

async fn lock_key_budget_state(
    tx: &mut Transaction<'_, Any>,
    key_id: Uuid,
    now: i64,
) -> Result<(i64, i64), AppError> {
    let key_id = key_id.to_string();
    sqlx::query(
        "INSERT INTO key_budget_state (key_id, settled_lifetime_micros, reserved_micros, updated_at) SELECT $1, 0, 0, $2 WHERE EXISTS (SELECT 1 FROM key_records WHERE id = $3) ON CONFLICT(key_id) DO NOTHING",
    )
    .bind(&key_id)
    .bind(now)
    .bind(&key_id)
    .execute(&mut **tx)
    .await?;
    let locked = sqlx::query("UPDATE key_budget_state SET updated_at = $1 WHERE key_id = $2")
        .bind(now)
        .bind(&key_id)
        .execute(&mut **tx)
        .await?;
    if locked.rows_affected() != 1 {
        return Err(AppError::NotFound);
    }
    let row = sqlx::query(
        "SELECT settled_lifetime_micros, reserved_micros FROM key_budget_state WHERE key_id = $1",
    )
    .bind(key_id)
    .fetch_one(&mut **tx)
    .await?;
    Ok((
        row.try_get("settled_lifetime_micros")?,
        row.try_get("reserved_micros")?,
    ))
}

async fn key_budget_daily_settled(
    tx: &mut Transaction<'_, Any>,
    key_id: Uuid,
    now: i64,
) -> Result<i64, AppError> {
    Ok(sqlx::query(
        "SELECT COALESCE((SELECT settled_micros FROM key_budget_daily_rollups WHERE key_id = $1 AND day_bucket = $2), 0) AS amount",
    )
    .bind(key_id.to_string())
    .bind(now / 86_400_000)
    .fetch_one(&mut **tx)
    .await?
    .try_get("amount")?)
}

async fn key_budget_rolling_weekly_settled(
    tx: &mut Transaction<'_, Any>,
    key_id: Uuid,
    now: i64,
) -> Result<i64, AppError> {
    let cutoff = now.saturating_sub(7 * 86_400_000);
    let first_full_day = cutoff / 86_400_000 + 1;
    let first_full_day_at = first_full_day.saturating_mul(86_400_000);
    Ok(sqlx::query(
        "SELECT CAST(COALESCE((SELECT SUM(settled_micros) FROM key_budget_daily_rollups WHERE key_id = $1 AND day_bucket >= $2), 0) + COALESCE((SELECT SUM(amount_micros) FROM key_budget_usage_events WHERE key_id = $3 AND settled_at >= $4 AND settled_at < $5), 0) AS BIGINT) AS amount",
    )
    .bind(key_id.to_string())
    .bind(first_full_day)
    .bind(key_id.to_string())
    .bind(cutoff)
    .bind(first_full_day_at)
    .fetch_one(&mut **tx)
    .await?
    .try_get("amount")?)
}

fn price_token_usage(reservation: &UsageReservation, usage: &TokenUsage) -> Result<i64, AppError> {
    let fallback = ModelPriceTier {
        service_tier: "default".to_owned(),
        input_micros_per_million: reservation.input_micros_per_million,
        cached_input_micros_per_million: reservation.input_micros_per_million,
        cache_write_micros_per_million: reservation.input_micros_per_million,
        output_micros_per_million: reservation.output_micros_per_million,
        source: "legacy-snapshot".to_owned(),
    };
    let requested = usage.service_tier.as_deref().unwrap_or("default");
    let exact = reservation
        .price_tiers
        .iter()
        .find(|tier| tier.service_tier == requested);
    let conservative;
    let tier = if let Some(exact) = exact {
        exact
    } else if reservation.price_tiers.is_empty() {
        &fallback
    } else {
        conservative = ModelPriceTier {
            service_tier: requested.to_owned(),
            input_micros_per_million: reservation
                .price_tiers
                .iter()
                .map(|tier| tier.input_micros_per_million)
                .max()
                .unwrap_or(fallback.input_micros_per_million),
            cached_input_micros_per_million: reservation
                .price_tiers
                .iter()
                .map(|tier| tier.cached_input_micros_per_million)
                .max()
                .unwrap_or(fallback.cached_input_micros_per_million),
            cache_write_micros_per_million: reservation
                .price_tiers
                .iter()
                .map(|tier| tier.cache_write_micros_per_million)
                .max()
                .unwrap_or(fallback.cache_write_micros_per_million),
            output_micros_per_million: reservation
                .price_tiers
                .iter()
                .map(|tier| tier.output_micros_per_million)
                .max()
                .unwrap_or(fallback.output_micros_per_million),
            source: "conservative-snapshot".to_owned(),
        };
        &conservative
    };
    [
        priced_tokens(usage.input_tokens, tier.input_micros_per_million),
        priced_tokens(
            usage.cached_input_tokens,
            tier.cached_input_micros_per_million,
        ),
        priced_tokens(
            usage.cache_write_tokens,
            tier.cache_write_micros_per_million,
        ),
        priced_tokens(usage.output_tokens, tier.output_micros_per_million),
    ]
    .into_iter()
    .try_fold(0_i64, i64::checked_add)
    .ok_or(AppError::Internal)
}

#[allow(clippy::too_many_arguments)]
async fn upsert_price_tier(
    tx: &mut Transaction<'_, Any>,
    model: &str,
    currency: &str,
    service_tier: &str,
    input_micros: i64,
    cached_input_micros: i64,
    cache_write_micros: i64,
    output_micros: i64,
    source: &str,
    now: i64,
    cache_price_estimated: bool,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO model_price_tiers (id, model, currency, service_tier, input_micros_per_million, cached_input_micros_per_million, cache_write_micros_per_million, output_micros_per_million, source, updated_at, cache_price_estimated) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) ON CONFLICT(model, currency, service_tier) DO UPDATE SET input_micros_per_million = excluded.input_micros_per_million, cached_input_micros_per_million = excluded.cached_input_micros_per_million, cache_write_micros_per_million = excluded.cache_write_micros_per_million, output_micros_per_million = excluded.output_micros_per_million, source = excluded.source, updated_at = excluded.updated_at, cache_price_estimated = excluded.cache_price_estimated",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(model)
    .bind(currency)
    .bind(service_tier)
    .bind(input_micros)
    .bind(cached_input_micros)
    .bind(cache_write_micros)
    .bind(output_micros)
    .bind(source)
    .bind(now)
    .bind(i64::from(cache_price_estimated))
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn parse_uuid(value: String) -> Result<Uuid, AppError> {
    Uuid::parse_str(&value).map_err(|_| AppError::Internal)
}

fn generation_job_view(row: AnyRow) -> Result<GenerationJobView, AppError> {
    let result_json: Option<String> = row.try_get("result_json")?;
    Ok(GenerationJobView {
        job_id: parse_uuid(row.try_get("id")?)?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
        completed_at: row.try_get("completed_at")?,
        model: row.try_get("public_model")?,
        driver: row.try_get("driver")?,
        status: row.try_get("status")?,
        upstream_job_id: row.try_get("upstream_job_id")?,
        estimated_units: row.try_get("estimated_units")?,
        billed_units: row.try_get("billed_units")?,
        cost: micros_to_decimal_string(row.try_get("cost_micros")?),
        error_code: row.try_get("error_code")?,
        result: result_json
            .map(|value| serde_json::from_str(&value).map_err(|_| AppError::Internal))
            .transpose()?,
    })
}

fn generation_update_claimed(result: AnyQueryResult) -> Result<(), AppError> {
    if result.rows_affected() == 1 {
        Ok(())
    } else {
        Err(AppError::NotFound)
    }
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn replacement_for_gap(
    current: &str,
    replacement: Option<&str>,
) -> Result<Option<String>, AppError> {
    let Some(replacement) = replacement else {
        return Ok(None);
    };
    if current.starts_with("gap://") || current == replacement {
        return Ok(Some(replacement.to_owned()));
    }
    Err(AppError::BadRequest(
        "archive import refused to overwrite an existing object".into(),
    ))
}

pub fn unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after Unix epoch")
        .as_millis() as i64
}

struct Migration {
    version: i64,
    name: &'static str,
    sql: &'static str,
}

const SQLITE_MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "initial schema",
        sql: include_str!("../migrations/sqlite/0001_initial.sql"),
    },
    Migration {
        version: 2,
        name: "high volume query indexes",
        sql: include_str!("../migrations/sqlite/0002_query_indexes.sql"),
    },
    Migration {
        version: 3,
        name: "scoped service credentials",
        sql: include_str!("../migrations/common/0003_service_tokens.sql"),
    },
    Migration {
        version: 4,
        name: "request event stream",
        sql: include_str!("../migrations/sqlite/0004_request_events.sql"),
    },
    Migration {
        version: 5,
        name: "asynchronous generation jobs",
        sql: include_str!("../migrations/sqlite/0005_generation_jobs.sql"),
    },
    Migration {
        version: 6,
        name: "idempotent key provisioning",
        sql: include_str!("../migrations/sqlite/0006_key_provisioning.sql"),
    },
    Migration {
        version: 7,
        name: "idempotent grant reversals",
        sql: include_str!("../migrations/sqlite/0007_grant_reversals.sql"),
    },
    Migration {
        version: 8,
        name: "bounded plugin KV",
        sql: include_str!("../migrations/sqlite/0008_plugin_kv.sql"),
    },
    Migration {
        version: 9,
        name: "structured conversation hints",
        sql: include_str!("../migrations/sqlite/0009_structured_conversation_hints.sql"),
    },
    Migration {
        version: 10,
        name: "operator aggregate indexes",
        sql: include_str!("../migrations/sqlite/0010_operator_aggregate_indexes.sql"),
    },
    Migration {
        version: 11,
        name: "legacy key credentials",
        sql: include_str!("../migrations/sqlite/0011_legacy_key_credentials.sql"),
    },
    Migration {
        version: 12,
        name: "tenant scoped idempotency",
        sql: include_str!("../migrations/sqlite/0012_tenant_idempotency.sql"),
    },
    Migration {
        version: 13,
        name: "generation price snapshots",
        sql: include_str!("../migrations/common/0013_generation_price_snapshot.sql"),
    },
    Migration {
        version: 14,
        name: "idempotent credential rotation",
        sql: include_str!("../migrations/common/0014_credential_rotation_idempotency.sql"),
    },
    Migration {
        version: 15,
        name: "idempotent generation jobs",
        sql: include_str!("../migrations/common/0015_generation_job_idempotency.sql"),
    },
    Migration {
        version: 16,
        name: "conversation upstream response ids",
        sql: include_str!("../migrations/common/0016_conversation_upstream_response_ids.sql"),
    },
    Migration {
        version: 17,
        name: "subscription entitlement reconciliation",
        sql: include_str!("../migrations/common/0017_subscription_entitlements.sql"),
    },
    Migration {
        version: 18,
        name: "model price service and cache tiers",
        sql: include_str!("../migrations/common/0018_model_price_tiers.sql"),
    },
    Migration {
        version: 19,
        name: "session archive import provenance",
        sql: include_str!("../migrations/common/0019_session_archive_import.sql"),
    },
    Migration {
        version: 20,
        name: "bounded observability query indexes",
        sql: include_str!("../migrations/sqlite/0020_observability_indexes.sql"),
    },
    Migration {
        version: 21,
        name: "global request and event locators",
        sql: include_str!("../migrations/common/0021_request_locators.sql"),
    },
    Migration {
        version: 22,
        name: "transactional budget rollups",
        sql: include_str!("../migrations/common/0022_budget_rollups.sql"),
    },
    Migration {
        version: 23,
        name: "generation daily aggregates",
        sql: include_str!("../migrations/common/0023_generation_daily_aggregates.sql"),
    },
];

const POSTGRES_MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "initial partitioned schema",
        sql: include_str!("../migrations/postgres/0001_initial.sql"),
    },
    Migration {
        version: 2,
        name: "high volume query indexes",
        sql: include_str!("../migrations/postgres/0002_query_indexes.sql"),
    },
    Migration {
        version: 3,
        name: "scoped service credentials",
        sql: include_str!("../migrations/common/0003_service_tokens.sql"),
    },
    Migration {
        version: 4,
        name: "partitioned request event stream",
        sql: include_str!("../migrations/postgres/0004_request_events.sql"),
    },
    Migration {
        version: 5,
        name: "asynchronous generation jobs",
        sql: include_str!("../migrations/postgres/0005_generation_jobs.sql"),
    },
    Migration {
        version: 6,
        name: "idempotent key provisioning",
        sql: include_str!("../migrations/postgres/0006_key_provisioning.sql"),
    },
    Migration {
        version: 7,
        name: "idempotent grant reversals",
        sql: include_str!("../migrations/postgres/0007_grant_reversals.sql"),
    },
    Migration {
        version: 8,
        name: "bounded plugin KV",
        sql: include_str!("../migrations/postgres/0008_plugin_kv.sql"),
    },
    Migration {
        version: 9,
        name: "structured conversation hints",
        sql: include_str!("../migrations/postgres/0009_structured_conversation_hints.sql"),
    },
    Migration {
        version: 10,
        name: "operator aggregate indexes",
        sql: include_str!("../migrations/postgres/0010_operator_aggregate_indexes.sql"),
    },
    Migration {
        version: 11,
        name: "legacy key credentials",
        sql: include_str!("../migrations/postgres/0011_legacy_key_credentials.sql"),
    },
    Migration {
        version: 12,
        name: "tenant scoped idempotency",
        sql: include_str!("../migrations/postgres/0012_tenant_idempotency.sql"),
    },
    Migration {
        version: 13,
        name: "generation price snapshots",
        sql: include_str!("../migrations/common/0013_generation_price_snapshot.sql"),
    },
    Migration {
        version: 14,
        name: "idempotent credential rotation",
        sql: include_str!("../migrations/common/0014_credential_rotation_idempotency.sql"),
    },
    Migration {
        version: 15,
        name: "idempotent generation jobs",
        sql: include_str!("../migrations/common/0015_generation_job_idempotency.sql"),
    },
    Migration {
        version: 16,
        name: "conversation upstream response ids",
        sql: include_str!("../migrations/common/0016_conversation_upstream_response_ids.sql"),
    },
    Migration {
        version: 17,
        name: "subscription entitlement reconciliation",
        sql: include_str!("../migrations/common/0017_subscription_entitlements.sql"),
    },
    Migration {
        version: 18,
        name: "model price service and cache tiers",
        sql: include_str!("../migrations/common/0018_model_price_tiers.sql"),
    },
    Migration {
        version: 19,
        name: "session archive import provenance",
        sql: include_str!("../migrations/common/0019_session_archive_import.sql"),
    },
    Migration {
        version: 20,
        name: "bounded observability query indexes",
        sql: include_str!("../migrations/postgres/0020_observability_indexes.sql"),
    },
    Migration {
        version: 21,
        name: "global request and event locators",
        sql: include_str!("../migrations/common/0021_request_locators.sql"),
    },
    Migration {
        version: 22,
        name: "transactional budget rollups",
        sql: include_str!("../migrations/common/0022_budget_rollups.sql"),
    },
    Migration {
        version: 23,
        name: "generation daily aggregates",
        sql: include_str!("../migrations/common/0023_generation_daily_aggregates.sql"),
    },
];

const PARTITION_MAINTENANCE_SAVEPOINT: &str = "memeloop_partition_maintenance";

async fn maintain_postgres_partitions(
    connection: &mut AnyConnection,
) -> Result<PartitionMaintenanceReport, sqlx::Error> {
    let today = Utc::now().date_naive();
    let mut report = PartitionMaintenanceReport::default();
    for offset in 0..=8_u64 {
        let day = today
            .checked_add_days(Days::new(offset))
            .expect("partition date is representable");
        let next_day = day
            .checked_add_days(Days::new(1))
            .expect("partition end date is representable");
        let start = day
            .and_hms_opt(0, 0, 0)
            .expect("midnight is representable")
            .and_utc()
            .timestamp_millis();
        let end = next_day
            .and_hms_opt(0, 0, 0)
            .expect("midnight is representable")
            .and_utc()
            .timestamp_millis();
        let suffix = day.format("%Y%m%d");
        for table in ["request_records", "request_events"] {
            let partition = format!("{table}_{suffix}");
            let statement = format!(
                "CREATE TABLE IF NOT EXISTS {partition} PARTITION OF {table} FOR VALUES FROM ({start}) TO ({end})"
            );
            sqlx::query(&format!("SAVEPOINT {PARTITION_MAINTENANCE_SAVEPOINT}"))
                .execute(&mut *connection)
                .await?;
            match sqlx::query(&statement).execute(&mut *connection).await {
                Ok(_) => {
                    sqlx::query(&format!(
                        "RELEASE SAVEPOINT {PARTITION_MAINTENANCE_SAVEPOINT}"
                    ))
                    .execute(&mut *connection)
                    .await?;
                    report.ready_partitions += 1;
                }
                Err(error) => {
                    // A PostgreSQL statement error aborts the transaction until it is rolled back.
                    // Keep each partition DDL behind a savepoint so a blocked day cannot prevent
                    // the migration transaction from committing or later days from being created.
                    sqlx::query(&format!(
                        "ROLLBACK TO SAVEPOINT {PARTITION_MAINTENANCE_SAVEPOINT}"
                    ))
                    .execute(&mut *connection)
                    .await?;
                    sqlx::query(&format!(
                        "RELEASE SAVEPOINT {PARTITION_MAINTENANCE_SAVEPOINT}"
                    ))
                    .execute(&mut *connection)
                    .await?;

                    if !is_default_partition_overlap(&error) {
                        return Err(error);
                    }

                    tracing::warn!(
                        table,
                        %partition,
                        %day,
                        start,
                        end,
                        %error,
                        "partition creation skipped because its DEFAULT partition contains rows in the target range; rows were left unchanged and a later maintenance run will retry"
                    );
                    report.blocked_partitions.push(BlockedPartition {
                        table: table.to_owned(),
                        partition,
                        day,
                    });
                }
            }
        }
    }
    for table in ["request_records", "request_events"] {
        let statement =
            format!("CREATE TABLE IF NOT EXISTS {table}_default PARTITION OF {table} DEFAULT");
        sqlx::query(&statement).execute(&mut *connection).await?;
    }
    Ok(report)
}

fn is_default_partition_overlap(error: &sqlx::Error) -> bool {
    matches!(
        error,
        sqlx::Error::Database(database_error)
            if database_error.code().as_deref() == Some("23514")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn budget_rollup_migration_backfills_existing_usage_and_reservations() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("budget-backfill.db").display()
        );
        let database = Database::connect(&database_url).await.unwrap();
        for statement in [
            "CREATE TABLE schema_migrations (version BIGINT PRIMARY KEY, name TEXT NOT NULL, applied_at BIGINT NOT NULL)",
            "CREATE TABLE key_records (id TEXT PRIMARY KEY, updated_at BIGINT NOT NULL)",
            "CREATE TABLE credit_accounts (id TEXT PRIMARY KEY, updated_at BIGINT NOT NULL)",
            "CREATE TABLE usage_reservations (id TEXT PRIMARY KEY, account_id TEXT NOT NULL, key_id TEXT NOT NULL, reserved_micros BIGINT NOT NULL, status TEXT NOT NULL)",
            "CREATE TABLE ledger_entries (id TEXT PRIMARY KEY, account_id TEXT NOT NULL, key_id TEXT, kind TEXT NOT NULL, amount_micros BIGINT NOT NULL, currency TEXT NOT NULL, source TEXT NOT NULL, idempotency_key TEXT, created_at BIGINT NOT NULL, reference_entry_id TEXT, entitlement_cycle_id TEXT)",
        ] {
            sqlx::query(statement)
                .execute(&database.pool)
                .await
                .unwrap();
        }
        sqlx::query("INSERT INTO key_records VALUES ('key', 300)")
            .execute(&database.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO credit_accounts VALUES ('account', 300)")
            .execute(&database.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO usage_reservations VALUES ('active', 'account', 'key', 250, 'reserved'), ('settled-one', 'account', 'key', 400, 'settled'), ('settled-two', 'account', 'key', 600, 'settled')")
            .execute(&database.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO ledger_entries (id, account_id, key_id, kind, amount_micros, currency, source, created_at) VALUES ('usage-one', 'account', 'key', 'usage', -400, 'USD', 'settled-one', 100), ('grant', 'account', NULL, 'grant', 2000, 'USD', 'test', 150), ('usage-two', 'account', 'key', 'usage', -600, 'USD', 'settled-two', 200)")
            .execute(&database.pool)
            .await
            .unwrap();

        let mut transaction = database.pool.begin().await.unwrap();
        apply_migration_range(&mut transaction, SQLITE_MIGRATIONS, 22, 22)
            .await
            .unwrap();
        transaction.commit().await.unwrap();

        let state = sqlx::query("SELECT settled_lifetime_micros, reserved_micros FROM key_budget_state WHERE key_id = 'key'")
            .fetch_one(&database.pool)
            .await
            .unwrap();
        assert_eq!(state.get::<i64, _>("settled_lifetime_micros"), 1_000);
        assert_eq!(state.get::<i64, _>("reserved_micros"), 250);
        let account: i64 = sqlx::query(
            "SELECT settled_lifetime_micros FROM account_usage_state WHERE account_id = 'account'",
        )
        .fetch_one(&database.pool)
        .await
        .unwrap()
        .get("settled_lifetime_micros");
        assert_eq!(account, 1_000);
        let snapshot: i64 = sqlx::query(
            "SELECT account_usage_micros_snapshot FROM ledger_entries WHERE id = 'grant'",
        )
        .fetch_one(&database.pool)
        .await
        .unwrap()
        .get("account_usage_micros_snapshot");
        assert_eq!(snapshot, 400);
    }

    #[tokio::test]
    async fn rate_window_cleanup_is_composite_keyed_and_bounded() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("rate-window-cleanup.db").display()
        );
        let database = Database::connect(&database_url).await.unwrap();
        database.migrate().await.unwrap();
        let old = unix_millis().saturating_sub(3 * 86_400_000);
        sqlx::query("INSERT INTO rate_limit_windows (key_id, window_start, requests, tokens) VALUES ('b', $1, 1, 1), ('a', $2, 1, 1), ('c', $3, 1, 1), ('current', $4, 1, 1)")
            .bind(old)
            .bind(old)
            .bind(old.saturating_add(1))
            .bind(unix_millis())
            .execute(&database.pool)
            .await
            .unwrap();
        assert_eq!(database.delete_expired_rate_windows(2).await.unwrap(), 2);
        let remaining =
            sqlx::query("SELECT key_id FROM rate_limit_windows ORDER BY window_start, key_id")
                .fetch_all(&database.pool)
                .await
                .unwrap()
                .into_iter()
                .map(|row| row.get::<String, _>("key_id"))
                .collect::<Vec<_>>();
        assert_eq!(remaining, vec!["c", "current"]);
    }

    #[tokio::test]
    async fn concurrent_budget_reservations_and_settlement_replays_are_exact() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("budget-concurrency.db").display()
        );
        let database = Database::connect_with_max(&database_url, 8).await.unwrap();
        database.migrate().await.unwrap();
        let pepper = b"a budget test pepper longer than thirty-two bytes";
        let issued = database
            .create_key(
                CreateKeyInput {
                    tenant_external_id: "budget-concurrency".to_owned(),
                    principal_external_id: "member".to_owned(),
                    alias: "budget-concurrency".to_owned(),
                    currency: "USD".to_owned(),
                    policy: KeyPolicy {
                        tokens_per_minute: 10_000,
                        daily_budget: Some("0.001".to_owned()),
                        weekly_budget: Some("0.001".to_owned()),
                        lifetime_budget: Some("0.001".to_owned()),
                        ..KeyPolicy::default()
                    },
                    initial_balance: Decimal::ONE,
                    idempotency_key: None,
                },
                pepper,
            )
            .await
            .unwrap();
        let key = database
            .authenticate_key(&issued.key, pepper)
            .await
            .unwrap();
        let price = database
            .upsert_model_price("budget-concurrency", "USD", Decimal::ZERO, Decimal::ONE)
            .await
            .unwrap();
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
        let mut tasks = Vec::new();
        for _ in 0..2 {
            let database = database.clone();
            let key = key.clone();
            let price = price.clone();
            let barrier = barrier.clone();
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                database.reserve_usage(&key, &price, 0, 600).await
            }));
        }
        let mut reservation = None;
        let mut rejected = 0;
        for task in tasks {
            match task.await.unwrap() {
                Ok(value) => reservation = Some(value),
                Err(AppError::QuotaExceeded) => rejected += 1,
                result => panic!("unexpected reservation result: {result:?}"),
            }
        }
        assert_eq!(rejected, 1);
        let reservation = reservation.unwrap();
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(8));
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let database = database.clone();
            let reservation = reservation.clone();
            let barrier = barrier.clone();
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                database.settle_usage(&reservation, 0, 700).await
            }));
        }
        for task in tasks {
            assert_eq!(task.await.unwrap().unwrap(), 700);
        }
        let state = sqlx::query("SELECT settled_lifetime_micros, reserved_micros FROM key_budget_state WHERE key_id = $1")
            .bind(issued.key_id.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap();
        assert_eq!(state.get::<i64, _>("settled_lifetime_micros"), 700);
        assert_eq!(state.get::<i64, _>("reserved_micros"), 0);
        let ledger_count: i64 = sqlx::query(
            "SELECT COUNT(*) AS count FROM ledger_entries WHERE key_id = $1 AND kind = 'usage'",
        )
        .bind(issued.key_id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap()
        .get("count");
        assert_eq!(ledger_count, 1);
    }

    #[test]
    fn common_generation_stats_do_not_scan_generation_jobs() {
        assert!(!FILTERED_ACTIVITY_SOURCE_AGGREGATED.contains("FROM generation_jobs"));
        assert!(FILTERED_ACTIVITY_SOURCE_AGGREGATED.contains("generation_daily_aggregates"));
        assert!(FILTERED_ACTIVITY_SOURCE_AGGREGATED.contains("generation_stats_facts"));
        assert!(!FILTERED_ACTIVITY_SOURCE_GENERATION_FACTS.contains("FROM generation_jobs"));
        assert!(FILTERED_ACTIVITY_SOURCE_PENDING_GENERATION.contains("FROM generation_jobs"));
        assert!(FILTERED_ACTIVITY_SOURCE_PENDING_GENERATION.contains("g.created_at >= $3"));
        assert!(FILTERED_ACTIVITY_SOURCE_PENDING_GENERATION.contains("g.created_at <= $4"));
    }

    #[tokio::test]
    async fn sqlite_generation_aggregate_migration_backfills_once() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory
                .path()
                .join("generation-aggregate-upgrade.db")
                .display()
        );
        let database = Database::connect(&database_url).await.unwrap();
        sqlx::query(
            "CREATE TABLE schema_migrations (version BIGINT PRIMARY KEY, name TEXT NOT NULL, applied_at BIGINT NOT NULL)",
        )
        .execute(&database.pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE generation_jobs (id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, key_id TEXT NOT NULL, upstream_account_id TEXT NOT NULL, public_model TEXT NOT NULL, status TEXT NOT NULL, error_code TEXT, billed_units BIGINT, cost_micros BIGINT NOT NULL, created_at BIGINT NOT NULL, updated_at BIGINT NOT NULL, completed_at BIGINT)",
        )
        .execute(&database.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO generation_jobs (id, tenant_id, key_id, upstream_account_id, public_model, status, error_code, billed_units, cost_micros, created_at, updated_at, completed_at) VALUES ('old-job', 'tenant-1', 'key-1', 'upstream-1', 'image-old', 'failed', 'upstream_error', 2, 750000, 86400123, 86400456, 86400456)",
        )
        .execute(&database.pool)
        .await
        .unwrap();

        let mut transaction = database.pool.begin().await.unwrap();
        apply_migration_range(&mut transaction, SQLITE_MIGRATIONS, 23, 23)
            .await
            .unwrap();
        transaction.commit().await.unwrap();

        let aggregate = sqlx::query(
            "SELECT day_bucket, status_class, error_code, requests, billed_units, cost_micros FROM generation_daily_aggregates WHERE key_id = 'key-1'",
        )
        .fetch_one(&database.pool)
        .await
        .unwrap();
        assert_eq!(aggregate.get::<i64, _>("day_bucket"), 1);
        assert_eq!(aggregate.get::<String, _>("status_class"), "failure");
        assert_eq!(aggregate.get::<String, _>("error_code"), "upstream_error");
        assert_eq!(aggregate.get::<i64, _>("requests"), 1);
        assert_eq!(aggregate.get::<i64, _>("billed_units"), 2);
        assert_eq!(aggregate.get::<i64, _>("cost_micros"), 750_000);
        let fact = sqlx::query(
            "SELECT created_at, duration_ms, cost_micros FROM generation_stats_facts WHERE job_id = 'old-job'",
        )
        .fetch_one(&database.pool)
        .await
        .unwrap();
        assert_eq!(fact.get::<i64, _>("created_at"), 86_400_123);
        assert_eq!(fact.get::<i64, _>("duration_ms"), 333);
        assert_eq!(fact.get::<i64, _>("cost_micros"), 750_000);
        let marker: Option<i64> = sqlx::query_scalar(
            "SELECT stats_aggregated_at FROM generation_jobs WHERE id = 'old-job'",
        )
        .fetch_one(&database.pool)
        .await
        .unwrap();
        assert_eq!(marker, Some(86_400_456));

        let mut transaction = database.pool.begin().await.unwrap();
        apply_migration_range(&mut transaction, SQLITE_MIGRATIONS, 23, 23)
            .await
            .unwrap();
        transaction.commit().await.unwrap();
        let requests: i64 = sqlx::query_scalar(
            "SELECT requests FROM generation_daily_aggregates WHERE key_id = 'key-1'",
        )
        .fetch_one(&database.pool)
        .await
        .unwrap();
        assert_eq!(requests, 1);
    }

    async fn create_locator_migration_fixture(database: &Database) {
        sqlx::query(
            "CREATE TABLE schema_migrations (version BIGINT PRIMARY KEY, name TEXT NOT NULL, applied_at BIGINT NOT NULL)",
        )
        .execute(&database.pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE request_records (id TEXT NOT NULL, created_at BIGINT NOT NULL, tenant_id TEXT NOT NULL, key_id TEXT NOT NULL)",
        )
        .execute(&database.pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE request_events (event_id TEXT NOT NULL, event_at BIGINT NOT NULL, tenant_id TEXT NOT NULL, key_id TEXT NOT NULL, request_id TEXT NOT NULL)",
        )
        .execute(&database.pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn sqlite_locator_migration_backfills_and_claims_idempotently() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("locator-upgrade.db").display()
        );
        let database = Database::connect(&database_url).await.unwrap();
        create_locator_migration_fixture(&database).await;
        sqlx::query(
            "INSERT INTO request_records (id, created_at, tenant_id, key_id) VALUES ('request-1', 100, 'tenant-1', 'key-1')",
        )
        .execute(&database.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO request_events (event_id, event_at, tenant_id, key_id, request_id) VALUES ('event-1', 101, 'tenant-1', 'key-1', 'request-1')",
        )
        .execute(&database.pool)
        .await
        .unwrap();

        let mut transaction = database.pool.begin().await.unwrap();
        apply_migration_range(&mut transaction, SQLITE_MIGRATIONS, 21, 21)
            .await
            .unwrap();
        transaction.commit().await.unwrap();

        let request = sqlx::query(
            "SELECT created_at, tenant_id, key_id FROM request_record_locators WHERE id = 'request-1'",
        )
        .fetch_one(&database.pool)
        .await
        .unwrap();
        assert_eq!(request.get::<i64, _>("created_at"), 100);
        assert_eq!(request.get::<String, _>("tenant_id"), "tenant-1");
        assert_eq!(request.get::<String, _>("key_id"), "key-1");
        let event = sqlx::query(
            "SELECT created_at, request_id FROM request_event_locators WHERE id = 'event-1'",
        )
        .fetch_one(&database.pool)
        .await
        .unwrap();
        assert_eq!(event.get::<i64, _>("created_at"), 101);
        assert_eq!(event.get::<String, _>("request_id"), "request-1");

        let mut transaction = database.pool.begin().await.unwrap();
        assert!(
            !claim_request_record_locator(&mut transaction, "request-1", 100, "tenant-1", "key-1")
                .await
                .unwrap()
        );
        assert!(
            claim_request_record_locator(&mut transaction, "request-1", 999, "tenant-1", "key-1")
                .await
                .is_err()
        );
        assert!(
            !claim_request_event_locator(
                &mut transaction,
                "event-1",
                101,
                "tenant-1",
                "key-1",
                "request-1"
            )
            .await
            .unwrap()
        );
        assert!(
            claim_request_event_locator(
                &mut transaction,
                "event-1",
                101,
                "tenant-1",
                "key-1",
                "request-2"
            )
            .await
            .is_err()
        );
        transaction.rollback().await.unwrap();
    }

    #[tokio::test]
    async fn sqlite_locator_migration_fails_closed_on_historical_duplicate_ids() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("locator-duplicate.db").display()
        );
        let database = Database::connect(&database_url).await.unwrap();
        create_locator_migration_fixture(&database).await;
        for created_at in [100_i64, 200] {
            sqlx::query(
                "INSERT INTO request_records (id, created_at, tenant_id, key_id) VALUES ('duplicate-request', $1, 'tenant-1', 'key-1')",
            )
            .bind(created_at)
            .execute(&database.pool)
            .await
            .unwrap();
        }

        let mut transaction = database.pool.begin().await.unwrap();
        let error = apply_migration_range(&mut transaction, SQLITE_MIGRATIONS, 21, 21)
            .await
            .expect_err("duplicate historical request ids must abort v21");
        assert!(error.to_string().contains("request_record_locators"));
        transaction.rollback().await.unwrap();
        let applied: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM schema_migrations WHERE version = 21")
                .fetch_one(&database.pool)
                .await
                .unwrap();
        assert_eq!(applied, 0);
    }

    #[tokio::test]
    async fn sqlite_request_lifecycle_uses_locators_for_finish_detail_and_events() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("locator-lifecycle.db").display()
        );
        let database = Database::connect(&database_url).await.unwrap();
        database.migrate().await.unwrap();
        let request_id = Uuid::now_v7();
        let tenant_id = Uuid::now_v7();
        let key_id = Uuid::now_v7();
        database
            .record_request_started(NewRequest {
                request_id,
                tenant_id,
                key_id,
                protocol: "openai-responses".into(),
                model: "locator-model".into(),
                request_object: "memory://locator-request".into(),
                reservation_id: Uuid::now_v7(),
                upstream_account_id: None,
                model_route_id: None,
            })
            .await
            .unwrap();
        let locator_created_at: i64 = sqlx::query_scalar(
            "SELECT created_at FROM request_record_locators WHERE id = $1 AND tenant_id = $2 AND key_id = $3",
        )
        .bind(request_id.to_string())
        .bind(tenant_id.to_string())
        .bind(key_id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap();

        database
            .record_request_finished(FinishRequest {
                request_id,
                status_code: 200,
                duration_ms: 12,
                input_tokens: 3,
                output_tokens: 5,
                cost_micros: 7,
                error_code: None,
                response_object: "memory://locator-response".into(),
            })
            .await
            .unwrap();
        let detail = database
            .request_archive_refs(key_id, request_id)
            .await
            .unwrap();
        assert_eq!(detail.view.created_at, locator_created_at);
        assert_eq!(detail.view.status_code, Some(200));
        assert_eq!(
            detail.response_object.as_deref(),
            Some("memory://locator-response")
        );
        let located_events: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM request_event_locators l JOIN request_events e ON e.event_id = l.id AND e.event_at = l.created_at WHERE l.request_id = $1",
        )
        .bind(request_id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap();
        assert_eq!(located_events, 2);

        // Removing the leaf while leaving its stable owner is treated as
        // corruption, never as permission to fall through to a broad id scan.
        sqlx::query("DELETE FROM request_records WHERE id = $1 AND created_at = $2")
            .bind(request_id.to_string())
            .bind(locator_created_at)
            .execute(&database.pool)
            .await
            .unwrap();
        assert!(matches!(
            database.request_archive_refs(key_id, request_id).await,
            Err(AppError::Internal)
        ));
    }

    #[tokio::test]
    async fn postgres_locator_migration_rejects_duplicates_across_partitions() {
        let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
            return;
        };
        sqlx::any::install_default_drivers();
        let pool = AnyPoolOptions::new()
            .max_connections(1)
            .connect(&database_url)
            .await
            .unwrap();
        let mut transaction = pool.begin().await.unwrap();
        let schema = format!("locator_duplicate_{}", Uuid::now_v7().simple());
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&mut *transaction)
            .await
            .unwrap();
        sqlx::query(&format!("SET LOCAL search_path = {schema}"))
            .execute(&mut *transaction)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE schema_migrations (version BIGINT PRIMARY KEY, name TEXT NOT NULL, applied_at BIGINT NOT NULL)",
        )
        .execute(&mut *transaction)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE request_records (id TEXT NOT NULL, created_at BIGINT NOT NULL, tenant_id TEXT NOT NULL, key_id TEXT NOT NULL) PARTITION BY RANGE (created_at)",
        )
        .execute(&mut *transaction)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE request_records_early PARTITION OF request_records FOR VALUES FROM (0) TO (100)",
        )
        .execute(&mut *transaction)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE request_records_late PARTITION OF request_records FOR VALUES FROM (100) TO (200)",
        )
        .execute(&mut *transaction)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE request_events (event_id TEXT NOT NULL, event_at BIGINT NOT NULL, tenant_id TEXT NOT NULL, key_id TEXT NOT NULL, request_id TEXT NOT NULL)",
        )
        .execute(&mut *transaction)
        .await
        .unwrap();
        for created_at in [50_i64, 150] {
            sqlx::query(
                "INSERT INTO request_records (id, created_at, tenant_id, key_id) VALUES ('duplicate-request', $1, 'tenant-1', 'key-1')",
            )
            .bind(created_at)
            .execute(&mut *transaction)
            .await
            .unwrap();
        }

        let error = apply_migration_range(&mut transaction, POSTGRES_MIGRATIONS, 21, 21)
            .await
            .expect_err("cross-partition duplicate ids must abort v21");
        assert!(error.to_string().contains("request_record_locators"));
        transaction.rollback().await.unwrap();
    }

    #[tokio::test]
    async fn postgres_locator_timestamp_prunes_request_detail_to_one_leaf() {
        let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
            return;
        };
        let database = Database::connect(&database_url).await.unwrap();
        database.migrate().await.unwrap();
        let request_id = Uuid::now_v7();
        let tenant_id = Uuid::now_v7();
        let key_id = Uuid::now_v7();
        database
            .record_request_started(NewRequest {
                request_id,
                tenant_id,
                key_id,
                protocol: "openai-responses".into(),
                model: "locator-pruning-model".into(),
                request_object: "memory://locator-pruning-request".into(),
                reservation_id: Uuid::now_v7(),
                upstream_account_id: None,
                model_route_id: None,
            })
            .await
            .unwrap();
        let created_at: i64 =
            sqlx::query_scalar("SELECT created_at FROM request_record_locators WHERE id = $1")
                .bind(request_id.to_string())
                .fetch_one(&database.pool)
                .await
                .unwrap();
        let storage_partition: String = sqlx::query_scalar(
            "SELECT tableoid::regclass::TEXT FROM request_records WHERE id = $1 AND created_at = $2",
        )
        .bind(request_id.to_string())
        .bind(created_at)
        .fetch_one(&database.pool)
        .await
        .unwrap();
        let plan = sqlx::query(
            "EXPLAIN (FORMAT TEXT) SELECT id, created_at, request_object FROM request_records WHERE id = $1 AND created_at = $2",
        )
        .bind(request_id.to_string())
        .bind(created_at)
        .fetch_all(&database.pool)
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.get::<String, _>(0))
        .collect::<Vec<_>>()
        .join("\n");
        assert!(plan.contains(&storage_partition), "{plan}");
        assert!(!plan.contains("Append"), "{plan}");
    }

    #[tokio::test]
    async fn postgres_partition_maintenance_skips_default_overlap_and_continues() {
        let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
            return;
        };
        sqlx::any::install_default_drivers();
        let pool = AnyPoolOptions::new()
            .max_connections(1)
            .connect(&database_url)
            .await
            .unwrap();
        let mut transaction = pool.begin().await.unwrap();
        let schema = format!("partition_maintenance_{}", Uuid::now_v7().simple());
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&mut *transaction)
            .await
            .unwrap();
        sqlx::query(&format!("SET LOCAL search_path = {schema}"))
            .execute(&mut *transaction)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE request_records (id TEXT NOT NULL, created_at BIGINT NOT NULL, payload TEXT NOT NULL) PARTITION BY RANGE (created_at)",
        )
        .execute(&mut *transaction)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE request_events (event_id TEXT NOT NULL, event_at BIGINT NOT NULL, payload TEXT NOT NULL) PARTITION BY RANGE (event_at)",
        )
        .execute(&mut *transaction)
        .await
        .unwrap();
        for table in ["request_records", "request_events"] {
            sqlx::query(&format!(
                "CREATE TABLE {table}_default PARTITION OF {table} DEFAULT"
            ))
            .execute(&mut *transaction)
            .await
            .unwrap();
        }

        let today = Utc::now().date_naive();
        let today_start = today
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp_millis();
        sqlx::query(
            "INSERT INTO request_records_default (id, created_at, payload) VALUES ($1, $2, $3)",
        )
        .bind("blocked-row")
        .bind(today_start + 123)
        .bind("must remain unchanged")
        .execute(&mut *transaction)
        .await
        .unwrap();

        let report = maintain_postgres_partitions(&mut transaction)
            .await
            .unwrap();

        assert_eq!(report.ready_partitions, 17);
        assert_eq!(
            report.blocked_partitions,
            vec![BlockedPartition {
                table: "request_records".to_owned(),
                partition: format!("request_records_{}", today.format("%Y%m%d")),
                day: today,
            }]
        );
        let stored = sqlx::query(
            "SELECT id, created_at, payload, tableoid::regclass::TEXT AS storage_partition FROM request_records WHERE id = $1",
        )
        .bind("blocked-row")
        .fetch_one(&mut *transaction)
        .await
        .unwrap();
        assert_eq!(stored.get::<String, _>("id"), "blocked-row");
        assert_eq!(stored.get::<i64, _>("created_at"), today_start + 123);
        assert_eq!(stored.get::<String, _>("payload"), "must remain unchanged");
        assert!(
            stored
                .get::<String, _>("storage_partition")
                .ends_with("request_records_default")
        );

        let tomorrow = today.checked_add_days(Days::new(1)).unwrap();
        for table in ["request_records", "request_events"] {
            let expected = format!("{table}_{}", tomorrow.format("%Y%m%d"));
            let created: Option<String> = sqlx::query_scalar("SELECT to_regclass($1)::TEXT")
                .bind(format!("{schema}.{expected}"))
                .fetch_one(&mut *transaction)
                .await
                .unwrap();
            assert_eq!(created.as_deref(), Some(expected.as_str()));
        }
        // The caller's outer transaction remains usable after the rejected partition DDL.
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT 1::BIGINT")
                .fetch_one(&mut *transaction)
                .await
                .unwrap(),
            1
        );
        transaction.rollback().await.unwrap();
    }

    #[tokio::test]
    async fn sqlite_upgrade_adds_request_routing_columns() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("upgrade.db").display()
        );
        let database = Database::connect(&database_url).await.unwrap();
        sqlx::query(
            "CREATE TABLE request_records (id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, key_id TEXT NOT NULL, created_at BIGINT NOT NULL, completed_at BIGINT, protocol TEXT NOT NULL, model TEXT NOT NULL, status_code BIGINT, duration_ms BIGINT, input_tokens BIGINT NOT NULL, output_tokens BIGINT NOT NULL, cost_micros BIGINT NOT NULL, error_code TEXT, request_object TEXT NOT NULL, response_object TEXT, reservation_id TEXT NOT NULL, conversation_cluster_id TEXT)",
        )
        .execute(&database.pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE upstream_accounts (id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, name TEXT NOT NULL, driver TEXT NOT NULL, auth_kind TEXT NOT NULL, config_json TEXT NOT NULL, status TEXT NOT NULL, credential_generation BIGINT NOT NULL, created_at BIGINT NOT NULL, updated_at BIGINT NOT NULL, UNIQUE(tenant_id, name))",
        )
        .execute(&database.pool)
        .await
        .unwrap();

        database.migrate().await.unwrap();

        for column in ["upstream_account_id", "model_route_id"] {
            let present = sqlx::query(
                "SELECT name FROM pragma_table_info('request_records') WHERE name = $1",
            )
            .bind(column)
            .fetch_optional(&database.pool)
            .await
            .unwrap()
            .is_some();
            assert!(present, "missing upgraded column {column}");
        }
        let oauth_session_present = sqlx::query(
            "SELECT name FROM pragma_table_info('upstream_accounts') WHERE name = 'oauth_session_id'",
        )
        .fetch_optional(&database.pool)
        .await
        .unwrap()
        .is_some();
        assert!(oauth_session_present);
    }

    #[tokio::test]
    async fn service_token_rotation_preserves_identity_and_revokes_old_generation() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("service-token.db").display()
        );
        let database = Database::connect(&database_url).await.unwrap();
        database.migrate().await.unwrap();
        let pepper = b"a service credential pepper longer than thirty-two bytes";
        let first = database
            .create_service_token(
                CreateServiceTokenInput {
                    name: "memeloop-web".to_owned(),
                    scopes: vec!["keys:write".to_owned(), "credits:write".to_owned()],
                    tenant_external_id: Some("memeloop".to_owned()),
                },
                pepper,
            )
            .await
            .unwrap();
        let authenticated = database
            .authenticate_service_token(&first.token, pepper)
            .await
            .unwrap();
        assert_eq!(authenticated.service_id, Some(first.service_id));
        assert!(authenticated.allows("keys:write"));
        assert!(!authenticated.allows("prices:write"));

        let rotated = database
            .rotate_service_token(first.service_id, "rotate-service-token-1", pepper)
            .await
            .unwrap();
        let replay = database
            .rotate_service_token(first.service_id, "rotate-service-token-1", pepper)
            .await
            .unwrap();
        assert_eq!(rotated.service_id, first.service_id);
        assert_eq!(rotated.credential_generation, 2);
        assert_eq!(replay.credential_generation, 2);
        assert_eq!(replay.token, rotated.token);
        assert!(matches!(
            database
                .authenticate_service_token(&first.token, pepper)
                .await,
            Err(AppError::Unauthorized)
        ));
        assert!(
            database
                .authenticate_service_token(&rotated.token, pepper)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn sqlite_key_rotation_is_concurrent_idempotent_and_resource_bound() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("key-rotation.db").display()
        );
        let database = Database::connect_with_max(&database_url, 8).await.unwrap();
        database.migrate().await.unwrap();
        let pepper: &'static [u8] = b"a rotation credential pepper longer than thirty-two bytes";
        let issued = database
            .create_key(
                CreateKeyInput {
                    tenant_external_id: "tenant".to_owned(),
                    principal_external_id: "member".to_owned(),
                    alias: "stable-identity".to_owned(),
                    currency: "USD".to_owned(),
                    policy: KeyPolicy {
                        allowed_models: vec!["codex-test".to_owned()],
                        ..KeyPolicy::default()
                    },
                    initial_balance: Decimal::ONE,
                    idempotency_key: None,
                },
                pepper,
            )
            .await
            .unwrap();

        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(8));
        let key_id = issued.key_id;
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let database = database.clone();
            let barrier = barrier.clone();
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                database
                    .rotate_key(key_id, "same-key-rotation", pepper)
                    .await
            }));
        }
        let mut rotated = Vec::new();
        for task in tasks {
            rotated.push(task.await.unwrap().unwrap());
        }
        assert!(rotated.iter().all(|value| {
            value.key_id == issued.key_id
                && value.account_id == issued.account_id
                && value.credential_generation == 2
                && value.key == rotated[0].key
        }));
        assert!(matches!(
            database.authenticate_key(&issued.key, pepper).await,
            Err(AppError::Unauthorized)
        ));
        let authenticated = database
            .authenticate_key(&rotated[0].key, pepper)
            .await
            .unwrap();
        assert_eq!(authenticated.account_id, issued.account_id);
        assert!(authenticated.policy.allows_model("codex-test"));

        let service = database
            .create_service_token(
                CreateServiceTokenInput {
                    name: "resource-binding-test".to_owned(),
                    scopes: vec!["keys:write".to_owned()],
                    tenant_external_id: None,
                },
                pepper,
            )
            .await
            .unwrap();
        assert!(matches!(
            database
                .rotate_service_token(service.service_id, "same-key-rotation", pepper)
                .await,
            Err(AppError::BadRequest(_))
        ));

        let replay_row = sqlx::query(
            "SELECT request_hash, response_ciphertext FROM credential_rotation_replays WHERE idempotency_key = $1",
        )
        .bind("same-key-rotation")
        .fetch_one(&database.pool)
        .await
        .unwrap();
        let request_hash: String = replay_row.try_get("request_hash").unwrap();
        let ciphertext: String = replay_row.try_get("response_ciphertext").unwrap();
        assert_eq!(request_hash.len(), 64);
        assert!(!ciphertext.contains(&rotated[0].key));

        sqlx::query(
            "UPDATE credential_rotation_replays SET expires_at = $1 WHERE idempotency_key = $2",
        )
        .bind(unix_millis().saturating_sub(1))
        .bind("same-key-rotation")
        .execute(&database.pool)
        .await
        .unwrap();
        database
            .expire_key_provisioning_responses(100)
            .await
            .unwrap();
        let expired: Option<String> = sqlx::query(
            "SELECT response_ciphertext FROM credential_rotation_replays WHERE idempotency_key = $1",
        )
        .bind("same-key-rotation")
        .fetch_one(&database.pool)
        .await
        .unwrap()
        .try_get("response_ciphertext")
        .unwrap();
        assert!(expired.is_none());
        assert!(matches!(
            database
                .rotate_key(issued.key_id, "same-key-rotation", pepper)
                .await,
            Err(AppError::BadRequest(_))
        ));
        let generation: i64 =
            sqlx::query("SELECT credential_generation FROM key_records WHERE id = $1")
                .bind(issued.key_id.to_string())
                .fetch_one(&database.pool)
                .await
                .unwrap()
                .try_get("credential_generation")
                .unwrap();
        assert_eq!(generation, 2);
    }

    #[tokio::test]
    async fn key_provisioning_replays_one_encrypted_response_for_an_idempotency_key() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("key-idempotency.db").display()
        );
        let database = Database::connect(&database_url).await.unwrap();
        database.migrate().await.unwrap();
        let pepper = b"a downstream key pepper longer than thirty-two bytes";
        let request = |alias: &str| CreateKeyInput {
            tenant_external_id: "tenant".to_owned(),
            principal_external_id: "member".to_owned(),
            alias: alias.to_owned(),
            currency: "USD".to_owned(),
            policy: KeyPolicy::default(),
            initial_balance: Decimal::ONE,
            idempotency_key: Some("registration-event-1".to_owned()),
        };

        let first = database
            .create_key(request("primary"), pepper)
            .await
            .unwrap();
        let replay = database
            .create_key(request("primary"), pepper)
            .await
            .unwrap();
        assert_eq!(replay.key_id, first.key_id);
        assert_eq!(replay.account_id, first.account_id);
        assert_eq!(replay.key, first.key);
        assert!(matches!(
            database.create_key(request("different"), pepper).await,
            Err(AppError::BadRequest(_))
        ));

        let count: i64 = sqlx::query("SELECT COUNT(*) AS count FROM key_records")
            .fetch_one(&database.pool)
            .await
            .unwrap()
            .try_get("count")
            .unwrap();
        assert_eq!(count, 1);
        let ciphertext: String =
            sqlx::query("SELECT issued_key_ciphertext FROM key_records WHERE id = $1")
                .bind(first.key_id.to_string())
                .fetch_one(&database.pool)
                .await
                .unwrap()
                .try_get("issued_key_ciphertext")
                .unwrap();
        assert!(!ciphertext.contains(&first.key));

        let other_tenant = database
            .create_key(
                CreateKeyInput {
                    tenant_external_id: "other-tenant".to_owned(),
                    principal_external_id: "member".to_owned(),
                    alias: "primary".to_owned(),
                    currency: "USD".to_owned(),
                    policy: KeyPolicy::default(),
                    initial_balance: Decimal::ONE,
                    idempotency_key: Some("registration-event-1".to_owned()),
                },
                pepper,
            )
            .await
            .unwrap();
        assert_ne!(other_tenant.key_id, first.key_id);
    }

    #[tokio::test]
    async fn grant_reversal_is_idempotent_and_only_revokes_unspent_credit() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("grant-reversal.db").display()
        );
        let database = Database::connect(&database_url).await.unwrap();
        database.migrate().await.unwrap();
        let issued = database
            .create_key(
                CreateKeyInput {
                    tenant_external_id: "tenant".to_owned(),
                    principal_external_id: "member".to_owned(),
                    alias: "refund-test".to_owned(),
                    currency: "USD".to_owned(),
                    policy: KeyPolicy::default(),
                    initial_balance: Decimal::ZERO,
                    idempotency_key: None,
                },
                b"a downstream key pepper longer than thirty-two bytes",
            )
            .await
            .unwrap();
        let other_account = database
            .create_key(
                CreateKeyInput {
                    tenant_external_id: "other-tenant".to_owned(),
                    principal_external_id: "member".to_owned(),
                    alias: "refund-test".to_owned(),
                    currency: "USD".to_owned(),
                    policy: KeyPolicy::default(),
                    initial_balance: Decimal::ZERO,
                    idempotency_key: None,
                },
                b"a downstream key pepper longer than thirty-two bytes",
            )
            .await
            .unwrap();

        assert_eq!(
            database
                .grant(
                    issued.account_id,
                    Decimal::new(10, 0),
                    "subscription:pro",
                    "subscription:one:grant",
                )
                .await
                .unwrap(),
            "10"
        );
        assert_eq!(
            database
                .grant(
                    other_account.account_id,
                    Decimal::new(10, 0),
                    "subscription:pro",
                    "subscription:one:grant",
                )
                .await
                .unwrap(),
            "10"
        );
        assert_eq!(
            database
                .reverse_grant(
                    issued.account_id,
                    "subscription:one:grant",
                    "subscription_cancelled",
                    "subscription:one:reversal",
                )
                .await
                .unwrap(),
            "10"
        );
        assert_eq!(
            database
                .reverse_grant(
                    issued.account_id,
                    "subscription:one:grant",
                    "subscription_cancelled",
                    "subscription:one:reversal",
                )
                .await
                .unwrap(),
            "10"
        );
        assert!(matches!(
            database
                .reverse_grant(
                    issued.account_id,
                    "subscription:one:grant",
                    "duplicate",
                    "subscription:one:other-reversal",
                )
                .await,
            Err(AppError::BadRequest(_))
        ));

        database
            .grant(
                issued.account_id,
                Decimal::new(5, 0),
                "subscription:basic",
                "subscription:two:grant",
            )
            .await
            .unwrap();
        sqlx::query("UPDATE credit_accounts SET available_micros = 4000000 WHERE id = $1")
            .bind(issued.account_id.to_string())
            .execute(&database.pool)
            .await
            .unwrap();
        assert!(matches!(
            database
                .reverse_grant(
                    issued.account_id,
                    "subscription:two:grant",
                    "subscription_cancelled",
                    "subscription:two:reversal",
                )
                .await,
            Err(AppError::QuotaExceeded)
        ));
        let reversals: i64 = sqlx::query(
            "SELECT COUNT(*) AS count FROM ledger_entries WHERE kind = 'grant_reversal'",
        )
        .fetch_one(&database.pool)
        .await
        .unwrap()
        .try_get("count")
        .unwrap();
        assert_eq!(reversals, 1);
    }

    #[tokio::test]
    async fn plugin_kv_is_namespaced_and_bounded() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("plugin-kv.db").display()
        );
        let database = Database::connect(&database_url).await.unwrap();
        database.migrate().await.unwrap();

        database
            .plugin_kv_put("routing-plugin", "oauth/state", b"encrypted-state")
            .await
            .unwrap();
        assert_eq!(
            database
                .plugin_kv_get("routing-plugin", "oauth/state")
                .await
                .unwrap(),
            Some(b"encrypted-state".to_vec())
        );
        assert_eq!(
            database
                .plugin_kv_get("other-plugin", "oauth/state")
                .await
                .unwrap(),
            None
        );
        database
            .plugin_kv_put("routing-plugin", "oauth/state", b"next-state")
            .await
            .unwrap();
        assert_eq!(
            database
                .plugin_kv_get("routing-plugin", "oauth/state")
                .await
                .unwrap(),
            Some(b"next-state".to_vec())
        );
        assert!(
            database
                .plugin_kv_put("routing-plugin", "unsafe key", b"value")
                .await
                .is_err()
        );
        assert!(
            database
                .plugin_kv_put("routing-plugin", "too-large", &vec![0_u8; 1024 * 1024 + 1])
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn maintenance_releases_old_unlinked_reservations() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("orphan-reservation.db").display()
        );
        let database = Database::connect(&database_url).await.unwrap();
        database.migrate().await.unwrap();
        let pepper = b"a downstream key pepper longer than thirty-two bytes";
        let issued = database
            .create_key(
                CreateKeyInput {
                    tenant_external_id: "tenant".to_owned(),
                    principal_external_id: "member".to_owned(),
                    alias: "orphan-test".to_owned(),
                    currency: "USD".to_owned(),
                    policy: KeyPolicy::default(),
                    initial_balance: Decimal::ONE,
                    idempotency_key: None,
                },
                pepper,
            )
            .await
            .unwrap();
        let key = database
            .authenticate_key(&issued.key, pepper)
            .await
            .unwrap();
        let price = database
            .upsert_model_price("orphan-model", "USD", Decimal::ZERO, Decimal::ONE)
            .await
            .unwrap();
        let reservation = database
            .reserve_usage(&key, &price, 0, 1_000)
            .await
            .unwrap();
        assert_eq!(reservation.reserved_micros, 1_000);
        let reserved_account = sqlx::query(
            "SELECT available_micros, reserved_micros FROM credit_accounts WHERE id = $1",
        )
        .bind(issued.account_id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap();
        assert_eq!(reserved_account.get::<i64, _>("available_micros"), 999_000);
        assert_eq!(reserved_account.get::<i64, _>("reserved_micros"), 1_000);
        sqlx::query("UPDATE usage_reservations SET created_at = $1 WHERE id = $2")
            .bind(unix_millis().saturating_sub(31 * 60 * 1_000))
            .bind(reservation.id.to_string())
            .execute(&database.pool)
            .await
            .unwrap();

        assert_eq!(
            database.release_orphaned_reservations(100).await.unwrap(),
            1
        );
        let reservation_row =
            sqlx::query("SELECT status, actual_micros FROM usage_reservations WHERE id = $1")
                .bind(reservation.id.to_string())
                .fetch_one(&database.pool)
                .await
                .unwrap();
        assert_eq!(reservation_row.get::<String, _>("status"), "settled");
        assert_eq!(reservation_row.get::<i64, _>("actual_micros"), 0);
        let account_row = sqlx::query(
            "SELECT available_micros, reserved_micros FROM credit_accounts WHERE id = $1",
        )
        .bind(issued.account_id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap();
        assert_eq!(account_row.get::<i64, _>("available_micros"), 1_000_000);
        assert_eq!(account_row.get::<i64, _>("reserved_micros"), 0);

        let linked_reservation = database
            .reserve_usage(&key, &price, 0, 1_000)
            .await
            .unwrap();
        let linked_request_id = Uuid::now_v7();
        database
            .record_request_started(NewRequest {
                request_id: linked_request_id,
                key_id: key.key_id,
                tenant_id: key.tenant_id,
                protocol: "openai".to_owned(),
                model: "orphan-model".to_owned(),
                request_object: format!("gap://{linked_request_id}/request"),
                reservation_id: linked_reservation.id,
                upstream_account_id: None,
                model_route_id: None,
            })
            .await
            .unwrap();
        sqlx::query("UPDATE usage_reservations SET created_at = $1 WHERE id = $2")
            .bind(unix_millis().saturating_sub(31 * 60 * 1_000))
            .bind(linked_reservation.id.to_string())
            .execute(&database.pool)
            .await
            .unwrap();
        assert_eq!(
            database.release_orphaned_reservations(100).await.unwrap(),
            1
        );
        let expired_request = sqlx::query(
            "SELECT status_code, error_code, completed_at FROM request_records WHERE id = $1",
        )
        .bind(linked_request_id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap();
        assert_eq!(expired_request.get::<i64, _>("status_code"), 504);
        assert_eq!(
            expired_request.get::<String, _>("error_code"),
            "request_expired"
        );
        assert!(
            expired_request
                .get::<Option<i64>, _>("completed_at")
                .is_some()
        );

        let overage_reservation = database
            .reserve_usage(&key, &price, 0, 1_000)
            .await
            .unwrap();
        assert_eq!(
            database
                .settle_usage(&overage_reservation, 0, 2_000)
                .await
                .unwrap(),
            2_000
        );
        let overage_account = sqlx::query(
            "SELECT available_micros, reserved_micros FROM credit_accounts WHERE id = $1",
        )
        .bind(issued.account_id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap();
        assert_eq!(overage_account.get::<i64, _>("available_micros"), 998_000);
        assert_eq!(overage_account.get::<i64, _>("reserved_micros"), 0);

        let capped_reservation = database
            .reserve_usage(&key, &price, 0, 1_000)
            .await
            .unwrap();
        assert!(matches!(
            database
                .settle_usage(&capped_reservation, 0, 2_000_000_000)
                .await,
            Err(AppError::BadRequest(_))
        ));
        assert_eq!(
            database
                .settle_usage(&capped_reservation, 0, 1_000_000_000)
                .await
                .unwrap(),
            998_000
        );
        let capped_account = sqlx::query(
            "SELECT available_micros, reserved_micros FROM credit_accounts WHERE id = $1",
        )
        .bind(issued.account_id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap();
        assert_eq!(capped_account.get::<i64, _>("available_micros"), 0);
        assert_eq!(capped_account.get::<i64, _>("reserved_micros"), 0);
    }

    #[tokio::test]
    async fn settlement_cannot_cross_a_hard_lifetime_budget() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("settlement-budget.db").display()
        );
        let database = Database::connect(&database_url).await.unwrap();
        database.migrate().await.unwrap();
        let pepper = b"a downstream key pepper longer than thirty-two bytes";
        let issued = database
            .create_key(
                CreateKeyInput {
                    tenant_external_id: "budget-tenant".to_owned(),
                    principal_external_id: "budget-member".to_owned(),
                    alias: "hard-budget".to_owned(),
                    currency: "USD".to_owned(),
                    policy: KeyPolicy {
                        allowed_models: vec!["budget-model".to_owned()],
                        lifetime_budget: Some("0.0015".to_owned()),
                        ..KeyPolicy::default()
                    },
                    initial_balance: Decimal::ONE,
                    idempotency_key: None,
                },
                pepper,
            )
            .await
            .unwrap();
        let key = database
            .authenticate_key(&issued.key, pepper)
            .await
            .unwrap();
        let price = database
            .upsert_model_price("budget-model", "USD", Decimal::ZERO, Decimal::ONE)
            .await
            .unwrap();
        let reservation = database
            .reserve_usage(&key, &price, 0, 1_000)
            .await
            .unwrap();

        assert_eq!(
            database.settle_usage(&reservation, 0, 2_000).await.unwrap(),
            1_500
        );
        assert!(matches!(
            database.reserve_usage(&key, &price, 0, 1).await,
            Err(AppError::QuotaExceeded)
        ));
        let account = sqlx::query(
            "SELECT available_micros, reserved_micros FROM credit_accounts WHERE id = $1",
        )
        .bind(issued.account_id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap();
        assert_eq!(account.get::<i64, _>("available_micros"), 998_500);
        assert_eq!(account.get::<i64, _>("reserved_micros"), 0);
    }

    #[tokio::test]
    async fn settlement_uses_cache_and_service_tier_price_snapshots() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("tier-pricing.db").display()
        );
        let database = Database::connect(&database_url).await.unwrap();
        database.migrate().await.unwrap();
        let pepper = b"a downstream key pepper longer than thirty-two bytes";
        let issued = database
            .create_key(
                CreateKeyInput {
                    tenant_external_id: "tier-tenant".to_owned(),
                    principal_external_id: "tier-member".to_owned(),
                    alias: "tier-pricing".to_owned(),
                    currency: "USD".to_owned(),
                    policy: KeyPolicy {
                        tokens_per_minute: 2_000_000,
                        ..KeyPolicy::default()
                    },
                    initial_balance: Decimal::from(10),
                    idempotency_key: None,
                },
                pepper,
            )
            .await
            .unwrap();
        let key = database
            .authenticate_key(&issued.key, pepper)
            .await
            .unwrap();
        database
            .upsert_model_price_tier(
                "tier-model",
                "USD",
                "default",
                Decimal::ONE,
                Decimal::new(1, 1),
                Decimal::TWO,
                Decimal::from(3),
                false,
            )
            .await
            .unwrap();
        let price = database
            .upsert_model_price_tier(
                "tier-model",
                "USD",
                "priority",
                Decimal::from(5),
                Decimal::from(5),
                Decimal::from(5),
                Decimal::from(6),
                false,
            )
            .await
            .unwrap();
        let reservation = database
            .reserve_usage(&key, &price, 300_000, 100_000)
            .await
            .unwrap();
        assert_eq!(reservation.reserved_micros, 2_100_000);
        assert_eq!(reservation.price_tiers.len(), 2);
        let snapshot: String =
            sqlx::query("SELECT price_snapshot_json FROM usage_reservations WHERE id = $1")
                .bind(reservation.id.to_string())
                .fetch_one(&database.pool)
                .await
                .unwrap()
                .try_get("price_snapshot_json")
                .unwrap();
        assert!(snapshot.contains("cached_input_micros_per_million"));

        let cost = database
            .settle_token_usage(
                &reservation,
                &TokenUsage {
                    input_tokens: 100_000,
                    cached_input_tokens: 100_000,
                    cache_write_tokens: 100_000,
                    output_tokens: 100_000,
                    service_tier: Some("default".to_owned()),
                },
            )
            .await
            .unwrap();
        assert_eq!(cost, 610_000);

        let conservative = database
            .reserve_usage(&key, &price, 300_000, 100_000)
            .await
            .unwrap();
        let cost = database
            .settle_token_usage(
                &conservative,
                &TokenUsage {
                    input_tokens: 100_000,
                    cached_input_tokens: 100_000,
                    cache_write_tokens: 100_000,
                    output_tokens: 100_000,
                    service_tier: Some("flex".to_owned()),
                },
            )
            .await
            .unwrap();
        assert_eq!(
            cost, 2_100_000,
            "unknown response tiers use snapshot maxima"
        );
    }
}
