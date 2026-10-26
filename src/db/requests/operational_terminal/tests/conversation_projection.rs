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
    // Merely having a request is not evidence of runtime completion.
    assert!(
        fixture
            .database
            .project_claimed_conversation_projection_task(owner, request_id)
            .await
            .is_err()
    );
    let terminal = fixture.operational(429);
    fixture.record(Uuid::now_v7(), &terminal).await.unwrap();
    fixture
        .database
        .mark_pending_proxy_settlement_unknown(&fixture.claim)
        .await
        .unwrap();
    assert!(
        fixture
            .database
            .project_claimed_conversation_projection_task(owner, request_id)
            .await
            .unwrap()
    );
    assert!(
        !fixture
            .database
            .project_claimed_conversation_projection_task(owner, request_id)
            .await
            .unwrap()
    );
    let event = sqlx::query(
        "SELECT status_code, duration_ms, error_code FROM request_events WHERE request_id = $1 AND event_kind = 'projected'",
    )
    .bind(request_id.to_string())
    .fetch_all(&fixture.database.pool)
    .await
    .unwrap();
    assert_eq!(event.len(), 1, "semantic replay must not duplicate events");
    assert_eq!(event[0].try_get::<i64, _>("status_code").unwrap(), 429);
    assert_eq!(
        event[0].try_get::<i64, _>("duration_ms").unwrap(),
        terminal.duration_ms
    );
    assert_eq!(
        event[0].try_get::<Option<String>, _>("error_code").unwrap(),
        terminal.error_code
    );
    let row = sqlx::query(
        "SELECT r.completed_at, r.conversation_cluster_id, p.state, p.unknown_reason FROM request_records r JOIN pending_proxy_settlements p ON p.request_id = r.id WHERE r.id = $1",
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
