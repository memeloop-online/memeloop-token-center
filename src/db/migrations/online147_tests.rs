use super::*;

struct Fixture {
    database: Database,
    administrative: sqlx::PgPool,
    schema: String,
    url: String,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let url = std::env::var("MTC_TEST_POSTGRES_URL").ok()?;
        let administrative = sqlx::PgPool::connect(&url).await.unwrap();
        let schema = format!("migration147_online_{}", Uuid::now_v7().simple());
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&administrative)
            .await
            .unwrap();
        let mut scoped = url::Url::parse(&url).unwrap();
        scoped
            .query_pairs_mut()
            .append_pair("options", &format!("-csearch_path={schema}"));
        let url = scoped.to_string();
        let database = Database::connect_for_migration(&url, 1).await.unwrap();
        sqlx::raw_sql("CREATE TABLE schema_migrations (version BIGINT PRIMARY KEY, name TEXT NOT NULL, applied_at BIGINT NOT NULL);
            INSERT INTO schema_migrations VALUES (142, 'original142', 123456);
            CREATE TABLE session_generated_titles (id BIGINT PRIMARY KEY);
            CREATE TABLE conversation_observations (explicit_session_id TEXT, key_id TEXT, cluster_id TEXT);
            CREATE TABLE request_stats_facts (tenant_id TEXT, key_id TEXT, session_id TEXT, created_at BIGINT, request_id TEXT);
            INSERT INTO conversation_observations VALUES ('session', 'key', 'cluster');
            INSERT INTO request_stats_facts VALUES ('tenant', 'key', 'session', 123, 'request')")
            .execute(&database.pool).await.unwrap();
        Some(Self {
            database,
            administrative,
            schema,
            url,
        })
    }

    async fn indexes(&self) -> Vec<(String, i64, bool)> {
        sqlx::query_as("SELECT c.relname::text, c.oid::bigint, i.indisvalid AND i.indisready AND i.indislive
            FROM pg_index i JOIN pg_class c ON c.oid=i.indexrelid
            JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname=current_schema()
            AND c.relname IN ('conversation_observations_explicit_session_idx', 'request_stats_facts_session_idx') ORDER BY c.relname")
            .fetch_all(&self.database.pool).await.unwrap()
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
async fn postgres147_reuses_concurrent_index_oids_and_preserves_applied_identity() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    {
        let mut connection = fixture.database.pool.acquire().await.unwrap();
        online147::prepare(&mut connection).await.unwrap();
    }
    let before = fixture.indexes().await;
    assert_eq!(before.len(), 2);
    assert!(before.iter().all(|entry| entry.2));
    let mut transaction = fixture.database.pool.begin().await.unwrap();
    apply_migration_range(&mut transaction, POSTGRES_MIGRATIONS, 147, 147)
        .await
        .unwrap();
    transaction.commit().await.unwrap();
    let ledger: Vec<(i64, String, i64)> =
        sqlx::query_as("SELECT version,name,applied_at FROM schema_migrations ORDER BY version")
            .fetch_all(&fixture.database.pool)
            .await
            .unwrap();
    assert_eq!(ledger[0], (142, "original142".into(), 123456));
    assert_eq!(ledger[1].0, 147);
    let blockers = sqlx::PgPool::connect(&fixture.url).await.unwrap();
    let mut business = blockers.begin().await.unwrap();
    sqlx::query(
        "LOCK TABLE conversation_observations, request_stats_facts IN ACCESS EXCLUSIVE MODE",
    )
    .execute(&mut *business)
    .await
    .unwrap();
    {
        let mut connection = fixture.database.pool.acquire().await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), online147::prepare(&mut connection))
            .await
            .unwrap()
            .unwrap();
    }
    let mut repeated = fixture.database.pool.begin().await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(2),
        apply_migration_range(&mut repeated, POSTGRES_MIGRATIONS, 147, 147),
    )
    .await
    .unwrap()
    .unwrap();
    repeated.commit().await.unwrap();
    business.rollback().await.unwrap();
    blockers.close().await;
    assert_eq!(fixture.indexes().await, before);
    let after: Vec<(i64, String, i64)> =
        sqlx::query_as("SELECT version,name,applied_at FROM schema_migrations ORDER BY version")
            .fetch_all(&fixture.database.pool)
            .await
            .unwrap();
    assert_eq!(after, ledger);
    fixture.cleanup().await;
}

#[tokio::test]
async fn postgres147_rejects_wrong_same_name_index_without_replacing_it() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    sqlx::query("CREATE INDEX request_stats_facts_session_idx ON request_stats_facts (request_id)")
        .execute(&fixture.database.pool)
        .await
        .unwrap();
    let original = fixture.indexes().await;
    let mut connection = fixture.database.pool.acquire().await.unwrap();
    let error = online147::prepare(&mut connection).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("request_stats_facts_session_idx")
    );
    drop(connection);
    assert!(fixture.indexes().await.contains(&original[0]));
    let applied: i64 =
        sqlx::query_scalar("SELECT count(*) FROM schema_migrations WHERE version=147")
            .fetch_one(&fixture.database.pool)
            .await
            .unwrap();
    assert_eq!(applied, 0);
    fixture.cleanup().await;
}

#[tokio::test]
async fn postgres147_recovers_only_exact_invalid_index_after_owned_cic_cancel() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let mut blocker = fixture.database.pool.acquire().await.unwrap();
    sqlx::raw_sql("BEGIN; LOCK TABLE conversation_observations IN ROW EXCLUSIVE MODE")
        .execute(&mut *blocker)
        .await
        .unwrap();
    let builder = sqlx::PgPool::connect(&fixture.url).await.unwrap();
    let mut connection = builder.acquire().await.unwrap();
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *connection)
        .await
        .unwrap();
    let build = tokio::spawn(async move {
        sqlx::query("CREATE INDEX CONCURRENTLY conversation_observations_explicit_session_idx ON conversation_observations (explicit_session_id, key_id, cluster_id)")
            .execute(&mut *connection).await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let waiting: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_stat_progress_create_index WHERE pid=$1 AND phase='waiting for writers before build')")
                .bind(pid).fetch_one(&fixture.administrative).await.unwrap();
            if waiting { break; }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.unwrap();
    sqlx::query("SELECT pg_cancel_backend($1)")
        .bind(pid)
        .execute(&fixture.administrative)
        .await
        .unwrap();
    let error = build.await.unwrap().unwrap_err();
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some("57014")
    );
    sqlx::query("ROLLBACK")
        .execute(&mut *blocker)
        .await
        .unwrap();
    drop(blocker);
    let invalid = fixture.indexes().await;
    assert_eq!(invalid.len(), 1);
    assert!(!invalid[0].2);
    {
        let mut connection = fixture.database.pool.acquire().await.unwrap();
        online147::prepare(&mut connection).await.unwrap();
    }
    let repaired = fixture.indexes().await;
    assert_eq!(repaired.len(), 2);
    assert!(repaired.iter().all(|entry| entry.2));
    assert_ne!(repaired[0].1, invalid[0].1);
    builder.close().await;
    fixture.cleanup().await;
}
