use super::*;
use crate::conversation::ConversationHints;
use crate::db::requests::conversations::{
    ConversationProjectionEnqueueInput, enqueue_conversation_projection_in_transaction,
};

#[tokio::test]
async fn sqlite_unknown_finance_allows_operational_conversation_projection() {
    contract(Fixture::sqlite().await).await;
    enqueue_contract(Fixture::sqlite().await).await;
}

#[tokio::test]
async fn postgres_unknown_finance_allows_operational_conversation_projection() {
    let Some(fixture) = Fixture::postgres().await else {
        return;
    };
    contract(fixture).await;
    enqueue_contract(Fixture::postgres().await.unwrap()).await;
}

async fn enqueue_contract(fixture: Fixture) {
    let body = serde_json::json!({"messages": [{"role": "user", "content": "unknown usage"}]});
    let hints = ConversationHints::default();
    let conversation = ProxyConversationInput {
        key: &fixture.key,
        request_json: &body,
        hints: &hints,
        client_name: Some("Codex"),
        upstream_response_id: None,
    };
    let receipt = Uuid::now_v7();
    assert!(
        fixture
            .database
            .enqueue_operational_conversation_projection(&fixture.claim, receipt, &conversation)
            .await
            .is_err()
    );
    fixture
        .record(receipt, &fixture.operational(429))
        .await
        .unwrap();
    let mut forged = fixture.claim.clone();
    forged.fence = Uuid::now_v7();
    assert!(
        fixture
            .database
            .enqueue_operational_conversation_projection(&forged, receipt, &conversation)
            .await
            .is_err()
    );
    assert!(
        fixture
            .database
            .enqueue_operational_conversation_projection(
                &fixture.claim,
                Uuid::now_v7(),
                &conversation,
            )
            .await
            .is_err()
    );
    for _ in 0..2 {
        fixture
            .database
            .enqueue_operational_conversation_projection(&fixture.claim, receipt, &conversation)
            .await
            .unwrap();
    }
    let owner = Uuid::now_v7();
    let tasks = fixture
        .database
        .claim_conversation_projection_tasks(owner, 1)
        .await
        .unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].request_id, fixture.claim.request_id);
    let (atoms, nodes) =
        crate::conversation::session_body_from_documents(&body, &serde_json::json!({}))
            .into_prefix();
    assert!(
        fixture
            .database
            .prepare_session_body_archive(owner, fixture.claim.request_id, atoms, &nodes, &[])
            .await
            .unwrap()
    );
    assert!(
        fixture
            .database
            .project_claimed_conversation_projection_task(owner, fixture.claim.request_id)
            .await
            .unwrap()
    );
    fixture
        .database
        .enqueue_operational_conversation_projection(&fixture.claim, receipt, &conversation)
        .await
        .unwrap();
    let remaining: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM conversation_semantic_payloads WHERE request_id = $1",
    )
    .bind(fixture.claim.request_id.to_string())
    .fetch_one(&fixture.database.pool)
    .await
    .unwrap();
    assert_eq!(remaining, 0, "replay must not resurrect consumed content");
    fixture.close().await;
}

