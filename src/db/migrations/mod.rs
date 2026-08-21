use super::*;

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

pub(crate) struct Migration {
    pub(crate) version: i64,
    pub(crate) name: &'static str,
    pub(crate) sql: &'static str,
}

pub(crate) const SQLITE_MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "initial schema",
        sql: include_str!("../../../migrations/sqlite/0001_initial.sql"),
    },
    Migration {
        version: 2,
        name: "high volume query indexes",
        sql: include_str!("../../../migrations/sqlite/0002_query_indexes.sql"),
    },
    Migration {
        version: 3,
        name: "scoped service credentials",
        sql: include_str!("../../../migrations/common/0003_service_tokens.sql"),
    },
    Migration {
        version: 4,
        name: "request event stream",
        sql: include_str!("../../../migrations/sqlite/0004_request_events.sql"),
    },
    Migration {
        version: 5,
        name: "asynchronous generation jobs",
        sql: include_str!("../../../migrations/sqlite/0005_generation_jobs.sql"),
    },
    Migration {
        version: 6,
        name: "idempotent key provisioning",
        sql: include_str!("../../../migrations/sqlite/0006_key_provisioning.sql"),
    },
    Migration {
        version: 7,
        name: "idempotent grant reversals",
        sql: include_str!("../../../migrations/sqlite/0007_grant_reversals.sql"),
    },
    Migration {
        version: 8,
        name: "bounded plugin KV",
        sql: include_str!("../../../migrations/sqlite/0008_plugin_kv.sql"),
    },
    Migration {
        version: 9,
        name: "structured conversation hints",
        sql: include_str!("../../../migrations/sqlite/0009_structured_conversation_hints.sql"),
    },
    Migration {
        version: 10,
        name: "operator aggregate indexes",
        sql: include_str!("../../../migrations/sqlite/0010_operator_aggregate_indexes.sql"),
    },
    Migration {
        version: 11,
        name: "legacy key credentials",
        sql: include_str!("../../../migrations/sqlite/0011_legacy_key_credentials.sql"),
    },
    Migration {
        version: 12,
        name: "tenant scoped idempotency",
        sql: include_str!("../../../migrations/sqlite/0012_tenant_idempotency.sql"),
    },
    Migration {
        version: 13,
        name: "generation price snapshots",
        sql: include_str!("../../../migrations/common/0013_generation_price_snapshot.sql"),
    },
    Migration {
        version: 14,
        name: "idempotent credential rotation",
        sql: include_str!("../../../migrations/common/0014_credential_rotation_idempotency.sql"),
    },
    Migration {
        version: 15,
        name: "idempotent generation jobs",
        sql: include_str!("../../../migrations/common/0015_generation_job_idempotency.sql"),
    },
    Migration {
        version: 16,
        name: "conversation upstream response ids",
        sql: include_str!("../../../migrations/common/0016_conversation_upstream_response_ids.sql"),
    },
    Migration {
        version: 17,
        name: "subscription entitlement reconciliation",
        sql: include_str!("../../../migrations/common/0017_subscription_entitlements.sql"),
    },
    Migration {
        version: 18,
        name: "model price service and cache tiers",
        sql: include_str!("../../../migrations/common/0018_model_price_tiers.sql"),
    },
    Migration {
        version: 19,
        name: "session archive import provenance",
        sql: include_str!("../../../migrations/common/0019_session_archive_import.sql"),
    },
    Migration {
        version: 20,
        name: "bounded observability query indexes",
        sql: include_str!("../../../migrations/sqlite/0020_observability_indexes.sql"),
    },
    Migration {
        version: 21,
        name: "global request and event locators",
        sql: include_str!("../../../migrations/common/0021_request_locators.sql"),
    },
    Migration {
        version: 22,
        name: "transactional budget rollups",
        sql: include_str!("../../../migrations/common/0022_budget_rollups.sql"),
    },
    Migration {
        version: 23,
        name: "generation daily aggregates",
        sql: include_str!("../../../migrations/common/0023_generation_daily_aggregates.sql"),
    },
    Migration {
        version: 24,
        name: "request statistics facts and daily aggregates",
        sql: include_str!("../../../migrations/common/0024_request_stats_rollups.sql"),
    },
    Migration {
        version: 25,
        name: "key scoped conversation projections",
        sql: include_str!("../../../migrations/common/0025_conversation_key_clusters.sql"),
    },
    Migration {
        version: 26,
        name: "authorized generation assets",
        sql: include_str!("../../../migrations/common/0026_generation_assets.sql"),
    },
    Migration {
        version: 27,
        name: "CPAMP immutable source digests",
        sql: include_str!("../../../migrations/common/0027_cpamp_source_digests.sql"),
    },
    Migration {
        version: 28,
        name: "session archive unlinked provenance",
        sql: include_str!("../../../migrations/common/0028_session_archive_unlinked.sql"),
    },
    Migration {
        version: 29,
        name: "generation worker submission and staging state",
        sql: include_str!("../../../migrations/common/0029_generation_worker_state.sql"),
    },
    Migration {
        version: 30,
        name: "generation preparation lease index",
        sql: include_str!("../../../migrations/common/0030_generation_preparing_lease.sql"),
    },
    Migration {
        version: 31,
        name: "conversation key isolation repair",
        sql: include_str!("../../../migrations/common/0031_conversation_key_isolation.sql"),
    },
    Migration {
        version: 32,
        name: "unified upstream OAuth lifecycle",
        sql: include_str!("../../../migrations/common/0032_upstream_oauth_lifecycle.sql"),
    },
    Migration {
        version: 33,
        name: "legacy credential global one-to-one mapping",
        sql: include_str!("../../../migrations/common/0033_legacy_credential_one_to_one.sql"),
    },
    Migration {
        version: 34,
        name: "CPA managed OAuth account imports",
        sql: include_str!("../../../migrations/common/0034_cpa_managed_oauth_imports.sql"),
    },
    Migration {
        version: 35,
        name: "durable archive staging attempts",
        sql: include_str!("../../../migrations/common/0035_archive_staging_attempts.sql"),
    },
    Migration {
        version: 36,
        name: "operator-only session archive quarantine",
        sql: include_str!("../../../migrations/common/0036_session_archive_quarantine.sql"),
    },
    Migration {
        version: 37,
        name: "MemeLoop Cloud subscription event audit",
        sql: include_str!("../../../migrations/common/0037_memeloop_cloud_subscription_events.sql"),
    },
    Migration {
        version: 38,
        name: "credential and billing keyset pagination",
        sql: include_str!("../../../migrations/common/0038_credential_billing_pagination.sql"),
    },
    Migration {
        version: 39,
        name: "observability currency snapshots and selective filters",
        sql: include_str!("../../../migrations/common/0039_observability_currency_and_filters.sql"),
    },
    Migration {
        version: 40,
        name: "generation cancellation claim index",
        sql: include_str!("../../../migrations/common/0040_generation_cancellation.sql"),
    },
    Migration {
        version: 41,
        name: "tenant-scoped plugin configurations",
        sql: include_str!("../../../migrations/common/0041_plugin_configurations.sql"),
    },
    Migration {
        version: 42,
        name: "bounded control list pagination indexes",
        sql: include_str!("../../../migrations/common/0042_control_list_pagination.sql"),
    },
    Migration {
        version: 43,
        name: "tenant-scoped routing groups and grants",
        sql: concat!(
            include_str!("../../../migrations/sqlite/0043_drop_model_route_legacy_unique.sql"),
            include_str!("../../../migrations/common/0043_routing_groups.sql")
        ),
    },
    Migration {
        version: 44,
        name: "atomic upstream model catalogs",
        sql: include_str!("../../../migrations/common/0044_upstream_model_catalog.sql"),
    },
    Migration {
        version: 45,
        name: "durable OAuth login sessions",
        sql: include_str!("../../../migrations/common/0045_oauth_login_sessions.sql"),
    },
    Migration {
        version: 46,
        name: "route model catalog policy",
        sql: include_str!("../../../migrations/common/0046_route_catalog_policy.sql"),
    },
    Migration {
        version: 47,
        name: "immutable generation route snapshots",
        sql: include_str!("../../../migrations/common/0047_generation_route_snapshot.sql"),
    },
    Migration {
        version: 48,
        name: "routing grant relation revisions",
        sql: include_str!("../../../migrations/common/0048_routing_grant_relation_revisions.sql"),
    },
    Migration {
        version: 49,
        name: "MemeLoop Cloud event query indexes",
        sql: include_str!("../../../migrations/common/0049_memeloop_cloud_event_queries.sql"),
    },
    Migration {
        version: 50,
        name: "generation usage modality and billing dimensions",
        sql: include_str!("../../../migrations/common/0050_generation_usage_dimensions.sql"),
    },
    Migration {
        version: 51,
        name: "logical session usage rollups",
        sql: include_str!("../../../migrations/common/0051_session_usage_rollups.sql"),
    },
    Migration {
        version: 52,
        name: "retire legacy allowed model policies",
        sql: include_str!("../../../migrations/sqlite/0052_retire_allowed_models.sql"),
    },
];

