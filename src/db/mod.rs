use std::time::Duration;

use chrono::{Days, Utc};
use rust_decimal::Decimal;
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
        EnforcementMode, EntitlementReconcileResult, EntitlementView, GenerationAssetDownload,
        GenerationAssetView, GenerationJobView, GenerationJobWork, GenerationPrice,
        GenerationStagedAssets, IssuedKey, IssuedServiceToken, JSON_SAFE_INTEGER_MAX, KeyAliasView,
        KeyBudgetSnapshot, KeyConcurrencySnapshot, KeyLimitSnapshot, KeyPolicy,
        KeyRateLimitSnapshot, KeyView, LedgerEntryView, ManagedKeyView, ModelPrice, ModelPriceTier,
        ModelPriceTierView, ModelPriceView, OperatorGenerationJobView, OperatorStats,
        RequestArchiveRefs, RequestEventView, RequestProvenanceView, RequestSessionAssociation,
        RequestSessionContext, RequestView, SelfStats, ServiceTokenView, StatsBucket, StatsSummary,
        TenantView, TokenUsage, UsageReservation, micros_to_decimal_string, priced_tokens,
    },
    provider::{
        ModelRouteView, ResolvedUpstream, UpstreamAccountView, UpstreamCredential, open_credential,
        open_private_json, seal_credential, seal_private_json, validate_config,
    },
};

mod archive_staging;
mod billing;
mod constants;
mod credentials;
mod generation;
mod groups;
mod migrations;
mod oauth_sessions;
mod plugin_configurations;
mod plugin_kv;
mod providers;
mod requests;
mod rotation;
mod routing;
mod rows;
mod session_analytics;
mod session_projection;
mod time;
mod usage_analysis;
mod validation;

use constants::*;
use rotation::*;
use rows::generation_asset_download;
pub use session_analytics::LogicalSessionListFilter;
pub(crate) use session_projection::{
    add_archive_record_to_session_projection_in_transaction,
    add_request_fact_to_session_projection_in_transaction,
    reclassify_request_session_in_transaction,
};
pub use time::unix_millis;
use validation::*;

const POSTGRES_SERVE_STATEMENT_TIMEOUT: &str = "SET statement_timeout = 30000";
const POSTGRES_SERVE_LOCK_TIMEOUT: &str = "SET lock_timeout = 10000";
const POSTGRES_SERVE_IDLE_TRANSACTION_TIMEOUT: &str =
    "SET idle_in_transaction_session_timeout = 30000";
const POSTGRES_MIGRATION_STATEMENT_TIMEOUT: &str = "SET statement_timeout = 900000";
const POSTGRES_MIGRATION_LOCK_TIMEOUT: &str = "SET lock_timeout = 60000";
const POSTGRES_MIGRATION_IDLE_TRANSACTION_TIMEOUT: &str =
    "SET idle_in_transaction_session_timeout = 300000";

#[derive(Clone, Copy)]
enum ConnectionProfile {
    Serve,
    Migration,
}

