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
];

const PARTITION_MAINTENANCE_SAVEPOINT: &str = "memeloop_partition_maintenance";

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
