use super::super::*;

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
    let reversals: i64 =
        sqlx::query("SELECT COUNT(*) AS count FROM ledger_entries WHERE kind = 'grant_reversal'")
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

    let maximum_plugin_id = "p".repeat(64);
    database
        .plugin_kv_put(&maximum_plugin_id, "boundary", b"accepted")
        .await
        .unwrap();
    assert!(matches!(
        database
            .plugin_kv_put(&"p".repeat(65), "boundary", b"rejected")
            .await,
        Err(AppError::BadRequest(message))
            if message == "plugin id must contain lowercase ASCII letters, digits, or hyphens"
    ));

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
