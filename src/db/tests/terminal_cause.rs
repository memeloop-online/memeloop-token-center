use super::super::*;

#[tokio::test]
async fn sqlite_terminal_cause_is_allowlisted_and_first_terminal_writer_wins() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("terminal-cause.db").display()
    );
    assert_terminal_cause_contract(&database_url).await;
}

#[tokio::test]
async fn postgres_terminal_cause_is_allowlisted_and_first_terminal_writer_wins() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    assert_terminal_cause_contract(&database_url).await;
}

async fn assert_terminal_cause_contract(database_url: &str) {
    let database = Database::connect(database_url).await.unwrap();
    database.migrate().await.unwrap();
    let unique = Uuid::now_v7();
    let pepper = b"terminal cause contract pepper";
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: format!("terminal-cause-{unique}"),
                principal_external_id: "member".to_owned(),
                alias: "terminal-cause".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy::default(),
                initial_balance: Decimal::from(10),
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
    let model = format!("terminal-cause-{unique}");
    let price = database
        .upsert_model_price(&model, "USD", Decimal::ONE, Decimal::ONE)
        .await
        .unwrap();

    use crate::model::RequestTerminalCause;
    for (error_code, terminal_cause, expected) in [
        (
            Some("http_502"),
            Some(RequestTerminalCause::Http2Reset),
            Some("upstream_http2_reset"),
        ),
        (
            Some("http_502"),
            Some(RequestTerminalCause::Http2GoAway),
            Some("upstream_http2_goaway"),
        ),
        (
            Some("http_502"),
            Some(RequestTerminalCause::ReadTimeout),
            Some("upstream_read_timeout"),
        ),
        (Some("upstream_http2_reset"), None, None),
        (Some("http_502"), None, None),
        (Some("Bearer untrusted-diagnostic"), None, None),
        (Some("transport_http2_reset_delivery_unknown"), None, None),
        (None, None, None),
    ] {
        let request_id = Uuid::now_v7();
        let reservation = database
            .start_proxy_request(StartProxyRequest {
                request_id,
                key: &key,
                price: &price,
                input_token_ceiling: 1,
                output_token_ceiling: 1,
                protocol: "openai-responses",
                model: &model,
                request_object: "gap://terminal-cause/request",
                upstream_account_id: None,
                model_route_id: None,
            })
            .await
            .unwrap();
        let pending = database
            .request_archive_refs(key.key_id, request_id)
            .await
            .unwrap();
        assert_eq!(pending.view.terminal_cause_code, None);
        let finish = |code, cause| FinishProxyRequest {
            terminal_cause: cause,
            usage_basis: Some(crate::model::RequestUsageBasis::NotObserved),
            first_output_ms: None,
            generation_duration_ms: None,
            request_id,
            tenant_id: key.tenant_id,
            reservation: &reservation,
            input_token_ceiling: 1,
            output_token_ceiling: 1,
            requested_service_tier: None,
            status_code: if error_code.is_some() { 502 } else { 200 },
            duration_ms: 1,
            usage: TokenUsage::default(),
            error_code: code,
            response_object: "gap://terminal-cause/response",
            routing_session_id: None,
            routing_terminal_observed_at: None,
            conversation: None,
        };
        assert!(matches!(
            database
                .finish_proxy_request(finish(error_code, terminal_cause))
                .await
                .unwrap(),
            FinishProxyRequestResult::Finished { .. }
        ));
        assert!(matches!(
            database
                .finish_proxy_request(finish(
                    Some("upstream_request_timeout"),
                    Some(RequestTerminalCause::RequestTimeout)
                ))
                .await
                .unwrap(),
            FinishProxyRequestResult::AlreadyFinished { .. }
        ));
        let stored: Option<String> =
            sqlx::query_scalar("SELECT terminal_cause_code FROM request_records WHERE id = $1")
                .bind(request_id.to_string())
                .fetch_one(&database.pool)
                .await
                .unwrap();
        assert_eq!(stored.as_deref(), expected);
        let detail = database
            .request_archive_refs(key.key_id, request_id)
            .await
            .unwrap();
        assert_eq!(detail.view.terminal_cause_code.as_deref(), expected);
        assert_eq!(detail.view.error_code.as_deref(), error_code);
        let serialized = serde_json::to_value(&detail.view).unwrap();
        assert_eq!(
            serialized["terminal_cause_code"],
            serde_json::json!(expected)
        );
        let rows = database.list_requests(key.key_id, 100).await.unwrap();
        let row = rows
            .iter()
            .find(|row| row.request_id == request_id)
            .unwrap();
        assert_eq!(row.terminal_cause_code.as_deref(), expected);
        let events = database
            .request_events_after(&format!("terminal-cause-{unique}"), 0, None, 100)
            .await
            .unwrap();
        let finished: Vec<_> = events
            .iter()
            .filter(|event| event.request_id == request_id && event.event_kind == "finished")
            .collect();
        assert_eq!(finished.len(), 1);
        assert_eq!(finished[0].terminal_cause_code.as_deref(), expected);
        let serialized = serde_json::to_value(finished[0]).unwrap();
        assert_eq!(
            serialized["terminal_cause_code"],
            serde_json::json!(expected)
        );
        let unlinked = database
            .logical_session_detail(
                key.tenant_id,
                key.key_id,
                &format!("unlinked:{}", key.key_id),
                ConversationDetailFilter {
                    limit: 100,
                    before_created_at: None,
                    before_request_id: None,
                },
            )
            .await
            .unwrap();
        let row = unlinked
            .requests
            .iter()
            .find(|row| row.request.request_id == request_id)
            .unwrap();
        assert_eq!(row.request.terminal_cause_code.as_deref(), expected);
        let cluster_id = database
            .record_conversation_observation(
                &key,
                request_id,
                &serde_json::json!({"input": "terminal cause evidence"}),
                &ConversationHints {
                    session_id: Some(request_id.to_string()),
                    ..Default::default()
                },
                Some("codex"),
            )
            .await
            .unwrap();
        let linked = database
            .conversation_cluster_detail(
                key.key_id,
                cluster_id,
                ConversationDetailFilter {
                    limit: 100,
                    before_created_at: None,
                    before_request_id: None,
                },
            )
            .await
            .unwrap();
        let row = linked
            .requests
            .iter()
            .find(|row| row.request.request_id == request_id)
            .unwrap();
        assert_eq!(row.request.terminal_cause_code.as_deref(), expected);
    }
}