async fn contract(fixture: Fixture) {
    let request_id = fixture.claim.request_id;
    let active_filter = LogicalSessionListFilter {
        state: "active".into(),
        limit: 10,
        ..Default::default()
    };
    let active = fixture
        .database
        .operator_recent_sessions("operational-test", active_filter.clone())
        .await
        .unwrap();
    assert_eq!(active.len(), 1, "a running request must remain active");
    assert_eq!(active[0].active_requests, 1);
    let hints = ConversationHints::default();
    let mut transaction = fixture.database.begin_write_transaction().await.unwrap();
    enqueue_conversation_projection_in_transaction(
        &mut transaction,
        ConversationProjectionEnqueueInput {
            request_id,
            key: &fixture.key,
            hints: &hints,
            client_name: Some("Codex"),
            upstream_response_id: None,
            observed_at: unix_millis(),
        },
    )
    .await
    .unwrap();
    transaction.commit().await.unwrap();
    let owner = Uuid::now_v7();
    let tasks = fixture
        .database
        .claim_conversation_projection_tasks(owner, 1)
        .await
        .unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].request_id, request_id);
    // An empty deferred snapshot is a harmless no-op, even before runtime completion.
    assert!(
        !fixture
            .database
            .project_claimed_conversation_projection_task(owner, request_id)
            .await
            .unwrap()
    );
    let terminal = fixture.operational(429);
    fixture.record(Uuid::now_v7(), &terminal).await.unwrap();
    fixture
        .database
        .mark_pending_proxy_settlement_unknown(&fixture.claim)
        .await
        .unwrap();
    super::unknown_bridge::drain_terminal(&fixture).await;
    let statistics_event = projected_events(&fixture, request_id).await;
    assert_eq!(
        statistics_event.len(),
        1,
        "statistics commit publishes once"
    );
    let statistics_event_id: String = statistics_event[0].get("event_id");
    let statistics_cursor = (
        statistics_event[0].get::<i64, _>("event_at"),
        statistics_event_id.clone(),
    );
    let (projected_at, usage_basis, stats_outcome): (Option<i64>, String, String) = sqlx::query_as(
        "SELECT projected_at, usage_basis, statistics_outcome FROM terminal_projection_outbox WHERE request_id = $1 AND tenant_id = $2 AND key_id = $3",
    )
    .bind(request_id.to_string())
    .bind(fixture.key.tenant_id.to_string())
    .bind(fixture.key.key_id.to_string())
    .fetch_one(&fixture.database.pool)
    .await
    .unwrap();
    assert!(projected_at.is_some());
    assert_eq!(usage_basis, "not_observed");
    assert_eq!(stats_outcome, "applied");
    let tasks = fixture
        .database
        .claim_conversation_projection_tasks(owner, 1)
        .await
        .unwrap();
    assert_eq!(tasks.len(), 1);
    let body = serde_json::json!({"messages": [{"role": "user", "content": "unknown usage"}]});
    let session = crate::conversation::session_body_from_documents(&body, &serde_json::json!({}));
    let (atoms, nodes) = session.into_prefix();
    assert!(
        fixture
            .database
            .prepare_session_body_archive(owner, request_id, atoms, &nodes, &[])
            .await
            .unwrap()
    );
    assert!(
        fixture
            .database
            .project_claimed_conversation_projection_task(owner, request_id)
            .await
            .unwrap()
    );
    let events = projected_events(&fixture, request_id).await;
    assert_eq!(events.len(), 2, "semantic commit adds one event");
    let event_ids: Vec<String> = events.iter().map(|row| row.get("event_id")).collect();
    assert!(event_ids.contains(&statistics_event_id));
    assert_ne!(event_ids[0], event_ids[1]);
    let semantic_event = events
        .iter()
        .find(|row| row.get::<String, _>("event_id") != statistics_event_id)
        .unwrap();
    let semantic_cursor = (
        semantic_event.get::<i64, _>("event_at"),
        semantic_event.get::<String, _>("event_id"),
    );
    assert!(
        semantic_cursor > statistics_cursor,
        "semantic event must be visible after the statistics cursor"
    );
    assert_eq!(semantic_event.get::<i64, _>("status_code"), 429);
    assert_eq!(
        semantic_event.get::<i64, _>("duration_ms"),
        terminal.duration_ms
    );
    assert_eq!(
        semantic_event.get::<Option<String>, _>("error_code"),
        terminal.error_code
    );
    assert!(
        !fixture
            .database
            .project_claimed_conversation_projection_task(owner, request_id)
            .await
            .unwrap()
    );
    let replay_ids: Vec<String> = projected_events(&fixture, request_id)
        .await
        .iter()
        .map(|row| row.get("event_id"))
        .collect();
    assert_eq!(
        replay_ids, event_ids,
        "replay must preserve both publications"
    );
    let row = sqlx::query(
        "SELECT r.completed_at, r.conversation_cluster_id, p.state, p.unknown_reason, x.projected_at AS semantic_completed, x.statistics_outcome AS semantic_outcome FROM request_records r JOIN pending_proxy_settlements p ON p.request_id = r.id JOIN conversation_projection_outbox x ON x.request_id = r.id AND x.tenant_id = r.tenant_id AND x.key_id = r.key_id WHERE r.id = $1",
    )
    .bind(request_id.to_string())
    .fetch_one(&fixture.database.pool)
    .await
    .unwrap();
    assert!(
        row.try_get::<Option<i64>, _>("completed_at")
            .unwrap()
            .is_none()
    );
    assert!(
        row.try_get::<Option<String>, _>("conversation_cluster_id")
            .unwrap()
            .is_some()
    );
    assert!(row.get::<Option<i64>, _>("semantic_completed").is_some());
    assert_eq!(row.get::<String, _>("semantic_outcome"), "session_body");
    assert_eq!(row.try_get::<String, _>("state").unwrap(), "pending");
    assert_eq!(
        row.try_get::<String, _>("unknown_reason").unwrap(),
        "authoritative_usage_unavailable"
    );
    assert!(
        fixture
            .database
            .operator_recent_sessions("operational-test", active_filter.clone())
            .await
            .unwrap()
            .is_empty(),
        "runtime completion must leave the active filter while finance stays pending"
    );
    let session_id = row.try_get::<String, _>("conversation_cluster_id").unwrap();
    assert!(
        fixture
            .database
            .operator_session_summaries(
                "operational-test",
                &[(fixture.key.key_id, session_id.clone())],
                active_filter,
            )
            .await
            .unwrap()
            .is_empty(),
        "targeted realtime refresh must use the same runtime predicate"
    );
    for model in [None, Some("operational-model".into())] {
        let sessions = fixture
            .database
            .operator_recent_sessions(
                "operational-test",
                LogicalSessionListFilter {
                    state: "all".into(),
                    limit: 10,
                    model,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(sessions.len(), 1, "completed history remains discoverable");
        for session in sessions {
            assert_eq!(session.active_requests, 0);
        }
    }
    fixture.close().await;
}

async fn projected_events(fixture: &Fixture, request_id: Uuid) -> Vec<sqlx::any::AnyRow> {
    let events = sqlx::query(
        "SELECT event_id, event_at, tenant_id, key_id, status_code, duration_ms, error_code FROM request_events WHERE request_id = $1 AND event_kind = 'projected' ORDER BY event_id",
    )
    .bind(request_id.to_string())
    .fetch_all(&fixture.database.pool)
    .await
    .unwrap();
    let tenant = fixture.key.tenant_id.to_string();
    let key = fixture.key.key_id.to_string();
    for event in &events {
        assert_eq!(event.get::<String, _>("tenant_id"), tenant);
        assert_eq!(event.get::<String, _>("key_id"), key);
    }
    events
}
