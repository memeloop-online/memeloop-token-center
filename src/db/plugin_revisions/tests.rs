use super::*;
use std::time::Duration;

#[tokio::test]
async fn empty_inventory_publication_waits_for_sqlite_writer_without_losing_cas() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("publication-writer.db").display()
    );
    let database = Database::connect(&database_url).await.unwrap();
    database.migrate().await.unwrap();
    database
        .stage_application_plugin_candidate("empty", "identity", "contract")
        .await
        .unwrap();
    let writer = Database::connect(&database_url).await.unwrap();
    let held = writer.begin_write_transaction().await.unwrap();
    let publication = database.publish_application_plugin(
        "empty",
        0,
        "initial",
        "publish-empty",
        "publish-hash",
        "operator",
    );
    tokio::pin!(publication);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut publication)
            .await
            .is_err(),
        "publication must await the existing writer, not fail a deferred read-to-write upgrade"
    );
    assert!(
        database
            .optional_application_plugin_head()
            .await
            .unwrap()
            .is_none()
    );
    held.commit().await.unwrap();
    let published = tokio::time::timeout(Duration::from_secs(1), publication)
        .await
        .expect("publication must complete after the competing writer releases")
        .unwrap();
    assert_eq!(published.revision, 1);
    assert_eq!(published.inventory_id, "empty");
    assert_eq!(
        database
            .publish_application_plugin(
                "empty",
                0,
                "initial",
                "publish-empty",
                "publish-hash",
                "operator",
            )
            .await
            .unwrap()
            .revision,
        1
    );
    assert!(matches!(
        database
            .publish_application_plugin(
                "empty",
                0,
                "initial",
                "stale-publish",
                "stale-hash",
                "operator",
            )
            .await,
        Err(AppError::Conflict(_))
    ));
    assert!(matches!(
        database
            .publish_application_plugin(
                "empty",
                1,
                "reload",
                "publish-empty",
                "different-hash",
                "operator",
            )
            .await,
        Err(AppError::Conflict(_))
    ));
    assert_eq!(
        database.application_plugin_head().await.unwrap().revision,
        1
    );
    for statement in [
        "SELECT COUNT(*) FROM application_plugin_operations",
        "SELECT COUNT(*) FROM application_plugin_revisions",
        "SELECT COUNT(*) FROM application_plugin_audit WHERE action='publish' AND outcome='published'",
    ] {
        assert_eq!(
            sqlx::query_scalar::<_, i64>(statement)
                .fetch_one(&database.pool)
                .await
                .unwrap(),
            1,
            "{statement}"
        );
    }
}
