use super::super::*;
use crate::conversation::ConversationHints;
use std::time::Duration;

async fn admitted(
    database: &Database,
    metered: bool,
) -> (AuthenticatedKey, UsageReservation, Uuid) {
    let unique = Uuid::now_v7().to_string();
    let pepper = b"terminal projection acceptance test pepper";
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: unique.clone(),
                principal_external_id: "member".into(),
                alias: unique.clone(),
                currency: "USD".into(),
                policy: KeyPolicy {
                    enforcement_mode: if metered {
                        EnforcementMode::MeteredUnlimited
                    } else {
                        EnforcementMode::Prepaid
                    },
                    ..KeyPolicy::default()
                },
                initial_balance: Decimal::TEN,
                idempotency_key: None,
            },
            pepper,
        )
        .await
        .unwrap();
    let key = database
        .authenticate_key(&issued.key, pepper)
        .await
        .unwrap();
    let price = database
        .upsert_model_price(&unique, "USD", Decimal::ONE, Decimal::ONE)
        .await
        .unwrap();
    let request_id = Uuid::now_v7();
    let reservation = database
        .start_proxy_request(StartProxyRequest {
            request_id,
            key: &key,
            price: &price,
            input_token_ceiling: 10,
            output_token_ceiling: 10,
            protocol: "openai",
            model: &unique,
            request_object: "gap://terminal-projection/request",
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap();
    (key, reservation, request_id)
}

fn finish<'a>(
    key: &'a AuthenticatedKey,
    reservation: &'a UsageReservation,
    request_id: Uuid,
) -> FinishProxyRequest<'a> {
    FinishProxyRequest {
        terminal_cause: None,
        usage_basis: Some(crate::model::RequestUsageBasis::ProviderReported),
        first_output_ms: Some(1),
        generation_duration_ms: None,
        request_id,
        tenant_id: key.tenant_id,
        reservation,
        input_token_ceiling: 10,
        output_token_ceiling: 10,
        requested_service_tier: None,
        status_code: 200,
        duration_ms: 10,
        usage: TokenUsage {
            input_tokens: 7,
            output_tokens: 3,
            ..TokenUsage::default()
        },
        error_code: None,
        response_object: "gap://terminal-projection/response",
        routing_session_id: None,
        routing_terminal_observed_at: None,
        conversation: None,
    }
}

async fn assert_counts(database: &Database, request_id: Uuid, reservation_id: Uuid, facts: i64) {
    let row = sqlx::query("SELECT (SELECT COUNT(*) FROM ledger_entries WHERE source = $1 AND kind = 'usage') AS ledger, (SELECT COUNT(*) FROM request_stats_facts WHERE request_id = $2) AS facts, (SELECT COUNT(*) FROM request_events WHERE request_id = $2 AND event_kind = 'finished') AS events")
        .bind(reservation_id.to_string()).bind(request_id.to_string()).fetch_one(&database.pool).await.unwrap();
    assert_eq!(row.get::<i64, _>("ledger"), 1);
    assert_eq!(row.get::<i64, _>("facts"), facts);
    assert_eq!(row.get::<i64, _>("events"), 1);
}

