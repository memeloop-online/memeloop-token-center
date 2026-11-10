use super::*;

async fn scheduled_claim_contract(database: &Database) {
    let id = account(database, &format!("scheduled-{}", Uuid::now_v7())).await;
    let context = database.probe_account_context(id).await.unwrap();
    let now = super::super::super::unix_millis();
    let interval = 3_600_000;
    let (first, second) = tokio::join!(
        database.claim_upstream_service_probe(id, now, interval),
        database.claim_upstream_service_probe(id, now, interval),
    );
    assert_ne!(first.unwrap(), second.unwrap(), "one worker owns dispatch");
    // A terminated worker or failed result write must not repeat paid traffic
    // at the next minute tick, nor just before the configured interval.
    for retry_at in [now + 60_000, now + interval - 1] {
        assert!(
            !database
                .claim_upstream_service_probe(id, retry_at, interval)
                .await
                .unwrap()
        );
    }
    let result = serde_json::json!({
        "status": "healthy", "checked_at": now + 10, "source": "model_probe",
        "credential_generation": context.credential_generation,
        "transport_revision": context.transport_revision,
    });
    database
        .store_upstream_service_probe(id, &result)
        .await
        .unwrap();
    // Claiming another attempt preserves the last completed health result.
    let next = now + interval + 10;
    let (first, second) = tokio::join!(
        database.claim_upstream_service_probe(id, next, interval),
        database.claim_upstream_service_probe(id, next, interval),
    );
    assert_ne!(first.unwrap(), second.unwrap());
    let visible = database.upstream_service_probe(id).await.unwrap().unwrap();
    assert_eq!(visible["status"], "healthy");
    assert_eq!(visible["checked_at"], now + 10);
    // A late write from an older attempt cannot roll the reservation back.
    database
        .store_upstream_service_probe(id, &result)
        .await
        .unwrap();
    assert_eq!(
        database
            .upstream_service_probe_last_attempt(id)
            .await
            .unwrap(),
        Some(next)
    );
    sqlx::query("UPDATE upstream_accounts SET status = 'disabled' WHERE id = $1")
        .bind(id.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    assert!(
        !database
            .claim_upstream_service_probe(id, next + interval, interval)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn sqlite_scheduled_probe_claim_is_atomic_and_durable() {
    let directory = tempfile::tempdir().unwrap();
    let database = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("probe-claims.db").display()
    ))
    .await
    .unwrap();
    database.migrate().await.unwrap();
    scheduled_claim_contract(&database).await;
    database.pool.close().await;
}

#[tokio::test]
async fn postgres_scheduled_probe_claim_is_atomic_and_durable() {
    let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let database = Database::connect(&url).await.unwrap();
    database.migrate().await.unwrap();
    scheduled_claim_contract(&database).await;
    database.pool.close().await;
}
