use super::*;

async fn pending(
    database: &Database,
    seed_legacy_payload: bool,
) -> (AuthenticatedKey, UsageReservation, Uuid, Uuid) {
    let (key, reservation, request_id) = admitted(database, false).await;
    let body =
        serde_json::json!({"messages": [{"role": "user", "content": request_id.to_string()}]});
    let hints = ConversationHints {
        session_id: Some(request_id.to_string()),
        ..ConversationHints::default()
    };
    let mut input = finish(&key, &reservation, request_id);
    input.conversation = Some(ProxyConversationInput {
        key: &key,
        request_json: &body,
        hints: &hints,
        client_name: None,
        upstream_response_id: None,
    });
    database.finish_proxy_request_deferred(input).await.unwrap();
    if seed_legacy_payload {
        let encoded = body.to_string();
        let mut digest = blake3::Hasher::new_derive_key("MTC conversation semantic payload v1");
        for value in [
            request_id.to_string(),
            key.tenant_id.to_string(),
            key.key_id.to_string(),
            key.principal_id.to_string(),
            encoded.clone(),
        ] {
            digest.update(&(value.len() as u64).to_le_bytes());
            digest.update(value.as_bytes());
        }
        sqlx::query("INSERT INTO conversation_semantic_payloads (request_id, tenant_id, key_id, principal_id, format_version, encoded_bytes, digest, request_json) VALUES ($1, $2, $3, $4, 1, $5, $6, $7)")
            .bind(request_id.to_string())
            .bind(key.tenant_id.to_string())
            .bind(key.key_id.to_string())
            .bind(key.principal_id.to_string())
            .bind(encoded.len() as i64)
            .bind(digest.finalize().to_hex().to_string())
            .bind(encoded)
            .execute(&database.pool)
            .await
            .unwrap();
    }
    let terminal_owner = Uuid::now_v7();
    assert!(
        database
            .claim_terminal_projection_tasks(terminal_owner, 32)
            .await
            .unwrap()
            .contains(&request_id)
    );
    assert!(
        database
            .project_claimed_terminal_projection_task(terminal_owner, request_id)
            .await
            .unwrap()
    );
    let owner = Uuid::now_v7();
    assert!(
        database
            .claim_conversation_projection_tasks(owner, 32)
            .await
            .unwrap()
            .iter()
            .any(|task| task.request_id == request_id)
    );
    (key, reservation, request_id, owner)
}

async fn expiry(database: &Database, request_id: Uuid) -> i64 {
    sqlx::query_scalar("SELECT terminal_lease_expires_at FROM conversation_projection_outbox WHERE request_id = $1")
        .bind(request_id.to_string()).fetch_one(&database.pool).await.unwrap()
}

async fn retained(
    database: &Database,
    request_id: Uuid,
    expected_expiry: i64,
    expected_payloads: i64,
) {
    let row = sqlx::query("SELECT lease_owner, lease_expires_at, terminal_lease_expires_at, projected_at, (SELECT COUNT(*) FROM conversation_semantic_payloads WHERE request_id = $1) AS payloads, (SELECT COUNT(*) FROM conversation_observations WHERE request_id = $1) AS observations FROM conversation_projection_outbox WHERE request_id = $1")
        .bind(request_id.to_string()).fetch_one(&database.pool).await.unwrap();
    assert_eq!(row.get::<String, _>("lease_owner"), "terminal-v118");
    assert_eq!(row.get::<i64, _>("lease_expires_at"), i64::MAX);
    assert_eq!(
        row.get::<i64, _>("terminal_lease_expires_at"),
        expected_expiry
    );
    assert_eq!(row.get::<Option<i64>, _>("projected_at"), None);
    assert_eq!(row.get::<i64, _>("payloads"), expected_payloads);
    assert_eq!(row.get::<i64, _>("observations"), 0);
}

