use super::super::*;
use crate::conversation::ConversationHints;
use std::time::Duration;

mod cache_coverage;
mod semantic_isolation;
mod semantic_retry;

#[tokio::test]
async fn sqlite_deferred_session_body_retries_after_terminal_completion() {
    let directory = tempfile::tempdir().unwrap();
    let state = crate::AppState::initialize(crate::config::Config::for_test(format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("deferred-session-body.db").display()
    )))
    .await
    .unwrap();
    let database = &state.db;
    let (key, reservation, request_id) = admitted(database, false).await;
    let request = serde_json::json!({"messages": [{"role": "user", "content": "retry this turn"}]});
    let hints = ConversationHints::default();
    let mut input = finish(&key, &reservation, request_id);
    input.conversation = Some(ProxyConversationInput {
        key: &key,
        request_json: &request,
        hints: &hints,
        client_name: None,
        upstream_response_id: None,
    });
    database.finish_proxy_request_deferred(input).await.unwrap();

    let empty_owner = Uuid::now_v7();
    assert_eq!(
        database
            .claim_conversation_projection_tasks(empty_owner, 1)
            .await
            .unwrap()[0]
            .request_id,
        request_id
    );
    assert!(
        !database
            .project_claimed_conversation_projection_task(empty_owner, request_id)
            .await
            .unwrap()
    );

    // The empty snapshot must release its lease without waiting five minutes.
    let body_owner = Uuid::now_v7();
    assert_eq!(
        database
            .claim_conversation_projection_tasks(body_owner, 1)
            .await
            .unwrap()[0]
            .request_id,
        request_id
    );
    // The worker materializes content before discovering the terminal writer
    // is still pending. Its rolled-back observation must release ownership too.
    assert!(
        !crate::worker::archive_session_body(&state, body_owner, request_id)
            .await
            .unwrap()
    );
    let row = sqlx::query("SELECT lease_owner, lease_expires_at, terminal_lease_owner, terminal_lease_expires_at, projected_at FROM conversation_projection_outbox WHERE request_id = $1")
        .bind(request_id.to_string()).fetch_one(&database.pool).await.unwrap();
    assert_eq!(row.get::<String, _>("lease_owner"), "terminal-v118");
    assert_eq!(row.get::<i64, _>("lease_expires_at"), i64::MAX);
    assert_eq!(row.get::<Option<String>, _>("terminal_lease_owner"), None);
    assert_eq!(row.get::<Option<i64>, _>("terminal_lease_expires_at"), None);
    assert_eq!(row.get::<Option<i64>, _>("projected_at"), None);

    let owner = Uuid::now_v7();
    assert_eq!(
        database
            .claim_conversation_projection_tasks(owner, 1)
            .await
            .unwrap()[0]
            .request_id,
        request_id
    );
    // A stale owner cannot release the replacement worker's lease.
    assert!(
        !database
            .project_claimed_conversation_projection_task(body_owner, request_id)
            .await
            .unwrap()
    );
    // Statement retries must also recognize an already materialized snapshot,
    // whose payload marker has been cleared by the session-body worker.
    assert!(
        database
            .schedule_rolled_back_conversation_retry(request_id, owner)
            .await
            .unwrap()
    );
    let retry_at: i64 = sqlx::query_scalar(
        "SELECT terminal_lease_expires_at FROM conversation_projection_outbox WHERE request_id = $1",
    )
    .bind(request_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert!(retry_at <= unix_millis() + 4000);
    let terminal_owner = Uuid::now_v7();
    assert!(
        database
            .claim_terminal_projection_tasks(terminal_owner, 1)
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
    assert!(
        crate::worker::archive_session_body(&state, owner, request_id)
            .await
            .unwrap()
    );
    assert!(
        !crate::worker::archive_session_body(&state, owner, request_id)
            .await
            .unwrap()
    );
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM conversation_observations WHERE request_id = $1")
            .bind(request_id.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(count, 1);
    assert!(
        database
            .claim_conversation_projection_tasks(Uuid::now_v7(), 1)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn sqlite_deferred_session_edges_follow_admission_when_settlement_is_reversed() {
    let directory = tempfile::tempdir().unwrap();
    let state = crate::AppState::initialize(crate::config::Config::for_test(format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("deferred-session-order.db").display()
    )))
    .await
    .unwrap();
    let database = &state.db;
    let (key, parent_reservation, parent) = admitted(database, false).await;
    let price = database
        .upsert_model_price("deferred-order", "USD", Decimal::ONE, Decimal::ONE)
        .await
        .unwrap();
    let child = Uuid::now_v7();
    let child_reservation = database
        .start_proxy_request(StartProxyRequest {
            request_id: child,
            key: &key,
            price: &price,
            input_token_ceiling: 10,
            output_token_ceiling: 10,
            protocol: "openai",
            model: "deferred-order",
            request_object: "gap://deferred-order/request",
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap();
    let hints = ConversationHints {
        session_id: Some("deferred-order".into()),
        ..Default::default()
    };
    let parent_body = serde_json::json!({"messages": [{"role": "user", "content": "one"}]});
    let child_body = serde_json::json!({"messages": [
        {"role": "user", "content": "one"},
        {"role": "assistant", "content": "hello"},
        {"role": "user", "content": "three"}
    ]});
    for (request_id, reservation, body) in [
        (child, &child_reservation, &child_body),
        (parent, &parent_reservation, &parent_body),
    ] {
        let mut input = finish(&key, reservation, request_id);
        input.response_object =
            r#"inline-json:{"choices":[{"message":{"role":"assistant","content":"hello"}}]}"#;
        input.conversation = Some(ProxyConversationInput {
            key: &key,
            request_json: body,
            hints: &hints,
            client_name: None,
            upstream_response_id: None,
        });
        database.finish_proxy_request_deferred(input).await.unwrap();
        let terminal_owner = Uuid::now_v7();
        database
            .claim_terminal_projection_tasks(terminal_owner, 2)
            .await
            .unwrap();
        assert!(
            database
                .project_claimed_terminal_projection_task(terminal_owner, request_id)
                .await
                .unwrap()
        );
        let owner = Uuid::now_v7();
        assert_eq!(
            database
                .claim_conversation_projection_tasks(owner, 1)
                .await
                .unwrap()[0]
                .request_id,
            request_id
        );
        assert!(
            crate::worker::archive_session_body(&state, owner, request_id)
                .await
                .unwrap()
        );
    }
    let edges: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT source.request_id, target.request_id, edge.relation_kind FROM conversation_edges edge JOIN conversation_observations source ON source.id = edge.from_observation_id JOIN conversation_observations target ON target.id = edge.to_observation_id",
    )
    .fetch_all(&database.pool)
    .await
    .unwrap();
    assert_eq!(
        edges,
        vec![(parent.to_string(), child.to_string(), "continues".into())]
    );
}

#[tokio::test]
async fn sqlite_session_body_rebuild_rewrites_only_two_locator_rows() {
    let directory = tempfile::tempdir().unwrap();
    let state = crate::AppState::initialize(crate::config::Config::for_test(format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("session-body-rebuild.db").display()
    )))
    .await
    .unwrap();
    let database = &state.db;
    let request = serde_json::json!({
        "model": "raw-provider-model",
        "private_request_field": "exclude from training",
        "messages": [
            {"role": "system", "content": "rules"},
            {"role": "user", "content": "question"}
        ]
    });
    let response = serde_json::json!({
        "id": "raw-provider-response-id",
        "private_response_field": "exclude from training",
        "choices": [{"message": {"role": "assistant", "content": "answer"}}]
    });
    let request_locator = format!("inline-json:{request}");
    let response_locator = format!("inline-json:{response}");
    let mut selected = None;
    let mut excluded = Vec::new();
    for (request_object, response_object) in [
        ("", None),
        (request_locator.as_str(), None),
        (request_locator.as_str(), Some("")),
        ("", Some(response_locator.as_str())),
        (request_locator.as_str(), Some(response_locator.as_str())),
    ] {
        let (key, reservation, request_id) = admitted(database, false).await;
        let hints = ConversationHints {
            session_id: Some(request_id.to_string()),
            ..ConversationHints::default()
        };
        let mut input = finish(&key, &reservation, request_id);
        input.conversation = Some(ProxyConversationInput {
            key: &key,
            request_json: &request,
            hints: &hints,
            client_name: None,
            upstream_response_id: None,
        });
        database.finish_proxy_request_deferred(input).await.unwrap();
        let terminal_owner = Uuid::now_v7();
        database
            .claim_terminal_projection_tasks(terminal_owner, 32)
            .await
            .unwrap();
        assert!(
            database
                .project_claimed_terminal_projection_task(terminal_owner, request_id)
                .await
                .unwrap()
        );
        let cluster = database
            .record_conversation_observation(&key, request_id, &request, &hints, None)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE request_records SET request_object = $1, response_object = $2 WHERE id = $3",
        )
        .bind(request_object)
        .bind(response_object)
        .bind(request_id.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
        // Both old snapshot columns must be cleared before exposing a rebuild.
        sqlx::query("UPDATE conversation_projection_outbox SET projected_at = 1, statistics_outcome = 'applied', semantic_snapshot_json = '{}', semantic_payload_snapshot_json = '{}', lease_owner = NULL, lease_expires_at = NULL WHERE request_id = $1")
            .bind(request_id.to_string()).execute(&database.pool).await.unwrap();
        if !request_object.is_empty() && response_object.is_some_and(|value| !value.is_empty()) {
            selected = Some((request_id, key.tenant_id, cluster));
        } else {
            excluded.push(request_id);
        }
    }
    let (request_id, tenant_id, cluster) = selected.unwrap();
    assert_eq!(database.reopen_session_body_rebuilds(32).await.unwrap(), 1);
    let marker = sqlx::query("SELECT projected_at, semantic_snapshot_json, semantic_payload_snapshot_json, lease_owner, lease_expires_at, terminal_lease_owner, terminal_lease_expires_at FROM conversation_projection_outbox WHERE request_id = $1")
        .bind(request_id.to_string()).fetch_one(&database.pool).await.unwrap();
    assert_eq!(marker.get::<Option<i64>, _>("projected_at"), None);
    for field in [
        "semantic_snapshot_json",
        "semantic_payload_snapshot_json",
        "terminal_lease_owner",
    ] {
        assert_eq!(marker.get::<Option<String>, _>(field), None);
    }
    assert_eq!(
        marker.get::<Option<i64>, _>("terminal_lease_expires_at"),
        None
    );
    assert_eq!(
        marker.get::<String, _>("lease_owner"),
        "session-body-rebuild"
    );
    assert_eq!(marker.get::<i64, _>("lease_expires_at"), i64::MAX);
    let old_claimable: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conversation_projection_outbox WHERE projected_at IS NULL AND ((lease_expires_at IS NULL OR lease_expires_at <= $1) OR ((semantic_snapshot_json IS NOT NULL OR semantic_payload_snapshot_json IS NOT NULL) AND (terminal_lease_expires_at IS NULL OR terminal_lease_expires_at <= $1)))")
        .bind(unix_millis()).fetch_one(&database.pool).await.unwrap();
    assert_eq!(old_claimable, 0);
    let owner = Uuid::now_v7();
    let tasks = database
        .claim_conversation_projection_tasks(owner, 32)
        .await
        .unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].request_id, request_id);
    assert!(
        crate::worker::archive_session_body(&state, owner, request_id)
            .await
            .unwrap()
    );
    let expected = crate::conversation::session_body_from_documents(&request, &response);
    let (atoms, nodes) = expected.into_prefix();
    assert_eq!(atoms.len(), 3);
    let observation = sqlx::query(
        "SELECT leaf_node_hash, cluster_id FROM conversation_observations WHERE request_id = $1",
    )
    .bind(request_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(
        observation.get::<String, _>("leaf_node_hash"),
        nodes.last().unwrap().node_hash
    );
    assert_eq!(
        observation.get::<String, _>("cluster_id"),
        cluster.to_string()
    );
    let mut turns = Vec::new();
    for atom in atoms {
        let content: String = sqlx::query_scalar(
            "SELECT content_json FROM semantic_atoms WHERE tenant_id = $1 AND content_hash = $2",
        )
        .bind(tenant_id.to_string())
        .bind(&atom.content_hash)
        .fetch_one(&database.pool)
        .await
        .unwrap();
        assert!(!content.contains("private_request_field"));
        assert!(!content.contains("private_response_field"));
        assert!(!content.contains("raw-provider"));
        turns.push(serde_json::from_str(&content).unwrap());
    }
    let dialogue = crate::conversation::training_dialogue(&turns);
    assert_eq!(
        dialogue
            .iter()
            .map(|turn| turn.role.as_str())
            .collect::<Vec<_>>(),
        ["system", "user", "assistant"]
    );
    assert_eq!(dialogue[2].origin, "return");
    assert_eq!(dialogue[2].parts[0]["text"], "answer");
    assert_eq!(database.reopen_session_body_rebuilds(32).await.unwrap(), 0);
    for excluded_id in excluded {
        let row = sqlx::query("SELECT projected_at, statistics_outcome, semantic_snapshot_json, semantic_payload_snapshot_json FROM conversation_projection_outbox WHERE request_id = $1")
            .bind(excluded_id.to_string()).fetch_one(&database.pool).await.unwrap();
        assert_eq!(row.get::<i64, _>("projected_at"), 1);
        assert_eq!(row.get::<String, _>("statistics_outcome"), "applied");
        assert_eq!(row.get::<String, _>("semantic_snapshot_json"), "{}");
        assert_eq!(row.get::<String, _>("semantic_payload_snapshot_json"), "{}");
    }
}

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
    let envelope = sqlx::query("SELECT request_json, COALESCE(semantic_payload_snapshot_json, semantic_snapshot_json) AS semantic_snapshot_json, lease_owner FROM conversation_projection_outbox WHERE request_id = $1")
        .bind(request_id.to_string()).fetch_one(&database.pool).await.unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&envelope.get::<String, _>("request_json"))
            .unwrap(),
        body,
        "outbox keeps the reduced dialogue for a gap locator"
    );
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
    assert!(
        database
            .claim_conversation_projection_tasks(conversation_owner, 32)
            .await
            .unwrap()
            .iter()
            .any(|task| task.request_id == request_id)
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
async fn sqlite_terminal_ack_failure_recovers_across_pools_without_recharging() {
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
    database.pool.close().await;
    let database = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("rollback.db").display()
    ))
    .await
    .unwrap();
    sqlx::query("UPDATE terminal_projection_outbox SET lease_expires_at = 0 WHERE request_id = $1")
        .bind(request_id.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    let recovery_owner = Uuid::now_v7();
    assert!(
        database
            .claim_terminal_projection_tasks(recovery_owner, 32)
            .await
            .unwrap()
            .contains(&request_id)
    );
    assert!(
        !database
            .project_claimed_terminal_projection_task(owner, request_id)
            .await
            .unwrap()
    );
    assert!(
        database
            .project_claimed_terminal_projection_task(recovery_owner, request_id)
            .await
            .unwrap()
    );
    database.pool.close().await;
    let database = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("rollback.db").display()
    ))
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
    assert!(
        !database
            .claim_terminal_projection_tasks(Uuid::now_v7(), 32)
            .await
            .unwrap()
            .contains(&request_id)
    );
    assert!(
        !database
            .project_claimed_terminal_projection_task(recovery_owner, request_id)
            .await
            .unwrap()
    );
    let lifetime: i64 = sqlx::query_scalar(
        "SELECT settled_lifetime_micros FROM account_usage_state WHERE account_id = $1",
    )
    .bind(key.account_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(lifetime, 10);
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

#[tokio::test]
async fn postgres_terminal_and_legacy_metered_workers_share_account_first_order() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let admin = sqlx::PgPool::connect(&database_url).await.unwrap();
    let schema = format!("terminal_mixed_{}", Uuid::now_v7().simple());
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
    let (key, legacy_reservation, legacy_id) = admitted(&database, true).await;
    let model: String = sqlx::query_scalar("SELECT model FROM request_records WHERE id = $1")
        .bind(legacy_id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap();
    let price = database
        .upsert_model_price(&model, "USD", Decimal::ONE, Decimal::ONE)
        .await
        .unwrap();
    let terminal_id = Uuid::now_v7();
    let terminal_reservation = database
        .start_proxy_request(StartProxyRequest {
            request_id: terminal_id,
            key: &key,
            price: &price,
            input_token_ceiling: 10,
            output_token_ceiling: 10,
            protocol: "openai",
            model: &model,
            request_object: "gap://mixed/request",
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap();
    database
        .finish_proxy_request(finish(&key, &legacy_reservation, legacy_id))
        .await
        .unwrap();
    database
        .finish_proxy_request_deferred(finish(&key, &terminal_reservation, terminal_id))
        .await
        .unwrap();
    let legacy_owner = Uuid::now_v7();
    assert_eq!(
        database
            .claim_metered_usage_projection_tasks(legacy_owner, 32)
            .await
            .unwrap()
            .len(),
        1
    );
    let terminal_owner = Uuid::now_v7();
    assert_eq!(
        database
            .claim_terminal_projection_tasks(terminal_owner, 32)
            .await
            .unwrap(),
        vec![terminal_id]
    );
    let legacy = Database::connect_with_max(isolated.as_str(), 1)
        .await
        .unwrap();
    let terminal = Database::connect_with_max(isolated.as_str(), 1)
        .await
        .unwrap();
    let legacy_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&legacy.pool)
        .await
        .unwrap();
    let terminal_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&terminal.pool)
        .await
        .unwrap();
    let trigger = format!(
        "CREATE FUNCTION mixed_account_gate() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF pg_backend_pid() = {legacy_pid} THEN PERFORM pg_advisory_xact_lock(hashtextextended('{schema}', 0)); END IF; RETURN NEW; END $$; CREATE TRIGGER mixed_account_gate AFTER UPDATE ON credit_accounts FOR EACH ROW EXECUTE FUNCTION mixed_account_gate();"
    );
    sqlx::raw_sql(sqlx::AssertSqlSafe(trigger))
        .execute(&database.pool)
        .await
        .unwrap();
    let mut gate = database.begin_write_transaction().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(&schema)
        .execute(&mut *gate)
        .await
        .unwrap();
    let legacy_task = tokio::spawn(async move {
        legacy
            .project_claimed_metered_usage_projection_task(legacy_owner, legacy_reservation.id)
            .await
    });
    wait_for_projection_lock(&database, legacy_pid).await;
    let terminal_task = tokio::spawn(async move {
        terminal
            .project_claimed_terminal_projection_task(terminal_owner, terminal_id)
            .await
    });
    wait_for_projection_lock(&database, terminal_pid).await;
    gate.commit().await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        assert!(legacy_task.await.unwrap().unwrap());
        assert!(terminal_task.await.unwrap().unwrap());
    })
    .await
    .expect("mixed workers must complete without an account/aggregate deadlock");
    let lifetime: i64 = sqlx::query_scalar(
        "SELECT settled_lifetime_micros FROM account_usage_state WHERE account_id = $1",
    )
    .bind(key.account_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(lifetime, 20);
    let requests: i64 = sqlx::query_scalar(
        "SELECT CAST(SUM(requests) AS BIGINT) FROM request_daily_aggregates WHERE key_id = $1",
    )
    .bind(key.key_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(requests, 2);
    assert_counts(&database, terminal_id, terminal_reservation.id, 1).await;
    database.pool.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
}

async fn wait_for_projection_lock(database: &Database, pid: i32) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let waiting: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM pg_locks WHERE pid = $1 AND NOT granted")
                    .bind(pid)
                    .fetch_one(&database.pool)
                    .await
                    .unwrap();
            if waiting > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("worker must reach the controlled lock gate");
}