pub(crate) const POSTGRES_MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "initial partitioned schema",
        sql: include_str!("../../../migrations/postgres/0001_initial.sql"),
    },
    Migration {
        version: 2,
        name: "high volume query indexes",
        sql: include_str!("../../../migrations/postgres/0002_query_indexes.sql"),
    },
    Migration {
        version: 3,
        name: "scoped service credentials",
        sql: include_str!("../../../migrations/common/0003_service_tokens.sql"),
    },
    Migration {
        version: 4,
        name: "partitioned request event stream",
        sql: include_str!("../../../migrations/postgres/0004_request_events.sql"),
    },
    Migration {
        version: 5,
        name: "asynchronous generation jobs",
        sql: include_str!("../../../migrations/postgres/0005_generation_jobs.sql"),
    },
    Migration {
        version: 6,
        name: "idempotent key provisioning",
        sql: include_str!("../../../migrations/postgres/0006_key_provisioning.sql"),
    },
    Migration {
        version: 7,
        name: "idempotent grant reversals",
        sql: include_str!("../../../migrations/postgres/0007_grant_reversals.sql"),
    },
    Migration {
        version: 8,
        name: "bounded plugin KV",
        sql: include_str!("../../../migrations/postgres/0008_plugin_kv.sql"),
    },
    Migration {
        version: 9,
        name: "structured conversation hints",
        sql: include_str!("../../../migrations/postgres/0009_structured_conversation_hints.sql"),
    },
    Migration {
        version: 10,
        name: "operator aggregate indexes",
        sql: include_str!("../../../migrations/postgres/0010_operator_aggregate_indexes.sql"),
    },
    Migration {
        version: 11,
        name: "legacy key credentials",
        sql: include_str!("../../../migrations/postgres/0011_legacy_key_credentials.sql"),
    },
    Migration {
        version: 12,
        name: "tenant scoped idempotency",
        sql: include_str!("../../../migrations/postgres/0012_tenant_idempotency.sql"),
    },
    Migration {
        version: 13,
        name: "generation price snapshots",
        sql: include_str!("../../../migrations/common/0013_generation_price_snapshot.sql"),
    },
    Migration {
        version: 14,
        name: "idempotent credential rotation",
        sql: include_str!("../../../migrations/common/0014_credential_rotation_idempotency.sql"),
    },
    Migration {
        version: 15,
        name: "idempotent generation jobs",
        sql: include_str!("../../../migrations/common/0015_generation_job_idempotency.sql"),
    },
    Migration {
        version: 16,
        name: "conversation upstream response ids",
        sql: include_str!("../../../migrations/common/0016_conversation_upstream_response_ids.sql"),
    },
    Migration {
        version: 17,
        name: "subscription entitlement reconciliation",
        sql: include_str!("../../../migrations/common/0017_subscription_entitlements.sql"),
    },
    Migration {
        version: 18,
        name: "model price service and cache tiers",
        sql: include_str!("../../../migrations/common/0018_model_price_tiers.sql"),
    },
    Migration {
        version: 19,
        name: "session archive import provenance",
        sql: include_str!("../../../migrations/common/0019_session_archive_import.sql"),
    },
    Migration {
        version: 20,
        name: "bounded observability query indexes",
        sql: include_str!("../../../migrations/postgres/0020_observability_indexes.sql"),
    },
    Migration {
        version: 21,
        name: "global request and event locators",
        sql: include_str!("../../../migrations/common/0021_request_locators.sql"),
    },
    Migration {
        version: 22,
        name: "transactional budget rollups",
        sql: include_str!("../../../migrations/common/0022_budget_rollups.sql"),
    },
    Migration {
        version: 23,
        name: "generation daily aggregates",
        sql: include_str!("../../../migrations/common/0023_generation_daily_aggregates.sql"),
    },
    Migration {
        version: 24,
        name: "request statistics facts and daily aggregates",
        sql: concat!(
            include_str!("../../../migrations/common/0024_request_stats_rollups.sql"),
            include_str!("../../../migrations/postgres/0024_history_cursor_indexes.sql")
        ),
    },
    Migration {
        version: 25,
        name: "key scoped conversation projections",
        sql: include_str!("../../../migrations/common/0025_conversation_key_clusters.sql"),
    },
    Migration {
        version: 26,
        name: "authorized generation assets",
        sql: include_str!("../../../migrations/common/0026_generation_assets.sql"),
    },
    Migration {
        version: 27,
        name: "CPAMP immutable source digests",
        sql: include_str!("../../../migrations/common/0027_cpamp_source_digests.sql"),
    },
    Migration {
        version: 28,
        name: "session archive unlinked provenance",
        sql: include_str!("../../../migrations/common/0028_session_archive_unlinked.sql"),
    },
    Migration {
        version: 29,
        name: "generation worker submission and staging state",
        sql: include_str!("../../../migrations/common/0029_generation_worker_state.sql"),
    },
    Migration {
        version: 30,
        name: "generation preparation lease index",
        sql: include_str!("../../../migrations/common/0030_generation_preparing_lease.sql"),
    },
    Migration {
        version: 31,
        name: "conversation key isolation repair",
        sql: include_str!("../../../migrations/common/0031_conversation_key_isolation.sql"),
    },
    Migration {
        version: 32,
        name: "unified upstream OAuth lifecycle",
        sql: include_str!("../../../migrations/common/0032_upstream_oauth_lifecycle.sql"),
    },
    Migration {
        version: 33,
        name: "legacy credential global one-to-one mapping",
        sql: include_str!("../../../migrations/common/0033_legacy_credential_one_to_one.sql"),
    },
    Migration {
        version: 34,
        name: "CPA managed OAuth account imports",
        sql: include_str!("../../../migrations/common/0034_cpa_managed_oauth_imports.sql"),
    },
    Migration {
        version: 35,
        name: "durable archive staging attempts",
        sql: include_str!("../../../migrations/common/0035_archive_staging_attempts.sql"),
    },
    Migration {
        version: 36,
        name: "operator-only session archive quarantine",
        sql: include_str!("../../../migrations/common/0036_session_archive_quarantine.sql"),
    },
    Migration {
        version: 37,
        name: "MemeLoop Cloud subscription event audit",
        sql: include_str!("../../../migrations/common/0037_memeloop_cloud_subscription_events.sql"),
    },
    Migration {
        version: 38,
        name: "credential and billing keyset pagination",
        sql: include_str!("../../../migrations/common/0038_credential_billing_pagination.sql"),
    },
    Migration {
        version: 39,
        name: "observability currency snapshots and selective filters",
        sql: include_str!("../../../migrations/common/0039_observability_currency_and_filters.sql"),
    },
    Migration {
        version: 40,
        name: "generation cancellation claim index",
        sql: include_str!("../../../migrations/common/0040_generation_cancellation.sql"),
    },
    Migration {
        version: 41,
        name: "tenant-scoped plugin configurations",
        sql: include_str!("../../../migrations/common/0041_plugin_configurations.sql"),
    },
    Migration {
        version: 42,
        name: "bounded control list pagination indexes",
        sql: include_str!("../../../migrations/common/0042_control_list_pagination.sql"),
    },
    Migration {
        version: 43,
        name: "tenant-scoped routing groups and grants",
        sql: concat!(
            include_str!("../../../migrations/postgres/0043_drop_model_route_legacy_unique.sql"),
            include_str!("../../../migrations/common/0043_routing_groups.sql")
        ),
    },
    Migration {
        version: 44,
        name: "atomic upstream model catalogs",
        sql: include_str!("../../../migrations/common/0044_upstream_model_catalog.sql"),
    },
    Migration {
        version: 45,
        name: "durable OAuth login sessions",
        sql: include_str!("../../../migrations/common/0045_oauth_login_sessions.sql"),
    },
    Migration {
        version: 46,
        name: "route model catalog policy",
        sql: include_str!("../../../migrations/common/0046_route_catalog_policy.sql"),
    },
    Migration {
        version: 47,
        name: "immutable generation route snapshots",
        sql: include_str!("../../../migrations/common/0047_generation_route_snapshot.sql"),
    },
    Migration {
        version: 48,
        name: "routing grant relation revisions",
        sql: include_str!("../../../migrations/common/0048_routing_grant_relation_revisions.sql"),
    },
    Migration {
        version: 49,
        name: "MemeLoop Cloud event query indexes",
        sql: include_str!("../../../migrations/common/0049_memeloop_cloud_event_queries.sql"),
    },
    Migration {
        version: 50,
        name: "generation usage modality and billing dimensions",
        sql: include_str!("../../../migrations/common/0050_generation_usage_dimensions.sql"),
    },
    Migration {
        version: 51,
        name: "logical session usage rollups",
        sql: include_str!("../../../migrations/common/0051_session_usage_rollups.sql"),
    },
    Migration {
        version: 52,
        name: "retire legacy allowed model policies",
        sql: include_str!("../../../migrations/postgres/0052_retire_allowed_models.sql"),
    },
];