async fn complete(
    database: &Database,
    request_id: Uuid,
    reservation: &UsageReservation,
    owner: Uuid,
) {
    assert!(
        database
            .project_claimed_conversation_projection_task(owner, request_id)
            .await
            .unwrap()
    );
    assert!(
        !database
            .project_claimed_conversation_projection_task(owner, request_id)
            .await
            .unwrap()
    );
    assert!(
        !database
            .schedule_rolled_back_conversation_retry(request_id, owner)
            .await
            .unwrap()
    );
    assert_counts(database, request_id, reservation.id, 1).await;
    let row = sqlx::query("SELECT (SELECT COUNT(*) FROM conversation_semantic_payloads WHERE request_id = $1) AS payloads, (SELECT COUNT(*) FROM conversation_observations WHERE request_id = $1) AS observations, (SELECT COUNT(*) FROM request_events WHERE request_id = $1 AND event_kind = 'projected') AS events")
        .bind(request_id.to_string()).fetch_one(&database.pool).await.unwrap();
    assert_eq!(row.get::<i64, _>("payloads"), 0);
    assert_eq!(row.get::<i64, _>("observations"), 1);
    assert_eq!(row.get::<i64, _>("events"), 1);
}

async fn retry_contract(database_url: &str) {
    let database = Database::connect_with_max(database_url, 1).await.unwrap();
    database.migrate().await.unwrap();
    let blockers = sqlx::PgPool::connect(database_url).await.unwrap();
    let (_key, reservation, request_id, owner) = pending(&database, true).await;
    let original_expiry = expiry(&database, request_id).await;
    assert!(
        !database
            .schedule_rolled_back_conversation_retry(request_id, Uuid::now_v7())
            .await
            .unwrap()
    );
    retained(&database, request_id, original_expiry, 1).await;
    let legacy_owner = Uuid::now_v7().to_string();
    sqlx::query("UPDATE conversation_projection_outbox SET lease_owner = $1 WHERE request_id = $2")
        .bind(&legacy_owner)
        .bind(request_id.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    assert!(
        !database
            .schedule_rolled_back_conversation_retry(request_id, owner)
            .await
            .unwrap()
    );
    assert_eq!(expiry(&database, request_id).await, original_expiry);
    let stored_owner: String = sqlx::query_scalar(
        "SELECT lease_owner FROM conversation_projection_outbox WHERE request_id = $1",
    )
    .bind(request_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(stored_owner, legacy_owner);
    sqlx::query("UPDATE conversation_projection_outbox SET lease_owner = 'terminal-v118' WHERE request_id = $1")
        .bind(request_id.to_string()).execute(&database.pool).await.unwrap();
    sqlx::query("SET lock_timeout = '100ms'")
        .execute(&database.pool)
        .await
        .unwrap();
    let mut blocker = blockers.begin().await.unwrap();
    sqlx::query("LOCK TABLE semantic_atoms, context_nodes IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *blocker)
        .await
        .unwrap();
    assert!(
        database
            .project_claimed_conversation_projection_task(owner, request_id)
            .await
            .is_err()
    );
    let retry_expiry = expiry(&database, request_id).await;
    assert!(retry_expiry < original_expiry);
    assert!(retry_expiry > unix_millis() && retry_expiry <= unix_millis() + 1000);
    retained(&database, request_id, retry_expiry, 1).await;
    assert!(
        database
            .claim_conversation_projection_tasks(Uuid::now_v7(), 32)
            .await
            .unwrap()
            .is_empty()
    );
    blocker.rollback().await.unwrap();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let next_owner = Uuid::now_v7();
    assert!(
        database
            .claim_conversation_projection_tasks(next_owner, 32)
            .await
            .unwrap()
            .iter()
            .any(|task| task.request_id == request_id)
    );
    let new_expiry = expiry(&database, request_id).await;
    assert!(
        !database
            .schedule_rolled_back_conversation_retry(request_id, owner)
            .await
            .unwrap()
    );
    retained(&database, request_id, new_expiry, 1).await;
    complete(&database, request_id, &reservation, next_owner).await;

    let (_key, reservation, request_id, owner) = pending(&database, false).await;
    let original_expiry = expiry(&database, request_id).await;
    let terminal_projected_at: i64 = sqlx::query_scalar(
        "SELECT projected_at FROM terminal_projection_outbox WHERE request_id = $1",
    )
    .bind(request_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    sqlx::query("UPDATE terminal_projection_outbox SET projected_at = NULL WHERE request_id = $1")
        .bind(request_id.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    sqlx::raw_sql("CREATE FUNCTION reject_retry() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.terminal_lease_expires_at IS NOT NULL AND NEW.terminal_lease_expires_at > OLD.terminal_lease_expires_at THEN RAISE EXCEPTION USING ERRCODE = '23514', MESSAGE = 'injected scheduling failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER reject_retry BEFORE UPDATE OF terminal_lease_expires_at ON conversation_projection_outbox FOR EACH ROW EXECUTE FUNCTION reject_retry();")
        .execute(&blockers).await.unwrap();
    assert!(
        !database
            .project_claimed_conversation_projection_task(owner, request_id)
            .await
            .unwrap()
    );
    retained(&database, request_id, original_expiry, 0).await;
    sqlx::raw_sql("DROP TRIGGER reject_retry ON conversation_projection_outbox; DROP FUNCTION reject_retry();")
        .execute(&blockers).await.unwrap();
    assert!(
        !database
            .project_claimed_conversation_projection_task(owner, request_id)
            .await
            .unwrap()
    );
    let retry_expiry = expiry(&database, request_id).await;
    assert!(retry_expiry < original_expiry);
    retained(&database, request_id, retry_expiry, 0).await;
    sqlx::query("UPDATE terminal_projection_outbox SET projected_at = $1 WHERE request_id = $2")
        .bind(terminal_projected_at)
        .bind(request_id.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    complete(&database, request_id, &reservation, owner).await;

    for (name, trigger) in [
        (
            "permanent",
            "CREATE FUNCTION reject_content() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION USING ERRCODE = '23514', MESSAGE = 'injected permanent error'; END $$; CREATE TRIGGER reject_content BEFORE INSERT ON semantic_atoms FOR EACH ROW EXECUTE FUNCTION reject_content();",
        ),
        (
            "commit",
            "CREATE FUNCTION reject_content() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION USING ERRCODE = '40001', MESSAGE = 'injected commit failure'; END $$; CREATE CONSTRAINT TRIGGER reject_content AFTER INSERT ON semantic_atoms DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION reject_content();",
        ),
        (
            "connection",
            "CREATE FUNCTION reject_content() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_terminate_backend(pg_backend_pid()); RETURN NEW; END $$; CREATE TRIGGER reject_content BEFORE INSERT ON semantic_atoms FOR EACH ROW EXECUTE FUNCTION reject_content();",
        ),
    ] {
        let (_key, reservation, request_id, owner) = pending(&database, true).await;
        let original_expiry = expiry(&database, request_id).await;
        sqlx::raw_sql(trigger).execute(&blockers).await.unwrap();
        assert!(
            database
                .project_claimed_conversation_projection_task(owner, request_id)
                .await
                .is_err(),
            "{name}"
        );
        retained(&database, request_id, original_expiry, 1).await;
        assert!(
            database
                .claim_conversation_projection_tasks(Uuid::now_v7(), 32)
                .await
                .unwrap()
                .is_empty(),
            "{name}"
        );
        sqlx::raw_sql(
            "DROP TRIGGER reject_content ON semantic_atoms; DROP FUNCTION reject_content();",
        )
        .execute(&blockers)
        .await
        .unwrap();
        complete(&database, request_id, &reservation, owner).await;
    }
    let (_key, reservation, request_id, owner) = pending(&database, true).await;
    let original_expiry = expiry(&database, request_id).await;
    sqlx::raw_sql("CREATE FUNCTION reject_observation() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION USING ERRCODE = '55P03', MESSAGE = 'injected retryable observation statement'; END $$; CREATE TRIGGER reject_observation BEFORE INSERT ON conversation_observations FOR EACH ROW EXECUTE FUNCTION reject_observation();")
        .execute(&blockers).await.unwrap();
    assert!(
        database
            .project_claimed_conversation_projection_task(owner, request_id)
            .await
            .is_err()
    );
    let retry_expiry = expiry(&database, request_id).await;
    assert!(retry_expiry < original_expiry);
    assert!(retry_expiry > unix_millis() && retry_expiry <= unix_millis() + 1000);
    retained(&database, request_id, retry_expiry, 1).await;
    sqlx::raw_sql("DROP TRIGGER reject_observation ON conversation_observations; DROP FUNCTION reject_observation();")
        .execute(&blockers).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let next_owner = Uuid::now_v7();
    assert!(
        database
            .claim_conversation_projection_tasks(next_owner, 32)
            .await
            .unwrap()
            .iter()
            .any(|task| task.request_id == request_id)
    );
    complete(&database, request_id, &reservation, next_owner).await;

    let (key, reservation, request_id, owner) = pending(&database, true).await;
    let original_expiry = expiry(&database, request_id).await;
    sqlx::raw_sql("CREATE FUNCTION reject_ack() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION USING ERRCODE = '55P03', MESSAGE = 'injected retryable ACK statement'; END $$; CREATE TRIGGER reject_ack BEFORE UPDATE OF projected_at ON conversation_projection_outbox FOR EACH ROW EXECUTE FUNCTION reject_ack();")
        .execute(&blockers).await.unwrap();
    assert!(
        database
            .project_claimed_conversation_projection_task(owner, request_id)
            .await
            .is_err()
    );
    let retry_expiry = expiry(&database, request_id).await;
    assert!(retry_expiry < original_expiry);
    retained(&database, request_id, retry_expiry, 1).await;
    let atoms: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM semantic_atoms WHERE tenant_id = $1")
        .bind(key.tenant_id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap();
    assert!(atoms > 0);
    sqlx::raw_sql(
        "DROP TRIGGER reject_ack ON conversation_projection_outbox; DROP FUNCTION reject_ack();",
    )
    .execute(&blockers)
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let next_owner = Uuid::now_v7();
    assert!(
        database
            .claim_conversation_projection_tasks(next_owner, 32)
            .await
            .unwrap()
            .iter()
            .any(|task| task.request_id == request_id)
    );
    complete(&database, request_id, &reservation, next_owner).await;

    let (_key, reservation, request_id, owner) = pending(&database, true).await;
    let original_expiry = expiry(&database, request_id).await;
    sqlx::raw_sql("CREATE FUNCTION reject_ack() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION USING ERRCODE = '40001', MESSAGE = 'injected final commit failure'; END $$; CREATE CONSTRAINT TRIGGER reject_ack AFTER UPDATE ON conversation_projection_outbox DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION reject_ack();")
        .execute(&blockers).await.unwrap();
    assert!(
        database
            .project_claimed_conversation_projection_task(owner, request_id)
            .await
            .is_err()
    );
    retained(&database, request_id, original_expiry, 1).await;
    assert!(
        database
            .claim_conversation_projection_tasks(Uuid::now_v7(), 32)
            .await
            .unwrap()
            .is_empty()
    );
    sqlx::raw_sql(
        "DROP TRIGGER reject_ack ON conversation_projection_outbox; DROP FUNCTION reject_ack();",
    )
    .execute(&blockers)
    .await
    .unwrap();
    complete(&database, request_id, &reservation, owner).await;

    let (_key, reservation, request_id, owner) = pending(&database, true).await;
    let original_expiry = expiry(&database, request_id).await;
    sqlx::query("SET lock_timeout = '5s'")
        .execute(&database.pool)
        .await
        .unwrap();
    let backend: i64 = sqlx::query_scalar("SELECT CAST(pg_backend_pid() AS BIGINT)")
        .fetch_one(&database.pool)
        .await
        .unwrap();
    let mut blocker = blockers.begin().await.unwrap();
    sqlx::query("LOCK TABLE semantic_atoms IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *blocker)
        .await
        .unwrap();
    let projecting = database.clone();
    let operation = tokio::spawn(async move {
        projecting
            .project_claimed_conversation_projection_task(owner, request_id)
            .await
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_locks WHERE pid = $1 AND relation = 'semantic_atoms'::regclass AND NOT granted)")
            .bind(backend as i32).fetch_one(&blockers).await.unwrap();
        if waiting {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "projection did not enter content lock"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    operation.abort();
    assert!(operation.await.unwrap_err().is_cancelled());
    blocker.rollback().await.unwrap();
    retained(&database, request_id, original_expiry, 1).await;
    assert!(
        database
            .claim_conversation_projection_tasks(Uuid::now_v7(), 32)
            .await
            .unwrap()
            .is_empty()
    );
    complete(&database, request_id, &reservation, owner).await;
    blockers.close().await;
    database.pool.close().await;
}

#[tokio::test]
#[ignore = "requires the configured CI PostgreSQL service"]
async fn postgres_semantic_projection_retries_only_confirmed_statement_rollback() {
    let database_url = std::env::var("MTC_TEST_POSTGRES_URL")
        .expect("PostgreSQL contract requires MTC_TEST_POSTGRES_URL");
    let admin = sqlx::PgPool::connect(&database_url).await.unwrap();
    let schema = format!("semantic_retry_{}", Uuid::now_v7().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    let mut isolated = url::Url::parse(&database_url).unwrap();
    isolated
        .query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema}"));
    retry_contract(isolated.as_str()).await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}
