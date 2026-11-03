use super::*;

const LOCK_TABLES: [&str; 4] = [
    "request_archive_spools",
    "response_archive_spools",
    "response_archive_spool_budget",
    "archive_budget_reservations",
];

struct Fixture {
    database: Database,
    administrative: sqlx::PgPool,
    schema: String,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let url = std::env::var("MTC_TEST_POSTGRES_URL").ok()?;
        let administrative = sqlx::PgPool::connect(&url).await.unwrap();
        let schema = format!("migration143_lock_{}", Uuid::now_v7().simple());
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&administrative)
            .await
            .unwrap();
        let mut scoped_url = url::Url::parse(&url).unwrap();
        scoped_url
            .query_pairs_mut()
            .append_pair("options", &format!("-csearch_path={schema}"));
        let database = Database::connect_for_migration(scoped_url.as_str(), 4)
            .await
            .unwrap();
        for source in [
            "CREATE TABLE schema_migrations (version BIGINT PRIMARY KEY, name TEXT NOT NULL, applied_at BIGINT NOT NULL); INSERT INTO schema_migrations VALUES (142, 'existing142', 123456)",
            "CREATE TABLE request_records (id TEXT PRIMARY KEY)",
            "CREATE TABLE request_events (id TEXT PRIMARY KEY)",
            include_str!("../../../migrations/common/0071_response_archive_spool.sql"),
            include_str!("../../../migrations/common/0080_request_archive_spool.sql"),
            include_str!("../../../migrations/common/0106_archive_budget_reservations.sql"),
            include_str!("../../../migrations/common/0112_request_archive_gap_evidence.sql"),
        ] {
            sqlx::raw_sql(source).execute(&database.pool).await.unwrap();
        }
        Some(Self {
            database,
            administrative,
            schema,
        })
    }

    async fn cleanup(self) {
        self.database.pool.close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP SCHEMA {} CASCADE",
            self.schema
        )))
        .execute(&self.administrative)
        .await
        .unwrap();
        self.administrative.close().await;
    }
}

#[tokio::test]
async fn postgres143_prepares_later_index_and_partition_locks_before_spool_ddl() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    for parent in ["request_records", "request_events"] {
        let mut business = fixture.database.pool.begin().await.unwrap();
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "LOCK TABLE {parent} IN ACCESS SHARE MODE"
        )))
        .execute(&mut *business)
        .await
        .unwrap();
        let mut migration = fixture.database.pool.begin().await.unwrap();
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            apply_migration_range(&mut migration, POSTGRES_MIGRATIONS, 143, 146),
        )
        .await
        .expect("later partition locks must not wait behind a business reader")
        .unwrap_err();
        assert_eq!(
            error.as_database_error().unwrap().code().as_deref(),
            Some("55P03")
        );
        migration.rollback().await.unwrap();
        for table in LOCK_TABLES {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "LOCK TABLE {table} IN ROW EXCLUSIVE MODE NOWAIT"
            )))
            .execute(&mut *business)
            .await
            .expect("maintenance must not retain spool locks behind the business reader");
        }
        business.rollback().await.unwrap();
    }
    fixture.cleanup().await;
}

#[tokio::test]
async fn postgres_pending143_fails_without_waiting_on_each_ddl_target() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    for table in LOCK_TABLES {
        let mut blocker = fixture.database.pool.begin().await.unwrap();
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "LOCK TABLE {table} IN ACCESS SHARE MODE"
        )))
        .execute(&mut *blocker)
        .await
        .unwrap();
        let mut migration = fixture.database.pool.begin().await.unwrap();
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            apply_migration_range(&mut migration, POSTGRES_MIGRATIONS, 143, 143),
        )
        .await
        .expect("maintenance must not queue behind a business reader")
        .unwrap_err();
        assert_eq!(
            error.as_database_error().unwrap().code().as_deref(),
            Some("55P03"),
            "{table}"
        );
        migration.rollback().await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM schema_migrations")
            .fetch_one(&fixture.database.pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
        let columns: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM information_schema.columns WHERE table_schema = current_schema() AND table_name = 'response_archive_spools' AND column_name = 'body_blake3'",
        )
        .fetch_one(&fixture.database.pool)
        .await
        .unwrap();
        assert_eq!(columns, 0);
        let mut business = fixture.database.pool.begin().await.unwrap();
        for target in LOCK_TABLES {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "LOCK TABLE {target} IN ROW EXCLUSIVE MODE NOWAIT"
            )))
            .execute(&mut *business)
            .await
            .expect("failed maintenance must release every earlier table lock");
        }
        business.rollback().await.unwrap();
        blocker.rollback().await.unwrap();
    }
    fixture.cleanup().await;
}

#[tokio::test]
async fn postgres143_preserves_sql_and_skips_locks_after_application() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let mut migration = fixture.database.pool.begin().await.unwrap();
    apply_migration_range(&mut migration, POSTGRES_MIGRATIONS, 143, 143)
        .await
        .unwrap();
    let held: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_locks WHERE pid = pg_backend_pid() AND mode = 'AccessExclusiveLock' AND granted AND relation IN ('request_archive_spools'::regclass, 'response_archive_spools'::regclass, 'response_archive_spool_budget'::regclass, 'archive_budget_reservations'::regclass)",
    )
    .fetch_one(&mut *migration)
    .await
    .unwrap();
    assert_eq!(held, 4);
    migration.commit().await.unwrap();
    let before: Vec<(i64, String, i64)> =
        sqlx::query_as("SELECT version, name, applied_at FROM schema_migrations ORDER BY version")
            .fetch_all(&fixture.database.pool)
            .await
            .unwrap();
    assert_eq!(before[0], (142, "existing142".into(), 123456));
    assert_eq!(before[1].0, 143);
    let mut business = fixture.database.pool.begin().await.unwrap();
    for table in LOCK_TABLES {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "LOCK TABLE {table} IN ACCESS SHARE MODE"
        )))
        .execute(&mut *business)
        .await
        .unwrap();
    }
    let mut repeated = fixture.database.pool.begin().await.unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        apply_migration_range(&mut repeated, POSTGRES_MIGRATIONS, 143, 143),
    )
    .await
    .expect("already applied143 must not acquire spool locks")
    .unwrap();
    repeated.commit().await.unwrap();
    let after: Vec<(i64, String, i64)> =
        sqlx::query_as("SELECT version, name, applied_at FROM schema_migrations ORDER BY version")
            .fetch_all(&fixture.database.pool)
            .await
            .unwrap();
    assert_eq!(after, before);
    business.rollback().await.unwrap();
    fixture.cleanup().await;
}
