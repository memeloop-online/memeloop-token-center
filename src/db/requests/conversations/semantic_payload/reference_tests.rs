use super::*;

async fn reference_contract(url: &str) {
    use super::super::prefix_storage::tests::key;
    let archive = crate::archive::ArchiveStore::training_memory_for_test();
    let database = Database::connect(url)
        .await
        .unwrap()
        .with_archive(archive.clone());
    database.migrate().await.unwrap();
    let key = key(&database, "semantic-reference").await;
    let request = Uuid::now_v7();
    let body = serde_json::json!({"input":[{"role":"user","content":"原文🚀"}]});
    let (encoded, semantic_digest) = encode(request, &key, &body).unwrap();
    let hints = ConversationHints::default();
    let mut tx = database.begin_write_transaction().await.unwrap();
    enqueue_conversation_projection_in_transaction(
        &mut tx,
        ConversationProjectionEnqueueInput {
            request_id: request,
            key: &key,
            hints: &hints,
            client_name: None,
            upstream_response_id: None,
            observed_at: 1,
        },
    )
    .await
    .unwrap();
    persist(&mut tx, request, &key, &encoded, &semantic_digest)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let tenant = key.tenant_id.to_string();
    let key_id = key.key_id.to_string();
    let principal = key.principal_id.to_string();
    let legacy = load(
        &database,
        request,
        &tenant,
        &key_id,
        &principal,
        &semantic_digest,
    )
    .await
    .unwrap();
    assert_eq!(legacy.body, encoded);
    let object = archive
        .put_tenant_content(key.tenant_id, bytes::Bytes::from(encoded.clone()))
        .await
        .unwrap();
    let reference = serde_json::to_string(&crate::archive::ConversationBodyReference {
        conversation_body_v1: object,
        chars: encoded.chars().count() as u64,
    })
    .unwrap();
    sqlx::query("UPDATE conversation_semantic_payloads SET format_version=2, request_json=$1 WHERE request_id=$2")
        .bind(&reference).bind(request.to_string()).execute(&database.pool).await.unwrap();
    let loaded = load(
        &database,
        request,
        &tenant,
        &key_id,
        &principal,
        &semantic_digest,
    )
    .await
    .unwrap();
    assert_eq!(loaded.body, encoded);
    let reduced = load_reduced(&database, request, &tenant, &key_id, &principal)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reduced.body, encoded);
    let mut tx = database.begin_write_transaction().await.unwrap();
    verify_in_transaction(&mut tx, database.backend, request, &key, &loaded.verified)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    // Even a semantically equivalent replacement of the stored ref invalidates
    // the exact source token read before entering the projection transaction.
    sqlx::query("UPDATE conversation_semantic_payloads SET request_json=$1 WHERE request_id=$2")
        .bind(format!(" {reference}"))
        .bind(request.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    let mut tx = database.begin_write_transaction().await.unwrap();
    assert!(
        verify_in_transaction(&mut tx, database.backend, request, &key, &loaded.verified)
            .await
            .is_err()
    );
    tx.rollback().await.unwrap();
    sqlx::query("UPDATE conversation_semantic_payloads SET request_json=$1 WHERE request_id=$2")
        .bind(&reference)
        .bind(request.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    assert!(
        load(
            &database,
            request,
            &tenant,
            &key_id,
            &principal,
            "wrong-semantic-digest"
        )
        .await
        .is_err()
    );
    let unavailable = database
        .clone()
        .with_archive(crate::archive::ArchiveStore::training_memory_for_test());
    assert!(
        load(
            &unavailable,
            request,
            &tenant,
            &key_id,
            &principal,
            &semantic_digest
        )
        .await
        .is_err()
    );
    let retained: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conversation_semantic_payloads WHERE request_id=$1 AND format_version=2")
        .bind(request.to_string()).fetch_one(&database.pool).await.unwrap();
    assert_eq!(retained, 1);
    let pending: Option<i64> = sqlx::query_scalar(
        "SELECT projected_at FROM conversation_projection_outbox WHERE request_id=$1",
    )
    .bind(request.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(pending, None);
    database.pool.close().await;
}

#[tokio::test]
async fn sqlite_semantic_reference_external_read_and_source_token_contract() {
    let directory = tempfile::tempdir().unwrap();
    reference_contract(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("ref.db").display()
    ))
    .await;
}

#[tokio::test]
#[ignore = "requires the configured CI PostgreSQL service"]
async fn postgres_semantic_reference_external_read_and_source_token_contract() {
    let url = std::env::var("MTC_TEST_POSTGRES_URL").expect("configured PG contract fixture");
    let admin = sqlx::PgPool::connect(&url).await.unwrap();
    let schema = format!("semantic_ref_{}", Uuid::now_v7().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    let mut isolated = url::Url::parse(&url).unwrap();
    isolated
        .query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema}"));
    reference_contract(isolated.as_str()).await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}