impl Database {
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
        // The legacy `*` policy is a snapshot of routes that existed at the
        // upgrade boundary, not a standing grant for routes created later.
        // Capture this before v43 is applied so subsequent startups never
        // widen a credential's authorization.
        let routing_groups_need_backfill =
            sqlx::query("SELECT version FROM schema_migrations WHERE version = 43")
                .fetch_optional(&mut *transaction)
                .await?
                .is_none();
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
                let statement = match column {
                    "upstream_account_id" => {
                        "ALTER TABLE request_records ADD COLUMN upstream_account_id TEXT"
                    }
                    "model_route_id" => {
                        "ALTER TABLE request_records ADD COLUMN model_route_id TEXT"
                    }
                    _ => unreachable!("migration column names are a closed internal set"),
                };
                sqlx::query(statement).execute(&mut *transaction).await?;
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
        // v43 freezes the legacy model-name policy into exact route IDs. It
        // must complete before v52 removes that legacy field from policy JSON.
        apply_migration_range(&mut transaction, migrations, 2, 43).await?;
        if routing_groups_need_backfill {
            backfill_routing_grants_from_legacy_policy(&mut transaction).await?;
        }
        apply_migration_range(&mut transaction, migrations, 44, i64::MAX).await?;
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
}

#[derive(serde::Deserialize)]
struct LegacyRoutingPolicy {
    #[serde(default)]
    allowed_models: Vec<String>,
}

