use super::*;

async fn recovery_contract(database_url: &str) {
    let database = Database::connect(database_url).await.unwrap();
    database.migrate().await.unwrap();
    let (key, reservation, request_id) = admitted(&database, true).await;
    let body =
        serde_json::json!({"messages": [{"role": "user", "content": "survives source deletion"}]});
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
    database
        .finish_proxy_request_deferred(input.clone())
        .await
        .unwrap();
    assert!(matches!(
        database.finish_proxy_request_deferred(input).await.unwrap(),
        FinishProxyRequestResult::AlreadyFinished { .. }
    ));
    let payload = sqlx::query("SELECT request_json, encoded_bytes, digest FROM conversation_semantic_payloads WHERE request_id = $1")
        .bind(request_id.to_string()).fetch_one(&database.pool).await.unwrap();
    let encoded: String = payload.get("request_json");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&encoded).unwrap(),
        body
    );
    assert_eq!(payload.get::<i64, _>("encoded_bytes"), encoded.len() as i64);
    assert!(encoded.len() <= 128 * 1024 * 1024);
    let atoms: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM semantic_atoms WHERE tenant_id = $1")
        .bind(key.tenant_id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap();
    assert_eq!(atoms, 0);
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
    for statement in [
        "DELETE FROM request_records WHERE id = $1",
        "DELETE FROM request_record_locators WHERE id = $1",
    ] {
        sqlx::query(statement)
            .bind(request_id.to_string())
            .execute(&database.pool)
            .await
            .unwrap();
    }
    let stale = Uuid::now_v7();
    assert!(
        database
            .claim_conversation_projection_tasks(stale, 32)
            .await
            .unwrap()
            .iter()
            .any(|task| task.request_id == request_id)
    );
    for field in ["tenant_id", "key_id", "principal_id"] {
        let original = match field {
            "tenant_id" => key.tenant_id,
            "key_id" => key.key_id,
            _ => key.principal_id,
        };
        let statement =
            format!("UPDATE conversation_semantic_payloads SET {field} = $1 WHERE request_id = $2");
        sqlx::query(sqlx::AssertSqlSafe(statement.clone()))
            .bind(Uuid::now_v7().to_string())
            .bind(request_id.to_string())
            .execute(&database.pool)
            .await
            .unwrap();
        assert!(
            database
                .project_claimed_conversation_projection_task(stale, request_id)
                .await
                .is_err()
        );
        sqlx::query(sqlx::AssertSqlSafe(statement))
            .bind(original.to_string())
            .bind(request_id.to_string())
            .execute(&database.pool)
            .await
            .unwrap();
    }
    sqlx::query("UPDATE conversation_semantic_payloads SET request_json = '{}', encoded_bytes = 2 WHERE request_id = $1")
        .bind(request_id.to_string()).execute(&database.pool).await.unwrap();
    assert!(
        database
            .project_claimed_conversation_projection_task(stale, request_id)
            .await
            .is_err()
    );
    sqlx::query("UPDATE conversation_semantic_payloads SET request_json = $1, encoded_bytes = $2 WHERE request_id = $3")
        .bind(&encoded).bind(encoded.len() as i64).bind(request_id.to_string()).execute(&database.pool).await.unwrap();
    let atoms: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM semantic_atoms WHERE tenant_id = $1")
        .bind(key.tenant_id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap();
    assert_eq!(atoms, 0);
    let reject_ack = match database.backend {
        DatabaseBackend::Sqlite => {
            "CREATE TRIGGER reject_semantic_ack BEFORE UPDATE OF projected_at ON conversation_projection_outbox BEGIN SELECT RAISE(ABORT, 'injected semantic ack failure'); END;"
        }
        DatabaseBackend::PostgreSql => {
            "CREATE FUNCTION reject_semantic_ack() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected semantic ack failure'; END $$; CREATE TRIGGER reject_semantic_ack BEFORE UPDATE OF projected_at ON conversation_projection_outbox FOR EACH ROW EXECUTE FUNCTION reject_semantic_ack();"
        }
    };
    sqlx::raw_sql(reject_ack)
        .execute(&database.pool)
        .await
        .unwrap();
    assert!(
        database
            .project_claimed_conversation_projection_task(stale, request_id)
            .await
            .is_err()
    );
    let state = sqlx::query("SELECT (SELECT COUNT(*) FROM semantic_atoms WHERE tenant_id = $1) AS atoms, (SELECT COUNT(*) FROM conversation_observations WHERE request_id = $2) AS observations, (SELECT COUNT(*) FROM conversation_semantic_payloads WHERE request_id = $2) AS payloads")
        .bind(key.tenant_id.to_string()).bind(request_id.to_string()).fetch_one(&database.pool).await.unwrap();
    assert!(state.get::<i64, _>("atoms") > 0);
    assert_eq!(state.get::<i64, _>("observations"), 0);
    assert_eq!(state.get::<i64, _>("payloads"), 1);
    let drop_ack = match database.backend {
        DatabaseBackend::Sqlite => "DROP TRIGGER reject_semantic_ack",
        DatabaseBackend::PostgreSql => {
            "DROP TRIGGER reject_semantic_ack ON conversation_projection_outbox; DROP FUNCTION reject_semantic_ack();"
        }
    };
    sqlx::raw_sql(drop_ack)
        .execute(&database.pool)
        .await
        .unwrap();
    database.pool.close().await;
    let database = Database::connect(database_url).await.unwrap();
    sqlx::query("UPDATE conversation_projection_outbox SET terminal_lease_expires_at = 0 WHERE request_id = $1")
        .bind(request_id.to_string()).execute(&database.pool).await.unwrap();
    let owner = Uuid::now_v7();
    assert!(
        database
            .claim_conversation_projection_tasks(owner, 32)
            .await
            .unwrap()
            .iter()
            .any(|task| task.request_id == request_id)
    );
    assert!(
        !database
            .project_claimed_conversation_projection_task(stale, request_id)
            .await
            .unwrap()
    );
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
    assert!(matches!(
        database
            .finish_proxy_request_deferred(finish(&key, &reservation, request_id))
            .await
            .unwrap(),
        FinishProxyRequestResult::AlreadyFinished {
            cost_micros: 10,
            ..
        }
    ));
    assert_counts(&database, request_id, reservation.id, 1).await;
    let payloads: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM conversation_semantic_payloads WHERE request_id = $1",
    )
    .bind(request_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(payloads, 0);
    let lifetime: i64 = sqlx::query_scalar(
        "SELECT settled_lifetime_micros FROM account_usage_state WHERE account_id = $1",
    )
    .bind(key.account_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(lifetime, 10);
    database.pool.close().await;
}

#[tokio::test]
async fn sqlite_semantic_payload_recovers_without_source_or_duplicate_finance() {
    let directory = tempfile::tempdir().unwrap();
    recovery_contract(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("semantic.db").display()
    ))
    .await;
}

#[tokio::test]
#[ignore = "requires the configured CI PostgreSQL service"]
async fn postgres_semantic_payload_recovers_without_source_or_duplicate_finance() {
    let database_url = std::env::var("MTC_TEST_POSTGRES_URL")
        .expect("PostgreSQL contract requires MTC_TEST_POSTGRES_URL");
    let admin = sqlx::PgPool::connect(&database_url).await.unwrap();
    let schema = format!("semantic_recovery_{}", Uuid::now_v7().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    let mut isolated = url::Url::parse(&database_url).unwrap();
    isolated
        .query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema}"));
    recovery_contract(isolated.as_str()).await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}
