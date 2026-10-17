use super::Database;

pub(crate) struct PostgresTestDatabase {
    administrative: Database,
    name: String,
    url: url::Url,
}

impl PostgresTestDatabase {
    pub(crate) async fn create(database_url: &str) -> Self {
        let administrative = Database::connect(database_url).await.unwrap();
        let name = format!("mtc_test_{}", uuid::Uuid::now_v7().simple());
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
            .execute(&administrative.pool)
            .await
            .unwrap();
        let mut url = url::Url::parse(database_url).unwrap();
        url.set_path(&name);
        Self {
            administrative,
            name,
            url,
        }
    }

    pub(crate) fn url(&self) -> &str {
        self.url.as_str()
    }

    pub(crate) async fn cleanup(self) {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE {} WITH (FORCE)",
            self.name
        )))
        .execute(&self.administrative.pool)
        .await
        .unwrap();
        self.administrative.close().await;
    }
}

#[tokio::test]
async fn postgres_fixture_databases_isolate_projection_advisory_locks() {
    use super::projection_lock::{
        lock_request_stats_projection_rebuild_in_transaction,
        lock_request_stats_projection_writer_in_transaction,
    };

    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let first_fixture = PostgresTestDatabase::create(&database_url).await;
    let second_fixture = PostgresTestDatabase::create(&database_url).await;
    let first = Database::connect(first_fixture.url()).await.unwrap();
    let second = Database::connect(second_fixture.url()).await.unwrap();
    let mut rebuild = first.begin_write_transaction().await.unwrap();
    lock_request_stats_projection_rebuild_in_transaction(&mut rebuild)
        .await
        .unwrap();
    let mut same_database = first.begin_write_transaction().await.unwrap();
    sqlx::query("SET LOCAL search_path = pg_catalog")
        .execute(&mut *same_database)
        .await
        .unwrap();
    let acquired: bool = sqlx::query_scalar(
        "SELECT pg_try_advisory_xact_lock_shared(hashtextextended('memeloop-token-center:request-stats', 734627102948314))",
    )
    .fetch_one(&mut *same_database)
    .await
    .unwrap();
    assert!(!acquired);
    let mut writer = second.begin_write_transaction().await.unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        lock_request_stats_projection_writer_in_transaction(&mut writer),
    )
    .await
    .expect("another fixture's rebuild must not block this fixture's writer")
    .unwrap();
    writer.rollback().await.unwrap();
    same_database.rollback().await.unwrap();
    rebuild.rollback().await.unwrap();
    first.close().await;
    second.close().await;
    first_fixture.cleanup().await;
    second_fixture.cleanup().await;
}