pub(crate) use billing::validate_entitlement_operation;
pub use billing::{
    ApplyCloudEntitlementInput, ApplyCloudEntitlementResult, CancelEntitlementInput,
    CloudRoutingGrantSnapshot, CloudSubscriptionEventInput, CloudSubscriptionEventView,
    EntitlementOperation, ReconcileEntitlementInput, ReplaceEntitlementInput,
};
pub(crate) use credentials::{
    CloudCredentialProvisioningInput, replace_key_routing_grants_in_transaction,
    validate_key_policy,
};
pub use credentials::{CreateKeyInput, CreateServiceTokenInput, ProvisionedCloudCredential};
pub use generation::{
    AttachGenerationJobResult, AttachSynchronousImageRequestObject, CreateGenerationJobInput,
    CreateGenerationJobResult, FinishGenerationJobInput, FinishSynchronousImageRequest,
    FinishSynchronousImageResult, GenerationJobIdempotency, StartGenerationJobInput,
    StartSynchronousImageRequest, StartSynchronousImageResult, SynchronousImageIdempotencyClaim,
};
pub use groups::{
    CreateGroupInput, GroupKind, GroupView, ReplaceGroupMembersInput, UpdateGroupInput,
};
pub use migrations::{BlockedPartition, PartitionMaintenanceReport};
pub(crate) use migrations::{POSTGRES_MIGRATIONS, SQLITE_MIGRATIONS};
#[cfg(test)]
use migrations::{apply_migration_range, maintain_postgres_partitions};
pub use oauth_sessions::{BeginOAuthLoginSession, OAuthLoginClaim, OAuthLoginSessionReference};
pub use providers::{
    AggregatedUpstreamModelCatalogView, AggregatedUpstreamModelView, CreateModelRouteInput,
    CreateUpstreamAccountInput, DiscoveredUpstreamModel, ImportManagedOAuthAccountInput,
    ManagedOAuthImportResult, ManagedOAuthImportStatus, ReauthorizeUpstreamAccountInput,
    ReplaceModelCatalogResult, UpdateModelRouteInput, UpdateUpstreamAccountInput,
    UpstreamModelCatalogView, UpstreamModelView,
};
#[cfg(test)]
pub(crate) use requests::claim_request_record_locator;
pub use requests::{
    AttachProxyArchiveResult, ConversationDetailFilter, ConversationListFilter, FinishProxyRequest,
    FinishProxyRequestResult, FinishRequest, NewRequest, ProxyConversationInput, RequestListFilter,
    SessionArchiveCommitInput, SessionArchiveCorrelation, SessionArchiveImportLock,
    SessionArchiveImportMatch, SessionArchiveImportMatchInput, SessionArchiveLegacyCheckpointInput,
    SessionArchiveMatchInput, SessionArchivePresentSummaryInput,
    SessionArchiveQuarantineBatchInput, SessionArchiveQuarantineCommitInput,
    SessionArchiveQuarantineFilter, SessionArchiveQuarantineRecordView,
    SessionArchiveQuarantineResolutionInput, SessionArchiveQuarantineResolutionView,
    SessionArchiveQuarantineTarget, SessionArchiveSnapshotApplyInput,
    SessionArchiveSnapshotApplyResult, SessionArchiveSnapshotChainInput, SessionArchiveTarget,
    SessionArchiveTombstoneInput, SessionArchiveUnlinkedCommitInput,
    SessionArchiveUnlinkedMetadata, SessionArchiveUnlinkedTarget, StartProxyRequest, StatsFilter,
    normalize_proxy_usage,
};
pub(crate) use requests::{
    ConversationObservationInput, MAX_STATS_RANGE_MILLIS,
    attach_conversation_upstream_response_in_transaction, claim_request_event_locator,
    price_token_usage, proxy_contract_ceiling_micros, record_request_finished_in_transaction,
    record_request_started_in_transaction, reserve_usage_in_transaction, search_prefix,
    settle_token_usage_in_transaction, settle_token_usage_in_transaction_with_charge,
    valid_archive_identifier, validate_numeric_range,
};
#[cfg(test)]
pub(crate) use requests::{
    FILTERED_ACTIVITY_SOURCE_FACTS, FILTERED_ACTIVITY_SOURCE_PENDING,
    FILTERED_ACTIVITY_SOURCE_ROLLUPS,
};
pub use routing::{
    CreateRoutedModelRouteInput, CredentialRoutingView, ReplaceCredentialRoutingInput,
    ReplaceRouteRoutingInput, RouteRoutingView, RouteSelectionOptions, UpdateRoutedModelRouteInput,
};
pub use usage_analysis::{UsageAnalysisFilter, UsageAnalysisUpstreamFilter};

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
        self.list_tenants_page(None, 100).await
    }

    pub async fn list_tenants_page(
        &self,
        after_external_id: Option<&str>,
        limit: i64,
    ) -> Result<Vec<TenantView>, AppError> {
        let rows = sqlx::query(
            "SELECT external_id FROM tenants WHERE external_id > $1 ORDER BY external_id ASC LIMIT $2",
        )
        .bind(after_external_id.unwrap_or_default())
        .bind(limit.clamp(1, 100))
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
        Self::connect_with_profile(database_url, maximum_connections, ConnectionProfile::Serve)
            .await
    }

    pub async fn connect_for_migration(
        database_url: &str,
        maximum_connections: u32,
    ) -> Result<Self, sqlx::Error> {
        Self::connect_with_profile(
            database_url,
            maximum_connections,
            ConnectionProfile::Migration,
        )
        .await
    }

    pub(crate) async fn close(&self) {
        self.pool.close().await;
    }

    async fn connect_with_profile(
        database_url: &str,
        maximum_connections: u32,
        profile: ConnectionProfile,
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
        let mut pool_options = AnyPoolOptions::new()
            .min_connections(0)
            .max_connections(maximum_connections.clamp(1, 32))
            .acquire_timeout(Duration::from_secs(10))
            .idle_timeout(Some(Duration::from_secs(5 * 60)))
            .max_lifetime(Some(Duration::from_secs(30 * 60)));
        if matches!(backend, DatabaseBackend::Sqlite) {
            // SQLite is a supported lightweight/test backend and can have a
            // background worker and an HTTP handler contend for its single
            // writer slot. Without a busy handler, SQLite returns SQLITE_BUSY
            // immediately and turns a safe, short write race into a spurious
            // HTTP 500. Install the bound on every pooled connection; this
            // changes no PostgreSQL behavior and still fails within the pool's
            // ten-second acquisition deadline.
            pool_options = pool_options.after_connect(|connection, _metadata| {
                Box::pin(async move {
                    sqlx::query("PRAGMA busy_timeout = 10000")
                        .execute(connection)
                        .await?;
                    Ok(())
                })
            });
        } else {
            pool_options = pool_options.after_connect(move |connection, _metadata| {
                Box::pin(async move {
                    let (statement_timeout, lock_timeout, idle_transaction_timeout) = match profile
                    {
                        ConnectionProfile::Serve => (
                            POSTGRES_SERVE_STATEMENT_TIMEOUT,
                            POSTGRES_SERVE_LOCK_TIMEOUT,
                            POSTGRES_SERVE_IDLE_TRANSACTION_TIMEOUT,
                        ),
                        ConnectionProfile::Migration => (
                            POSTGRES_MIGRATION_STATEMENT_TIMEOUT,
                            POSTGRES_MIGRATION_LOCK_TIMEOUT,
                            POSTGRES_MIGRATION_IDLE_TRANSACTION_TIMEOUT,
                        ),
                    };
                    sqlx::query(statement_timeout)
                        .execute(&mut *connection)
                        .await?;
                    sqlx::query(lock_timeout).execute(&mut *connection).await?;
                    sqlx::query(idle_transaction_timeout)
                        .execute(&mut *connection)
                        .await?;
                    Ok(())
                })
            });
        }
        let pool = pool_options.connect(database_url).await?;
        if matches!(backend, DatabaseBackend::Sqlite) {
            // The control, gateway, and worker roles may be separate processes
            // over the same lightweight/test database. WAL keeps readers on a
            // stable snapshot while one process owns the single writer slot;
            // BEGIN IMMEDIATE below then serializes only competing writers.
            // The pragma is file-scoped and idempotent. In-memory databases
            // retain their SQLite-selected journal mode.
            sqlx::query("PRAGMA journal_mode = WAL")
                .fetch_one(&pool)
                .await?;
        }
        Ok(Self { pool, backend })
    }

    pub(crate) async fn begin_write_transaction(
        &self,
    ) -> Result<Transaction<'static, Any>, sqlx::Error> {
        // SQLite's default deferred transaction can fail immediately with
        // SQLITE_BUSY when a read-before-write transaction races another
        // writer: the shared lock cannot be upgraded after that writer has
        // reserved the database. Claim the SQLite write reservation at BEGIN
        // so the configured busy handler can wait and serialize test/local
        // writers. PostgreSQL keeps its normal transaction semantics.
        self.pool
            .begin_with(match self.backend {
                DatabaseBackend::PostgreSql => "BEGIN",
                DatabaseBackend::Sqlite => "BEGIN IMMEDIATE",
            })
            .await
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
}

#[cfg(test)]
mod tests;