#[tokio::test]
async fn terminal_cause_upgrade_preserves_unknown_historical_causes() {
    use super::super::migrations::{SQLITE_MIGRATIONS, apply_migration_range};

    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("upgrade.db").display()
    );
    let database = Database::connect(&database_url).await.unwrap();
    sqlx::raw_sql(
        "CREATE TABLE schema_migrations (version BIGINT PRIMARY KEY, name TEXT NOT NULL, applied_at BIGINT NOT NULL);
         CREATE TABLE request_records (id TEXT PRIMARY KEY, error_code TEXT);
         INSERT INTO request_records VALUES ('legacy-reset', 'upstream_http2_reset'), ('legacy-success', NULL);",
    ).execute(&database.pool).await.unwrap();
    let mut transaction = database.pool.begin().await.unwrap();
    apply_migration_range(&mut transaction, SQLITE_MIGRATIONS, 115, 115)
        .await
        .unwrap();
    apply_migration_range(&mut transaction, SQLITE_MIGRATIONS, 115, 115)
        .await
        .unwrap();
    transaction.commit().await.unwrap();
    let rows =
        sqlx::query("SELECT error_code, terminal_cause_code FROM request_records ORDER BY id")
            .fetch_all(&database.pool)
            .await
            .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0].get::<String, _>("error_code"),
        "upstream_http2_reset"
    );
    for row in rows {
        assert_eq!(row.get::<Option<String>, _>("terminal_cause_code"), None);
    }
}
