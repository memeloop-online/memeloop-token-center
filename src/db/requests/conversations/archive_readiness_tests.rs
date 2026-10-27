use super::*;
use crate::{
    AppState,
    config::Config,
    response_archive_spool::{self, BufferedArchivePurpose},
};
use bytes::Bytes;

struct Fixture {
    state: AppState,
    request_id: Uuid,
    identity: ArchiveSpoolIdentity,
}

impl Fixture {
    async fn new(database_url: String, inline_request: bool) -> Self {
        let state = AppState::initialize(Config::for_test(database_url))
            .await
            .unwrap();
        let unique = Uuid::now_v7().to_string();
        let pepper = state.config.key_pepper.as_bytes();
        let issued = state
            .db
            .create_key(
                CreateKeyInput {
                    tenant_external_id: unique.clone(),
                    principal_external_id: "member".into(),
                    alias: unique.clone(),
                    currency: "USD".into(),
                    policy: KeyPolicy::default(),
                    initial_balance: Decimal::TEN,
                    idempotency_key: None,
                },
                pepper,
            )
            .await
            .unwrap();
        let key = state
            .db
            .authenticate_key(&issued.key, pepper)
            .await
            .unwrap();
        let price = state
            .db
            .upsert_model_price(&unique, "USD", Decimal::ONE, Decimal::ONE)
            .await
            .unwrap();
        let request_id = Uuid::now_v7();
        let request_locator = format!("gap://{request_id}/request");
        let response_locator = format!("gap://{request_id}/response");
        let reservation = state
            .db
            .start_proxy_request(StartProxyRequest {
                request_id,
                key: &key,
                price: &price,
                input_token_ceiling: 10,
                output_token_ceiling: 10,
                protocol: "openai",
                model: &unique,
                request_object: &request_locator,
                upstream_account_id: None,
                model_route_id: None,
            })
            .await
            .unwrap();
        let request = if inline_request {
            serde_json::json!({"messages": [{"role": "user", "content": "archive question"}]})
        } else {
            serde_json::json!({})
        };
        let hints = ConversationHints::default();
        state
            .db
            .finish_proxy_request_deferred(FinishProxyRequest {
                terminal_cause: None,
                usage_basis: None,
                first_output_ms: None,
                generation_duration_ms: None,
                request_id,
                tenant_id: key.tenant_id,
                reservation: &reservation,
                input_token_ceiling: 10,
                output_token_ceiling: 10,
                requested_service_tier: None,
                status_code: 200,
                duration_ms: 10,
                usage: TokenUsage {
                    input_tokens: 1,
                    output_tokens: 1,
                    ..TokenUsage::default()
                },
                error_code: None,
                response_object: &response_locator,
                routing_session_id: None,
                routing_terminal_observed_at: None,
                conversation: Some(ProxyConversationInput {
                    key: &key,
                    request_json: &request,
                    hints: &hints,
                    client_name: None,
                    upstream_response_id: None,
                }),
            })
            .await
            .unwrap();
        let terminal_owner = Uuid::now_v7();
        assert!(
            state
                .db
                .claim_terminal_projection_tasks(terminal_owner, 32)
                .await
                .unwrap()
                .contains(&request_id)
        );
        assert!(
            state
                .db
                .project_claimed_terminal_projection_task(terminal_owner, request_id)
                .await
                .unwrap()
        );
        let identity = ArchiveSpoolIdentity {
            request_id,
            tenant_id: key.tenant_id,
            reservation_id: reservation.id,
        };
        Self {
            state,
            request_id,
            identity,
        }
    }

    async fn claim(&self) -> Uuid {
        sqlx::query("UPDATE conversation_projection_outbox SET terminal_lease_expires_at = 0 WHERE request_id = $1")
            .bind(self.request_id.to_string()).execute(&self.state.db.pool).await.unwrap();
        let owner = Uuid::now_v7();
        assert!(
            self.state
                .db
                .claim_conversation_projection_tasks(owner, 32)
                .await
                .unwrap()
                .iter()
                .any(|task| task.request_id == self.request_id)
        );
        owner
    }