/// Convert the legacy model allowlist into immutable route grants once, in the
/// same transaction that applies v43. Malformed policy JSON aborts the entire
/// migration; it must never silently turn into broader or narrower access.
pub(super) async fn backfill_routing_grants_from_legacy_policy(
    transaction: &mut sqlx::Transaction<'_, sqlx::Any>,
) -> Result<u64, sqlx::Error> {
    const PAGE_SIZE: i64 = 256;
    let mut last_key_id = String::new();
    let mut inserted = 0_u64;
    loop {
        let keys = sqlx::query(
            "SELECT id, tenant_id, policy_json, created_at FROM key_records WHERE id > $1 ORDER BY id ASC LIMIT $2",
        )
        .bind(&last_key_id)
        .bind(PAGE_SIZE)
        .fetch_all(&mut **transaction)
        .await?;
        if keys.is_empty() {
            break;
        }
        for key in &keys {
            let key_id: String = key.try_get("id")?;
            let tenant_id: String = key.try_get("tenant_id")?;
            let policy_json: String = key.try_get("policy_json")?;
            let created_at: i64 = key.try_get("created_at")?;
            let policy: LegacyRoutingPolicy =
                serde_json::from_str(&policy_json).map_err(|error| {
                    sqlx::Error::Protocol(format!(
                        "invalid legacy routing policy for key {key_id}: {error}"
                    ))
                })?;
            let allowed_models = policy
                .allowed_models
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>();
            if allowed_models.contains("*") {
                inserted += sqlx::query(
                    "INSERT INTO routing_grants (tenant_id, key_id, model_route_id, route_group_id, created_at) SELECT r.tenant_id, $2, r.id, NULL, $3 FROM model_routes r WHERE r.tenant_id = $1 ON CONFLICT DO NOTHING",
                )
                .bind(&tenant_id)
                .bind(&key_id)
                .bind(created_at)
                .execute(&mut **transaction)
                .await?
                .rows_affected();
            } else {
                for model in allowed_models {
                    inserted += sqlx::query(
                        "INSERT INTO routing_grants (tenant_id, key_id, model_route_id, route_group_id, created_at) SELECT r.tenant_id, $2, r.id, NULL, $3 FROM model_routes r WHERE r.tenant_id = $1 AND r.public_model = $4 ON CONFLICT DO NOTHING",
                    )
                    .bind(&tenant_id)
                    .bind(&key_id)
                    .bind(created_at)
                    .bind(model)
                    .execute(&mut **transaction)
                    .await?
                    .rows_affected();
                }
            }
            last_key_id = key_id;
        }
    }
    Ok(inserted)
}

