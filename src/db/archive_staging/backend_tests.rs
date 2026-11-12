use super::*;
use crate::{archive::ArchiveStore, archive_staging::ArchiveStagingPurpose};

fn input() -> BeginArchiveStagingInput {
    BeginArchiveStagingInput {
        key: ArchiveStagingKey::new(
            ArchiveStagingOwner::GenerationJob(Uuid::now_v7()),
            ArchiveStagingPurpose::Assets,
            Uuid::now_v7(),
        )
        .unwrap(),
        intent_digest: ArchiveStagingIntentDigest::new("ab".repeat(32)).unwrap(),
        lease_token: Uuid::now_v7(),
        lease_owner: ArchiveStagingLeaseOwner::new("backend-contract").unwrap(),
    }
}

async fn contract(database_url: &str) {
    let database = Database::connect_with_max(database_url, 2).await.unwrap();
    let legacy_input = input();
    let BeginArchiveStagingResult::Created(legacy) = database
        .begin_archive_staging_attempt(legacy_input.clone())
        .await
        .unwrap()
    else {
        panic!("fresh legacy")
    };
    assert!(legacy.key.backend_identity.is_none());
    let training = database
        .clone()
        .with_archive(ArchiveStore::training_staging_memory_for_test());
    // A retry after enabling new writes must return the stored NULL legacy key.
    let BeginArchiveStagingResult::Replayed(replayed) = training
        .begin_archive_staging_attempt(legacy_input)
        .await
        .unwrap()
    else {
        panic!("legacy replay")
    };
    assert_eq!(replayed.key, legacy.key);
    let new_input = input();
    let BeginArchiveStagingResult::Created(lease) = training
        .begin_archive_staging_attempt(new_input.clone())
        .await
        .unwrap()
    else {
        panic!("fresh training")
    };
    assert_eq!(lease.key.backend_identity_text(), Some("a".repeat(64)));
    let stored = training
        .archive_staging_attempt(lease.key.attempt_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.key, lease.key);
    // Conversely disabling the default must never reinterpret an existing key.
    let BeginArchiveStagingResult::Replayed(replayed) = database
        .begin_archive_staging_attempt(new_input)
        .await
        .unwrap()
    else {
        panic!("training replay")
    };
    assert_eq!(replayed.key, lease.key);
    let mut forged = lease.clone();
    forged.key = forged
        .key
        .with_backend_identity(Some(&"b".repeat(64)))
        .unwrap();
    assert!(
        !database
            .bind_archive_staging_attempt(
                &forged,
                &format!("{}/asset", forged.key.canonical_prefix())
            )
            .await
            .unwrap()
    );
    let locator = format!("{}/asset", lease.key.canonical_prefix());
    assert!(
        database
            .bind_archive_staging_attempt(&lease, &locator)
            .await
            .unwrap()
    );
    let stored = database
        .archive_staging_attempt(lease.key.attempt_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.key, lease.key);
    assert_eq!(stored.bound_locator.as_deref(), Some(locator.as_str()));
    database.pool.close().await;
}

#[tokio::test]
async fn sqlite_staging_backend_identity_survives_default_changes() {
    let directory = tempfile::tempdir().unwrap();
    contract(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("staging.db").display()
    ))
    .await;
}

#[tokio::test]
async fn postgres_staging_backend_identity_survives_default_changes() {
    let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let admin = sqlx::PgPool::connect(&url).await.unwrap();
    let schema = format!("staging_backend_{}", Uuid::now_v7().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    let mut isolated = url::Url::parse(&url).unwrap();
    isolated
        .query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema}"));
    contract(isolated.as_str()).await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}