async fn replay_after_source_deletion(database: &Database, metered: bool, account_first: bool) {
    let (key, reservation, request_id) = admitted(database, metered).await;
    let body =
        serde_json::json!({"messages": [{"role": "user", "content": "durable conversation"}]});
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
    assert!(matches!(
        database
            .finish_proxy_request_deferred(input.clone())
            .await
            .unwrap(),
        FinishProxyRequestResult::Finished { .. }
    ));
    assert!(matches!(
        database.finish_proxy_request_deferred(input).await.unwrap(),
        FinishProxyRequestResult::AlreadyFinished { .. }
    ));
    let account_owner = Uuid::now_v7();
    let account_tasks = database
        .claim_metered_usage_projection_tasks(account_owner, 32)
        .await
        .unwrap();
    assert!(
        !account_tasks
            .iter()
            .any(|task| task.reservation_id == reservation.id)
    );
    let envelope = sqlx::query("SELECT request_json, semantic_snapshot_json, lease_owner FROM conversation_projection_outbox WHERE request_id = $1")
        .bind(request_id.to_string()).fetch_one(&database.pool).await.unwrap();
    assert_eq!(envelope.get::<String, _>("request_json"), "{}");
    assert!(envelope.get::<String, _>("semantic_snapshot_json").len() < 71_000);
    assert_eq!(envelope.get::<String, _>("lease_owner"), "terminal-v118");
    if account_first {
        for task in &account_tasks {
            assert!(
                database
                    .project_claimed_metered_usage_projection_task(
                        account_owner,
                        task.reservation_id
                    )
                    .await
                    .unwrap()
            );
        }
    }
    let conversation_owner = Uuid::now_v7();
    let conversations = database
        .claim_conversation_projection_tasks(conversation_owner, 32)
        .await
        .unwrap();
    assert!(
        conversations
            .iter()
            .any(|task| task.request_id == request_id)
    );
    assert!(
        !database
            .project_claimed_conversation_projection_task(conversation_owner, request_id)
            .await
            .unwrap()
    );
    sqlx::query("DELETE FROM request_records WHERE id = $1")
        .bind(request_id.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM request_record_locators WHERE id = $1")
        .bind(request_id.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
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
    let stale = Uuid::now_v7();
    assert!(
        database
            .claim_terminal_projection_tasks(stale, 32)
            .await
            .unwrap()
            .contains(&request_id)
    );
    sqlx::query("UPDATE terminal_projection_outbox SET lease_expires_at = 0 WHERE request_id = $1")
        .bind(request_id.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    let owner = Uuid::now_v7();
    assert!(
        database
            .claim_terminal_projection_tasks(owner, 32)
            .await
            .unwrap()
            .contains(&request_id)
    );
    assert!(
        !database
            .project_claimed_terminal_projection_task(stale, request_id)
            .await
            .unwrap()
    );
    assert!(
        database
            .project_claimed_terminal_projection_task(owner, request_id)
            .await
            .unwrap()
    );
    assert!(
        !database
            .project_claimed_terminal_projection_task(owner, request_id)
            .await
            .unwrap()
    );
    assert!(
        database
            .project_claimed_conversation_projection_task(conversation_owner, request_id)
            .await
            .unwrap()
    );
    assert!(
        !database
            .project_claimed_conversation_projection_task(conversation_owner, request_id)
            .await
            .unwrap()
    );
    if !account_first {
        for task in &account_tasks {
            assert!(
                database
                    .project_claimed_metered_usage_projection_task(
                        account_owner,
                        task.reservation_id
                    )
                    .await
                    .unwrap()
            );
        }
    }
    assert_counts(database, request_id, reservation.id, 1).await;
    let aggregate: i64 = sqlx::query_scalar(
        "SELECT CAST(SUM(requests) AS BIGINT) FROM request_daily_aggregates WHERE key_id = $1",
    )
    .bind(key.key_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(aggregate, 1);
    let sessions: i64 = sqlx::query_scalar(
        "SELECT CAST(SUM(requests) AS BIGINT) FROM session_usage_totals WHERE key_id = $1",
    )
    .bind(key.key_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(sessions, 1);
    let lifetime: i64 = sqlx::query_scalar(
        "SELECT settled_lifetime_micros FROM account_usage_state WHERE account_id = $1",
    )
    .bind(key.account_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(lifetime, 10);
}

#[tokio::test]
async fn sqlite_terminal_replay_survives_source_deletion_and_consumer_order() {
    let directory = tempfile::tempdir().unwrap();
    let database = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("terminal.db").display()
    ))
    .await
    .unwrap();
    database.migrate().await.unwrap();
    for metered in [false, true] {
        for account_first in [false, true] {
            replay_after_source_deletion(&database, metered, account_first).await;
        }
    }
}

#[tokio::test]
async fn sqlite_terminal_apply_and_ack_rollback_together() {
    let directory = tempfile::tempdir().unwrap();
    let database = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("rollback.db").display()
    ))
    .await
    .unwrap();
    database.migrate().await.unwrap();
    let (key, reservation, request_id) = admitted(&database, true).await;
    database
        .finish_proxy_request_deferred(finish(&key, &reservation, request_id))
        .await
        .unwrap();
    let owner = Uuid::now_v7();
    database
        .claim_terminal_projection_tasks(owner, 32)
        .await
        .unwrap();
    sqlx::raw_sql("CREATE TRIGGER reject_terminal_ack BEFORE UPDATE OF projected_at ON terminal_projection_outbox BEGIN SELECT RAISE(ABORT, 'injected ack failure'); END;").execute(&database.pool).await.unwrap();
    assert!(
        database
            .project_claimed_terminal_projection_task(owner, request_id)
            .await
            .is_err()
    );
    let facts: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM request_stats_facts WHERE request_id = $1")
            .bind(request_id.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(facts, 0);
    let receipt: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM terminal_projection_outbox WHERE request_id = $1 AND projected_at IS NULL AND account_projected_at IS NOT NULL")
        .bind(request_id.to_string()).fetch_one(&database.pool).await.unwrap();
    assert_eq!(receipt, 1);
    let lifetime: i64 = sqlx::query_scalar(
        "SELECT settled_lifetime_micros FROM account_usage_state WHERE account_id = $1",
    )
    .bind(key.account_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(lifetime, 10);
    sqlx::query("DROP TRIGGER reject_terminal_ack")
        .execute(&database.pool)
        .await
        .unwrap();
    assert!(
        database
            .project_claimed_terminal_projection_task(owner, request_id)
            .await
            .unwrap()
    );
    assert_counts(&database, request_id, reservation.id, 1).await;
}

#[tokio::test]
async fn sqlite_terminal_snapshot_failure_rolls_back_financial_winner() {
    let directory = tempfile::tempdir().unwrap();
    let database = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("enqueue.db").display()
    ))
    .await
    .unwrap();
    database.migrate().await.unwrap();
    let (key, reservation, request_id) = admitted(&database, false).await;
    sqlx::raw_sql("CREATE TRIGGER reject_terminal_enqueue BEFORE INSERT ON terminal_projection_outbox BEGIN SELECT RAISE(ABORT, 'injected enqueue failure'); END;").execute(&database.pool).await.unwrap();
    assert!(
        database
            .finish_proxy_request_deferred(finish(&key, &reservation, request_id))
            .await
            .is_err()
    );
    let state = sqlx::query("SELECT (SELECT COUNT(*) FROM ledger_entries WHERE source = $1 AND kind = 'usage') AS charges, (SELECT status FROM usage_reservations WHERE id = $1) AS status, (SELECT completed_at FROM request_records WHERE id = $2) AS completed_at")
        .bind(reservation.id.to_string()).bind(request_id.to_string()).fetch_one(&database.pool).await.unwrap();
    assert_eq!(state.get::<i64, _>("charges"), 0);
    assert_eq!(state.get::<String, _>("status"), "reserved");
    assert_eq!(state.get::<Option<i64>, _>("completed_at"), None);
    sqlx::query("DROP TRIGGER reject_terminal_enqueue")
        .execute(&database.pool)
        .await
        .unwrap();
    database
        .finish_proxy_request_deferred(finish(&key, &reservation, request_id))
        .await
        .unwrap();
    let owner = Uuid::now_v7();
    database
        .claim_terminal_projection_tasks(owner, 32)
        .await
        .unwrap();
    database
        .project_claimed_terminal_projection_task(owner, request_id)
        .await
        .unwrap();
    assert_counts(&database, request_id, reservation.id, 1).await;
}

#[tokio::test]
async fn postgres_terminal_financial_commit_does_not_wait_for_projection_or_cursor_locks() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let admin = sqlx::PgPool::connect(&database_url).await.unwrap();
    let schema = format!("terminal_projection_{}", Uuid::now_v7().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    let mut isolated = url::Url::parse(&database_url).unwrap();
    isolated
        .query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema}"));
    let database = Database::connect_with_max(isolated.as_str(), 8)
        .await
        .unwrap();
    database.migrate().await.unwrap();
    for metered in [false, true] {
        for lock in [
            "SELECT pg_advisory_xact_lock(hashtextextended('memeloop-token-center:request-stats', 734627102948314))",
            "SELECT pg_advisory_xact_lock(1297367877, 1)",
        ] {
            let (key, reservation, request_id) = admitted(&database, metered).await;
            let mut gate = database.begin_write_transaction().await.unwrap();
            sqlx::query(lock).execute(&mut *gate).await.unwrap();
            tokio::time::timeout(
                Duration::from_secs(5),
                database.finish_proxy_request_deferred(finish(&key, &reservation, request_id)),
            )
            .await
            .expect("financial commit must not wait on derived locks")
            .unwrap();
            let pending: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM terminal_projection_outbox WHERE request_id = $1 AND projected_at IS NULL").bind(request_id.to_string()).fetch_one(&database.pool).await.unwrap();
            assert_eq!(pending, 1);
            gate.commit().await.unwrap();
            let owner = Uuid::now_v7();
            database
                .claim_terminal_projection_tasks(owner, 32)
                .await
                .unwrap();
            assert!(
                database
                    .project_claimed_terminal_projection_task(owner, request_id)
                    .await
                    .unwrap()
            );
            assert_counts(&database, request_id, reservation.id, 1).await;
            database
                .publish_proxy_started_event(ArchiveSpoolIdentity {
                    request_id,
                    tenant_id: key.tenant_id,
                    reservation_id: reservation.id,
                })
                .await
                .unwrap();
            let inversion: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_events started JOIN request_events finished ON started.request_id = finished.request_id WHERE started.request_id = $1 AND started.event_kind = 'started' AND finished.event_kind = 'finished' AND (started.event_at > finished.event_at OR (started.event_at = finished.event_at AND started.event_id > finished.event_id))")
                .bind(request_id.to_string()).fetch_one(&database.pool).await.unwrap();
            assert_eq!(inversion, 0);
        }
    }
    replay_after_source_deletion(&database, true, false).await;
    database.pool.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
}

fn prune_sql() -> String {
    let source = include_str!("../../../scripts/maintenance/reconcile-postgres-request-stats.ts");
    let (_, function) = source.split_once("function pruneApply").unwrap();
    let (_, input) = function.split_once("input: `").unwrap();
    let (statement, _) = input.split_once("` });").unwrap();
    statement.replace(":'cutoff'", "'2021-01-01'")
}

async fn maintenance(database: &Database, statement: String) -> Result<(), sqlx::Error> {
    let mut connection = database.pool.acquire().await?;
    let result = sqlx::raw_sql(sqlx::AssertSqlSafe(statement))
        .execute(&mut *connection)
        .await;
    if result.is_err() {
        sqlx::query("ROLLBACK").execute(&mut *connection).await?;
    }
    result.map(|_| ())
}

async fn historical(database: &Database) -> (AuthenticatedKey, UsageReservation, Uuid) {
    let (key, reservation, request_id) = admitted(database, true).await;
    for statement in [
        "UPDATE request_records SET created_at = 1577836800000 WHERE id = $1",
        "UPDATE request_record_locators SET created_at = 1577836800000 WHERE id = $1",
    ] {
        sqlx::query(statement)
            .bind(request_id.to_string())
            .execute(&database.pool)
            .await
            .unwrap();
    }
    let body =
        serde_json::json!({"messages": [{"role": "user", "content": "retained semantic content"}]});
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
    sqlx::query("DELETE FROM request_records WHERE id = $1")
        .bind(request_id.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    (key, reservation, request_id)
}

#[tokio::test]
async fn postgres_terminal_prune_rebuild_and_delayed_commit_share_durable_boundary() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let admin = sqlx::PgPool::connect(&database_url).await.unwrap();
    let schema = format!("terminal_retention_{}", Uuid::now_v7().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    let mut isolated = url::Url::parse(&database_url).unwrap();
    isolated
        .query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema}"));
    let database = Database::connect_with_max(isolated.as_str(), 8)
        .await
        .unwrap();
    database.migrate().await.unwrap();
    let (key, reservation, request_id) = historical(&database).await;
    assert!(maintenance(&database, prune_sql()).await.is_err());
    let rebuild = include_str!("../../../scripts/maintenance/reconcile-observability-day.sql")
        .replace(":'day'", "'2020-01-01'");
    maintenance(&database, rebuild.clone()).await.unwrap();
    let owner = Uuid::now_v7();
    assert_eq!(
        database
            .claim_terminal_projection_tasks(owner, 32)
            .await
            .unwrap(),
        vec![request_id]
    );
    assert!(
        database
            .project_claimed_terminal_projection_task(owner, request_id)
            .await
            .unwrap()
    );
    maintenance(&database, rebuild.clone()).await.unwrap();
    assert_counts(&database, request_id, reservation.id, 1).await;
    let requests: i64 = sqlx::query_scalar(
        "SELECT CAST(SUM(requests) AS BIGINT) FROM request_daily_aggregates WHERE key_id = $1",
    )
    .bind(key.key_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(requests, 1);
    assert!(maintenance(&database, prune_sql()).await.is_err());
    let account_owner = Uuid::now_v7();
    let tasks = database
        .claim_metered_usage_projection_tasks(account_owner, 32)
        .await
        .unwrap();
    assert!(tasks.is_empty());
    let conversation_owner = Uuid::now_v7();
    assert_eq!(
        database
            .claim_conversation_projection_tasks(conversation_owner, 32)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        database
            .project_claimed_conversation_projection_task(conversation_owner, request_id)
            .await
            .unwrap()
    );
    maintenance(&database, prune_sql()).await.unwrap();
    assert_counts(&database, request_id, reservation.id, 0).await;
    assert!(
        !database
            .project_claimed_terminal_projection_task(owner, request_id)
            .await
            .unwrap()
    );
    assert!(maintenance(&database, rebuild).await.is_err());
    let (late_key, late_reservation, late_id) = historical(&database).await;
    let late_owner = Uuid::now_v7();
    assert_eq!(
        database
            .claim_terminal_projection_tasks(late_owner, 32)
            .await
            .unwrap(),
        vec![late_id]
    );
    assert!(
        database
            .project_claimed_terminal_projection_task(late_owner, late_id)
            .await
            .unwrap()
    );
    assert_counts(&database, late_id, late_reservation.id, 0).await;
    let outcome: String = sqlx::query_scalar(
        "SELECT statistics_outcome FROM terminal_projection_outbox WHERE request_id = $1",
    )
    .bind(late_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(outcome, "pruned");
    let late_account_owner = Uuid::now_v7();
    assert_eq!(
        database
            .claim_metered_usage_projection_tasks(late_account_owner, 32)
            .await
            .unwrap()
            .len(),
        0
    );
    let late_conversation_owner = Uuid::now_v7();
    assert_eq!(
        database
            .claim_conversation_projection_tasks(late_conversation_owner, 32)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        database
            .project_claimed_conversation_projection_task(late_conversation_owner, late_id)
            .await
            .unwrap()
    );
    let conversation_outcome: String = sqlx::query_scalar(
        "SELECT statistics_outcome FROM conversation_projection_outbox WHERE request_id = $1",
    )
    .bind(late_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(conversation_outcome, "pruned");
    for account in [key.account_id, late_key.account_id] {
        let lifetime: i64 = sqlx::query_scalar(
            "SELECT settled_lifetime_micros FROM account_usage_state WHERE account_id = $1",
        )
        .bind(account.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap();
        assert_eq!(lifetime, 10);
    }
    database.pool.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
}
