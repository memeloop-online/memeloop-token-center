use super::*;

#[tokio::test]
async fn sqlite_lifecycle_events_do_not_wait_for_archive_or_financial_projection() {
    lifecycle_events_contract(Fixture::sqlite().await).await;
}

#[tokio::test]
async fn postgres_lifecycle_events_do_not_wait_for_archive_or_financial_projection() {
    if let Some(fixture) = Fixture::postgres().await {
        lifecycle_events_contract(fixture).await;
    }
}

async fn lifecycle_events_contract(fixture: Fixture) {
    let archive_gate = fixture
        .database
        .saturate_gateway_persistence_for_test()
        .await;
    let price = fixture
        .database
        .upsert_model_price("operational-model", "USD", Decimal::ONE, Decimal::ONE)
        .await
        .unwrap();
    let request_id = Uuid::now_v7();
    let reservation = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        fixture.database.start_proxy_forwarding_request(
            StartProxyRequest {
                request_id,
                key: &fixture.key,
                price: &price,
                input_token_ceiling: 10,
                output_token_ceiling: 20,
                protocol: "openai",
                model: "operational-model",
                request_object: "gap://blocked-archive/request",
                upstream_account_id: None,
                model_route_id: None,
            },
            None,
        ),
    )
    .await
    .expect("admission cannot wait for archive permits")
    .unwrap();
    let started = fixture
        .database
        .request_events_after("operational-test", 0, None, 500)
        .await
        .unwrap();
    let started = started
        .iter()
        .find(|event| event.request_id == request_id)
        .expect("admission commits its event");
    assert_eq!(started.event_kind, "started");
    assert!(started.status_code.is_none());
    assert!(started.usage.tokens.is_none());
    assert!(started.billing.cost.is_none());
    let identity = ArchiveSpoolIdentity {
        request_id,
        tenant_id: fixture.key.tenant_id,
        reservation_id: reservation.id,
    };
    fixture
        .database
        .publish_proxy_started_event(identity)
        .await
        .unwrap();
    let started_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM request_events WHERE request_id = $1 AND event_kind = 'started'",
    )
    .bind(request_id.to_string())
    .fetch_one(&fixture.database.pool)
    .await
    .unwrap();
    assert_eq!(
        started_count, 1,
        "compatibility retries cannot duplicate admission"
    );

    let mut terminal = fixture.operational(200);
    terminal.error_code = None;
    terminal.terminal_cause = None;
    let receipt = Uuid::now_v7();
    fixture.record(receipt, &terminal).await.unwrap();
    fixture.record(receipt, &terminal).await.unwrap();
    let before = fixture
        .database
        .request_events_after("operational-test", 0, None, 500)
        .await
        .unwrap();
    let finished = before
        .iter()
        .filter(|event| {
            event.request_id == fixture.claim.request_id && event.event_kind == "finished"
        })
        .collect::<Vec<_>>();
    assert_eq!(finished.len(), 1, "one execution receipt has one event");
    assert_eq!(finished[0].status_code, Some(200));
    assert!(finished[0].usage.tokens.is_none());
    assert!(finished[0].billing.cost.is_none());
    let finished_cursor = (finished[0].event_at, finished[0].event_id);
    let json = serde_json::to_value(finished[0]).unwrap();
    assert_eq!(json["operational_terminal"]["financial_state"], "pending");
    fixture.assert_unsettled().await;
    // The semantic projector uses the same public event kind while finance is
    // still unknown. It must not suppress the later financial publication.
    let mut transaction = fixture.database.begin_write_transaction().await.unwrap();
    crate::db::requests::conversations::emit_conversation_projected_event_in_transaction(
        &mut transaction,
        fixture.claim.request_id,
        fixture.key.tenant_id,
        fixture.key.key_id,
    )
    .await
    .unwrap();
    transaction.commit().await.unwrap();
    let semantic = fixture
        .database
        .request_events_after(
            "operational-test",
            finished_cursor.0,
            Some(finished_cursor.1),
            500,
        )
        .await
        .unwrap();
    let semantic = semantic
        .iter()
        .find(|event| {
            event.request_id == fixture.claim.request_id && event.event_kind == "projected"
        })
        .unwrap();
    assert!(semantic.billing.cost.is_none());
    let semantic_cursor = (semantic.event_at, semantic.event_id);

    fixture.finish(receipt).await.unwrap();
    let owner = Uuid::now_v7();
    assert_eq!(
        fixture
            .database
            .claim_terminal_projection_tasks(owner, 10)
            .await
            .unwrap(),
        vec![fixture.claim.request_id]
    );
    assert!(
        fixture
            .database
            .project_claimed_terminal_projection_task(owner, fixture.claim.request_id)
            .await
            .unwrap()
    );
    assert!(
        !fixture
            .database
            .project_claimed_terminal_projection_task(owner, fixture.claim.request_id)
            .await
            .unwrap()
    );
    let after = fixture
        .database
        .request_events_after(
            "operational-test",
            semantic_cursor.0,
            Some(semantic_cursor.1),
            500,
        )
        .await
        .unwrap();
    let projected = after
        .iter()
        .filter(|event| {
            event.request_id == fixture.claim.request_id && event.event_kind == "projected"
        })
        .collect::<Vec<_>>();
    assert_eq!(
        projected.len(),
        1,
        "financial acknowledgement advances the stream exactly once"
    );
    let json = serde_json::to_value(projected[0]).unwrap();
    assert_eq!(json["operational_terminal"]["financial_state"], "applied");
    assert_eq!(json["input_tokens"], 5);
    assert_eq!(json["output_tokens"], 7);
    let rest = fixture
        .database
        .list_requests(fixture.key.key_id, 10)
        .await
        .unwrap();
    let rest = rest
        .iter()
        .find(|row| row.request_id == fixture.claim.request_id)
        .unwrap();
    let rest = serde_json::to_value(rest).unwrap();
    for field in [
        "status_code",
        "operational_terminal",
        "input_tokens",
        "output_tokens",
        "cost",
    ] {
        assert_eq!(
            json[field], rest[field],
            "settled stream and snapshot {field}"
        );
    }
    let ledger: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ledger_entries WHERE source = $1")
        .bind(fixture.reservation.id.to_string())
        .fetch_one(&fixture.database.pool)
        .await
        .unwrap();
    assert_eq!(ledger, 1);
    drop(archive_gate);
    fixture.close().await;
}