    async fn assert_deferred(&self) -> Uuid {
        let owner = self.claim().await;
        assert!(
            !crate::worker::archive_session_body(&self.state, owner, self.request_id)
                .await
                .unwrap()
        );
        let row = sqlx::query("SELECT lease_owner, lease_expires_at, terminal_lease_owner, terminal_lease_expires_at, projected_at, statistics_outcome FROM conversation_projection_outbox WHERE request_id = $1")
            .bind(self.request_id.to_string()).fetch_one(&self.state.db.pool).await.unwrap();
        assert_eq!(row.get::<String, _>("lease_owner"), "terminal-v118");
        assert_eq!(row.get::<i64, _>("lease_expires_at"), i64::MAX);
        assert!(
            row.get::<Option<String>, _>("terminal_lease_owner")
                .is_none()
        );
        assert!(row.get::<i64, _>("terminal_lease_expires_at") <= unix_millis() + 5000);
        assert!(row.get::<Option<i64>, _>("projected_at").is_none());
        assert_ne!(
            row.get::<Option<String>, _>("statistics_outcome")
                .as_deref(),
            Some("session_body")
        );
        let observations: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM conversation_observations WHERE request_id = $1",
        )
        .bind(self.request_id.to_string())
        .fetch_one(&self.state.db.pool)
        .await
        .unwrap();
        assert_eq!(observations, 0);
        assert!(
            self.state
                .db
                .claim_conversation_projection_tasks(Uuid::now_v7(), 32)
                .await
                .unwrap()
                .is_empty()
        );
        owner
    }

    async fn capture(&self) {
        for (purpose, body) in [
            (BufferedArchivePurpose::Request, Bytes::from_static(br#"{"messages":[{"role":"user","content":"archive question"}]}"#)),
            (BufferedArchivePurpose::Response, Bytes::from_static(br#"{"choices":[{"message":{"role":"assistant","content":"archive assistant answer"}}]}"#)),
        ] {
            assert!(response_archive_spool::capture_buffered(&self.state, self.identity, purpose, body).await);
        }
    }

    async fn assert_completed(&self, assistant: bool) {
        let owner = self.claim().await;
        assert!(
            crate::worker::archive_session_body(&self.state, owner, self.request_id)
                .await
                .unwrap()
        );
        assert!(
            !crate::worker::archive_session_body(&self.state, owner, self.request_id)
                .await
                .unwrap()
        );
        let leaf: String = sqlx::query_scalar(
            "SELECT leaf_node_hash FROM conversation_observations WHERE request_id = $1",
        )
        .bind(self.request_id.to_string())
        .fetch_one(&self.state.db.pool)
        .await
        .unwrap();
        let turns = self
            .state
            .db
            .export_training_dialogue(self.identity.tenant_id, &leaf)
            .await
            .unwrap();
        let encoded = turns
            .iter()
            .flat_map(|turn| &turn.parts)
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(encoded.contains("archive question"), "{encoded}");
        assert_eq!(
            encoded.contains("archive assistant answer"),
            assistant,
            "{encoded}"
        );
        let events: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_events WHERE request_id = $1 AND event_kind = 'projected'")
            .bind(self.request_id.to_string()).fetch_one(&self.state.db.pool).await.unwrap();
        assert_eq!(events, 1);
    }
}

async fn readiness_contract(database_url: String) {
    let fixture = Fixture::new(database_url, false).await;
    let stale_owner = fixture.assert_deferred().await;
    let replacement = fixture.claim().await;
    fixture
        .state
        .db
        .defer_session_body_archive(stale_owner, fixture.request_id)
        .await
        .unwrap();
    let stored_owner: String = sqlx::query_scalar(
        "SELECT terminal_lease_owner FROM conversation_projection_outbox WHERE request_id = $1",
    )
    .bind(fixture.request_id.to_string())
    .fetch_one(&fixture.state.db.pool)
    .await
    .unwrap();
    assert_eq!(stored_owner, replacement.to_string());
    assert!(
        !crate::worker::archive_session_body(&fixture.state, replacement, fixture.request_id)
            .await
            .unwrap()
    );
    fixture.capture().await;
    fixture.assert_deferred().await;
    for state in ["capturing", "uploading"] {
        sqlx::query("UPDATE response_archive_spools SET state = $1, attempts = 10, lease_expires_at = $2 WHERE request_id = $3")
            .bind(state).bind(unix_millis() + 60_000).bind(fixture.request_id.to_string())
            .execute(&fixture.state.db.pool).await.unwrap();
        fixture.assert_deferred().await;
    }
    sqlx::query("UPDATE response_archive_spools SET state = 'pending', attempts = 0, lease_expires_at = NULL WHERE request_id = $1")
        .bind(fixture.request_id.to_string()).execute(&fixture.state.db.pool).await.unwrap();
    assert!(response_archive_spool::process_one_for_test(&fixture.state).await);
    fixture.assert_deferred().await;
    assert!(response_archive_spool::process_one_for_test(&fixture.state).await);
    fixture.assert_completed(true).await;
}

#[tokio::test]
async fn sqlite_session_body_waits_for_capture_and_both_uploads() {
    let directory = tempfile::tempdir().unwrap();
    readiness_contract(format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("readiness.db").display()
    ))
    .await;
}

#[tokio::test]
async fn sqlite_final_archive_gap_keeps_request_turn() {
    let directory = tempfile::tempdir().unwrap();
    let fixture = Fixture::new(
        format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("gap.db").display()
        ),
        true,
    )
    .await;
    fixture.capture().await;
    sqlx::query("UPDATE request_archive_spools SET state = 'gap' WHERE request_id = $1")
        .bind(fixture.request_id.to_string())
        .execute(&fixture.state.db.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE response_archive_spools SET state = 'gap' WHERE request_id = $1")
        .bind(fixture.request_id.to_string())
        .execute(&fixture.state.db.pool)
        .await
        .unwrap();
    fixture.assert_completed(false).await;
}

#[tokio::test]
async fn sqlite_missing_capture_expires_without_acknowledging_early() {
    let directory = tempfile::tempdir().unwrap();
    let fixture = Fixture::new(
        format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("missing.db").display()
        ),
        true,
    )
    .await;
    fixture.assert_deferred().await;
    sqlx::query("UPDATE request_records SET completed_at = $1 WHERE id = $2")
        .bind(unix_millis() - super::super::super::archive_spool::CAPTURE_TTL - 1)
        .bind(fixture.request_id.to_string())
        .execute(&fixture.state.db.pool)
        .await
        .unwrap();
    fixture.assert_completed(false).await;
}

#[tokio::test]
async fn postgres_session_body_waits_for_capture_and_both_uploads() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let admin = sqlx::PgPool::connect(&database_url).await.unwrap();
    let schema = format!("archive_readiness_{}", Uuid::now_v7().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    let mut isolated = url::Url::parse(&database_url).unwrap();
    isolated
        .query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema}"));
    readiness_contract(isolated.to_string()).await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}