#[tokio::test]
async fn terminal_projection_configuration_reaches_real_finalization_and_persistence_pool() {
    for enabled in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("configured.db").display()
        );
        let mut config = crate::config::Config::for_test(database_url);
        assert!(!config.terminal_projection_enabled);
        config.terminal_projection_enabled = enabled;
        let state = crate::AppState::initialize(config).await.unwrap();
        let database = state.db.independent_persistence_pool();
        let (key, reservation, request_id) = admitted(&database, true).await;
        database
            .finish_proxy_request(finish(&key, &reservation, request_id))
            .await
            .unwrap();
        let pending: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM terminal_projection_outbox WHERE request_id = $1 AND projected_at IS NULL")
            .bind(request_id.to_string()).fetch_one(&database.pool).await.unwrap();
        assert_eq!(pending, i64::from(enabled));
        if enabled {
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
        }
        assert_counts(&database, request_id, reservation.id, 1).await;
    }
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
    sqlx::query("DELETE FROM request_stats_facts WHERE request_id = $1")
        .bind(request_id.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    assert!(
        !database
            .project_claimed_terminal_projection_task(owner, request_id)
            .await
            .unwrap()
    );
    for _attempt in 0..2 {
        maintenance(&database, rebuild.clone()).await.unwrap();
        assert_counts(&database, request_id, reservation.id, 1).await;
    }
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