pub(super) async fn apply_migration_range(
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

pub(super) async fn maintain_postgres_partitions(
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
            sqlx::query("SAVEPOINT memeloop_partition_maintenance")
                .execute(&mut *connection)
                .await?;
            // SQL safety boundary: table is selected from the two literals above, suffix is a
            // chrono-rendered YYYYMMDD date, and bounds are server-generated i64 timestamps.
            // No request, configuration, or plugin value can enter this DDL statement.
            match sqlx::query(sqlx::AssertSqlSafe(statement))
                .execute(&mut *connection)
                .await
            {
                Ok(_) => {
                    sqlx::query("RELEASE SAVEPOINT memeloop_partition_maintenance")
                        .execute(&mut *connection)
                        .await?;
                    report.ready_partitions += 1;
                }
                Err(error) => {
                    // A PostgreSQL statement error aborts the transaction until it is rolled back.
                    // Keep each partition DDL behind a savepoint so a blocked day cannot prevent
                    // the migration transaction from committing or later days from being created.
                    sqlx::query("ROLLBACK TO SAVEPOINT memeloop_partition_maintenance")
                        .execute(&mut *connection)
                        .await?;
                    sqlx::query("RELEASE SAVEPOINT memeloop_partition_maintenance")
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
        let statement = match table {
            "request_records" => {
                "CREATE TABLE IF NOT EXISTS request_records_default PARTITION OF request_records DEFAULT"
            }
            "request_events" => {
                "CREATE TABLE IF NOT EXISTS request_events_default PARTITION OF request_events DEFAULT"
            }
            _ => unreachable!("partitioned table names are a closed internal set"),
        };
        sqlx::query(statement).execute(&mut *connection).await?;
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
