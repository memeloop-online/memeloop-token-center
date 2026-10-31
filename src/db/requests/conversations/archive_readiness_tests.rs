use super::*;
use crate::{
    AppState,
    config::Config,
    db::test_support::PostgresTestDatabase,
    response_archive_spool::{self, BufferedArchive, BufferedArchivePurpose},
};
use bytes::Bytes;

struct Fixture {
    state: AppState,
    request_id: Uuid,
    identity: ArchiveSpoolIdentity,
    source: Option<std::sync::Arc<crate::gateway_body::request_spool::RequestSpool>>,
}

impl Fixture {
    async fn new(database_url: String, inline_request: bool) -> Self {
        Self::new_with_protocol(database_url, inline_request, None).await
    }

    async fn new_with_protocol(
        database_url: String,
        inline_request: bool,
        protocol: Option<&str>,
    ) -> Self {
        Self::new_with_source(database_url, inline_request, protocol, None).await
    }

    async fn new_with_source(
        database_url: String,
        inline_request: bool,
        protocol: Option<&str>,
        admission: Option<crate::gateway_body::request_spool::RequestSpoolAdmission>,
    ) -> Self {
        let mut state = AppState::initialize(Config::for_test(database_url))
            .await
            .unwrap();
        if let Some(admission) = admission {
            state.responses_request_spool = admission;
        }
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
        let memory = state.proxy_memory_budget.reservation();
        let source = if protocol.is_some() {
            let body = match protocol {
                Some("openai-responses") => Bytes::from_static(br#"{"input":[{"role":"user","content":[{"type":"input_text","text":"archive question"}]}],"provider_secret_field":"kept only in original object"}"#),
                _ => Bytes::from_static(br#"{"messages":[{"role":"user","content":"archive question"}],"provider_secret_field":"kept only in original object"}"#),
            };
            Some(std::sync::Arc::new(
                state
                    .responses_request_spool
                    .capture_with_memory(
                        axum::body::Body::from(body.clone()),
                        body.len(),
                        Some(body.len()),
                        Some(&memory),
                        0,
                        Some(&request_id.to_string()),
                    )
                    .await
                    .unwrap(),
            ))
        } else {
            None
        };
        let request_locator = source
            .as_ref()
            .and_then(|source| source.object_locator())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("gap://{request_id}/request"));
        let response_locator = if source.is_some() {
            let object = state.archive.put_tenant_content(
                key.tenant_id,
                Bytes::from_static(br#"{"choices":[{"message":{"role":"assistant","content":"archive assistant answer"}}]}"#),
            ).await.unwrap();
            state
                .archive
                .publish_raw_capture(key.tenant_id, request_id, "response", &object)
                .await
                .unwrap();
            object.object_locator
        } else {
            format!("gap://{request_id}/response")
        };
        let input = StartProxyRequest {
            request_id,
            key: &key,
            price: &price,
            input_token_ceiling: 10,
            output_token_ceiling: 10,
            protocol: protocol.unwrap_or("openai"),
            model: &unique,
            request_object: &request_locator,
            upstream_account_id: None,
            model_route_id: None,
        };
        let reservation = if let Some(source) = source.as_ref() {
            let held = state.db.saturate_gateway_persistence_for_test().await;
            let admitted = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                state
                    .db
                    .start_proxy_request_with_source(input, source, None, Some(10)),
            )
            .await
            .expect("source admission must not wait for the old archive queue")
            .unwrap();
            drop(held);
            admitted.reservation
        } else {
            state.db.start_proxy_request(input).await.unwrap()
        };
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
            source,
        }
    }

    async fn claim(&self) -> Uuid {
        sqlx::query("UPDATE conversation_projection_outbox SET terminal_lease_expires_at = 0 WHERE request_id = $1")
            .bind(self.request_id.to_string()).execute(&self.state.db.pool).await.unwrap();
        self.claim_ready().await
    }

    async fn claim_ready(&self) -> Uuid {
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
            let archive = BufferedArchive::new(
                self.identity,
                purpose,
                &body,
                self.state.config.key_pepper.as_bytes(),
                self.state.config.archive_spool_compression_enabled,
            )
            .unwrap();
            let mut transaction = self.state.db.begin_write_transaction().await.unwrap();
            assert!(
                self.state
                    .db
                    .capture_buffered_archive_body_in_transaction(
                        &mut transaction, unix_millis(), &archive, None,
                    )
                    .await
                    .unwrap()
            );
            transaction.commit().await.unwrap();
        }
    }

    async fn assert_completed(&self, assistant: bool) {
        let owner = self.claim().await;
        self.assert_completed_by(owner, assistant).await;
    }

    async fn assert_completed_by(&self, owner: Uuid, assistant: bool) {
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

async fn pending_archive_wake_contract(database_url: String) {
    for bind_before_defer in [true, false] {
        let fixture = Fixture::new(database_url.clone(), false).await;
        let owner = fixture.claim_ready().await;
        let pending = fixture
            .state
            .db
            .session_body_locators(fixture.request_id)
            .await
            .unwrap()
            .unwrap();
        assert!(pending.pending);
        fixture.capture().await;
        if bind_before_defer {
            // The worker already read pending; both bind wakes now encounter
            // its active claim and must leave that claim intact.
            assert!(response_archive_spool::process_one_for_test(&fixture.state).await);
            assert!(response_archive_spool::process_one_for_test(&fixture.state).await);
            let stored_owner: String = sqlx::query_scalar(
                "SELECT terminal_lease_owner FROM conversation_projection_outbox WHERE request_id = $1",
            )
            .bind(fixture.request_id.to_string())
            .fetch_one(&fixture.state.db.pool)
            .await
            .unwrap();
            assert_eq!(stored_owner, owner.to_string());
            fixture
                .state
                .db
                .defer_pending_session_body_archive(owner, fixture.request_id)
                .await
                .unwrap();
        } else {
            let before = unix_millis();
            assert!(
                !crate::worker::archive_session_body(&fixture.state, owner, fixture.request_id)
                    .await
                    .unwrap()
            );
            let row = sqlx::query("SELECT lease_owner, lease_expires_at, terminal_lease_owner, terminal_lease_expires_at FROM conversation_projection_outbox WHERE request_id = $1")
                .bind(fixture.request_id.to_string()).fetch_one(&fixture.state.db.pool).await.unwrap();
            assert_eq!(row.get::<String, _>("lease_owner"), "terminal-v118");
            assert_eq!(row.get::<i64, _>("lease_expires_at"), i64::MAX);
            assert!(
                row.get::<Option<String>, _>("terminal_lease_owner")
                    .is_none()
            );
            let expires_at = row.get::<i64, _>("terminal_lease_expires_at");
            assert!(expires_at >= before + 5000);
            assert!(expires_at <= unix_millis() + 5000);
            assert!(
                fixture
                    .state
                    .db
                    .claim_conversation_projection_tasks(Uuid::now_v7(), 32)
                    .await
                    .unwrap()
                    .is_empty(),
                "still-pending archives must retain their retry backoff"
            );
            assert!(response_archive_spool::process_one_for_test(&fixture.state).await);
            assert!(response_archive_spool::process_one_for_test(&fixture.state).await);
        }
        assert!(
            !fixture
                .state
                .db
                .session_body_locators(fixture.request_id)
                .await
                .unwrap()
                .unwrap()
                .pending
        );
        let row = sqlx::query("SELECT lease_owner, lease_expires_at, terminal_lease_owner, terminal_lease_expires_at FROM conversation_projection_outbox WHERE request_id = $1")
            .bind(fixture.request_id.to_string()).fetch_one(&fixture.state.db.pool).await.unwrap();
        assert_eq!(row.get::<String, _>("lease_owner"), "terminal-v118");
        assert_eq!(row.get::<i64, _>("lease_expires_at"), i64::MAX);
        assert!(
            row.get::<Option<String>, _>("terminal_lease_owner")
                .is_none()
        );
        assert!(
            row.get::<Option<i64>, _>("terminal_lease_expires_at")
                .is_none()
        );
        // Neither claim_ready nor assert_completed_by changes lease deadlines:
        // reclaim must succeed immediately after the real archive binds.
        let replacement = fixture.claim_ready().await;
        let expires_at: i64 = sqlx::query_scalar(
            "SELECT terminal_lease_expires_at FROM conversation_projection_outbox WHERE request_id = $1",
        )
        .bind(fixture.request_id.to_string())
        .fetch_one(&fixture.state.db.pool)
        .await
        .unwrap();
        fixture
            .state
            .db
            .defer_pending_session_body_archive(owner, fixture.request_id)
            .await
            .unwrap();
        let row = sqlx::query("SELECT terminal_lease_owner, terminal_lease_expires_at FROM conversation_projection_outbox WHERE request_id = $1")
            .bind(fixture.request_id.to_string()).fetch_one(&fixture.state.db.pool).await.unwrap();
        assert_eq!(
            row.get::<String, _>("terminal_lease_owner"),
            replacement.to_string()
        );
        assert_eq!(row.get::<i64, _>("terminal_lease_expires_at"), expires_at);
        fixture.assert_completed_by(replacement, true).await;
        let receipts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conversation_projection_outbox WHERE request_id = $1 AND projected_at IS NOT NULL AND statistics_outcome = 'session_body' AND COALESCE(semantic_payload_snapshot_json, semantic_snapshot_json) IS NOT NULL")
            .bind(fixture.request_id.to_string()).fetch_one(&fixture.state.db.pool).await.unwrap();
        assert_eq!(receipts, 1);
        let observations: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM conversation_observations WHERE request_id = $1",
        )
        .bind(fixture.request_id.to_string())
        .fetch_one(&fixture.state.db.pool)
        .await
        .unwrap();
        assert_eq!(observations, 1);
    }
}

#[tokio::test]
async fn sqlite_pending_archive_defer_preserves_bind_wakes_and_claim_ownership() {
    let directory = tempfile::tempdir().unwrap();
    pending_archive_wake_contract(format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("pending-wake.db").display()
    ))
    .await;
}

#[tokio::test]
async fn postgres_pending_archive_defer_preserves_bind_wakes_and_claim_ownership() {
    let database_url = std::env::var("MTC_TEST_POSTGRES_URL")
        .expect("pending archive wake PostgreSQL regression must not silently skip");
    // Projection advisory locks are database scoped, so a private schema
    // still shares contention with other PostgreSQL regression tests.
    let isolated = PostgresTestDatabase::create(&database_url).await;
    pending_archive_wake_contract(isolated.url().to_owned()).await;
    isolated.cleanup().await;
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
    // This fixture knows the original request. Persist those exact bytes;
    // the empty SQL outbox cannot stand in for a missing original.
    let object = fixture
        .state
        .archive
        .put_tenant_content(
            fixture.identity.tenant_id,
            Bytes::from_static(br#"{"messages":[{"role":"user","content":"archive question"}]}"#),
        )
        .await
        .unwrap();
    fixture
        .state
        .archive
        .publish_raw_capture(
            fixture.identity.tenant_id,
            fixture.request_id,
            "request",
            &object,
        )
        .await
        .unwrap();
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
async fn sqlite_missing_capture_never_becomes_final_from_elapsed_time() {
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
    fixture.assert_deferred().await;
}

#[tokio::test]
async fn postgres_session_body_waits_for_capture_and_both_uploads() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    // Projection advisory locks are database scoped, so a private schema
    // still shares contention with other PostgreSQL regression tests.
    let isolated = PostgresTestDatabase::create(&database_url).await;
    readiness_contract(isolated.url().to_owned()).await;
    isolated.cleanup().await;
}

async fn source_contract(database_url: String) {
    for protocol in ["openai", "anthropic", "openai-responses"] {
        let mut fixture =
            Fixture::new_with_protocol(database_url.clone(), true, Some(protocol)).await;
        let source = fixture.source.take().unwrap();
        assert!(source.upload_outcome().await);
        let locator = source.object_locator().unwrap().to_owned();
        let original = fixture.state.archive.get(&locator).await.unwrap();
        source.release_retained();
        drop(source);
        assert!(String::from_utf8_lossy(&original).contains("provider_secret_field"));
        fixture
            .state
            .db
            .record_request_source(
                fixture.identity,
                &locator,
                original.len(),
                *blake3::hash(&original).as_bytes(),
                true,
            )
            .await
            .unwrap();
        sqlx::query("DELETE FROM request_archive_spools WHERE request_id = $1")
            .bind(fixture.request_id.to_string())
            .execute(&fixture.state.db.pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM archive_staging_attempts WHERE attempt_id = $1")
            .bind(fixture.request_id.to_string())
            .execute(&fixture.state.db.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE request_records SET completed_at = $1 WHERE id = $2")
            .bind(unix_millis() - 24 * 60 * 60 * 1000)
            .bind(fixture.request_id.to_string())
            .execute(&fixture.state.db.pool)
            .await
            .unwrap();
        fixture.state.db = Database::connect_with_max(&database_url, 4)
            .await
            .unwrap()
            .with_archive(fixture.state.archive.clone());
        fixture.assert_completed(true).await;
        let root = fixture
            .state
            .archive
            .raw_capture_reference(fixture.identity.tenant_id, fixture.request_id, "request")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(root.size_bytes, original.len() as u64);
        assert_eq!(
            root.blake3_digest,
            blake3::hash(&original).to_hex().to_string()
        );
        assert_eq!(
            fixture
                .state
                .archive
                .get_tenant_content(fixture.identity.tenant_id, &root, original.len())
                .await
                .unwrap(),
            original
        );
        let recreated: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM request_archive_spools WHERE request_id = $1")
                .bind(fixture.request_id.to_string())
                .fetch_one(&fixture.state.db.pool)
                .await
                .unwrap();
        assert_eq!(
            recreated, 0,
            "immutable roots restore content without recreating retired spools"
        );
        let chunks: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM request_archive_spool_chunks WHERE request_id = $1",
        )
        .bind(fixture.request_id.to_string())
        .fetch_one(&fixture.state.db.pool)
        .await
        .unwrap();
        assert_eq!(chunks, 0);
        let leaf: String = sqlx::query_scalar(
            "SELECT leaf_node_hash FROM conversation_observations WHERE request_id = $1",
        )
        .bind(fixture.request_id.to_string())
        .fetch_one(&fixture.state.db.pool)
        .await
        .unwrap();
        let turns = fixture
            .state
            .db
            .export_training_dialogue(fixture.identity.tenant_id, &leaf)
            .await
            .unwrap();
        assert!(
            turns
                .iter()
                .flat_map(|turn| &turn.parts)
                .all(|part| !part.to_string().contains("provider_secret_field"))
        );
    }
}

async fn failed_source_metadata_contract(database_url: String) {
    use crate::gateway_body::request_spool::{MemoryObjectStore, RequestSpoolAdmission};
    use std::time::Duration;

    let store = MemoryObjectStore::new();
    store.fail_writes_for_test();
    let release = store.pause_next_write();
    let admission =
        RequestSpoolAdmission::new(std::path::PathBuf::new()).with_memory_store(store.clone());
    let mut fixture =
        Fixture::new_with_source(database_url, true, Some("openai"), Some(admission)).await;
    let source = fixture.source.take().unwrap();
    let locator = source.object_locator().unwrap().to_owned();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let capturing: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_archive_spools WHERE request_id = $1 AND state = 'capturing'")
                .bind(fixture.request_id.to_string()).fetch_one(&fixture.state.db.pool).await.unwrap();
            if capturing == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    }).await.expect("the real admitted request must have a live source writer");
    fixture.assert_deferred().await;
    let held = fixture
        .state
        .db
        .hold_gateway_archive_connection_for_test()
        .await;
    let before = fixture.state.db.request_source_write_failures_for_test();
    release.send(()).unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(300), source.upload_outcome())
            .await
            .is_err()
    );
    assert!(fixture.state.proxy_memory_budget.snapshot().0 > 0);
    store.resume_writes_for_test(fixture.state.archive.clone());
    assert!(source.upload_outcome().await);
    source.release_retained();
    drop(source);
    tokio::time::timeout(Duration::from_secs(5), async {
        while fixture.state.db.request_source_write_failures_for_test() == before
            || fixture.state.proxy_memory_budget.snapshot().0 != 0
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("completed Put releases raw bytes while its metadata retries");
    let pending: String =
        sqlx::query_scalar("SELECT state FROM request_archive_spools WHERE request_id = $1")
            .bind(fixture.request_id.to_string())
            .fetch_one(&fixture.state.db.pool)
            .await
            .unwrap();
    assert_eq!(pending, "capturing");
    drop(held);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let state: String = sqlx::query_scalar(
                "SELECT state FROM request_archive_spools WHERE request_id = $1",
            )
            .bind(fixture.request_id.to_string())
            .fetch_one(&fixture.state.db.pool)
            .await
            .unwrap();
            if state == "bound" {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the retained outcome must retry without a replacement object");
    let original = fixture.state.archive.get(&locator).await.unwrap();
    assert!(
        std::str::from_utf8(&original)
            .unwrap()
            .contains("provider_secret_field")
    );
    fixture.assert_completed(true).await;
}

async fn expired_live_source_writer_contract(database_url: String) {
    use crate::gateway_body::request_spool::{MemoryObjectStore, RequestSpoolAdmission};
    use std::time::Duration;

    let store = MemoryObjectStore::new();
    let release = store.pause_next_write();
    let admission =
        RequestSpoolAdmission::new(std::path::PathBuf::new()).with_memory_store(store.clone());
    let mut fixture =
        Fixture::new_with_source(database_url, true, Some("openai"), Some(admission)).await;
    let source = fixture.source.take().unwrap();
    let locator = source.object_locator().unwrap().to_owned();
    let length = source.len();
    let digest = source.digest();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let capturing: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_archive_spools WHERE request_id = $1 AND state = 'capturing'")
                .bind(fixture.request_id.to_string()).fetch_one(&fixture.state.db.pool).await.unwrap();
            if capturing == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.expect("the admitted source must register before its metadata outage");
    let held = fixture
        .state
        .db
        .hold_gateway_archive_connection_for_test()
        .await;
    let before = fixture.state.db.request_source_write_failures_for_test();
    sqlx::query("UPDATE archive_staging_attempts SET lease_expires_at = 0 WHERE attempt_id = $1 AND state = 'writing'")
        .bind(fixture.request_id.to_string()).execute(&fixture.state.db.pool).await.unwrap();
    drop(source);
    let charged = fixture.state.proxy_memory_budget.snapshot().0;
    assert!(
        charged > 0,
        "the pending Put still owns the raw bytes after request drop"
    );
    tokio::time::timeout(Duration::from_secs(30), async {
        while fixture.state.db.request_source_write_failures_for_test() == before {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the live writer must encounter metadata unavailability");
    assert!(
        !release.is_closed(),
        "metadata failure must retain the paused Put"
    );
    assert_eq!(fixture.state.proxy_memory_budget.snapshot().0, charged);
    drop(held);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let expiry: i64 = sqlx::query_scalar("SELECT lease_expires_at FROM archive_staging_attempts WHERE attempt_id = $1 AND state = 'writing'")
                .bind(fixture.request_id.to_string()).fetch_one(&fixture.state.db.pool).await.unwrap();
            if expiry > unix_millis() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.expect("the original writer must renew its expired lease before takeover");
    assert!(
        !release.is_closed(),
        "renewal must keep the original Put alive"
    );
    assert_eq!(
        fixture.state.proxy_memory_budget.snapshot().0,
        charged,
        "renewal must not release the raw budget before Put"
    );
    assert!(
        !fixture
            .state
            .db
            .recover_request_source(fixture.identity, &locator)
            .await
            .unwrap(),
        "recovery must defer to the renewed live writer"
    );
    store.resume_writes_for_test(fixture.state.archive.clone());
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let state: String = sqlx::query_scalar(
                "SELECT state FROM request_archive_spools WHERE request_id = $1",
            )
            .bind(fixture.request_id.to_string())
            .fetch_one(&fixture.state.db.pool)
            .await
            .unwrap();
            if state == "bound" && fixture.state.proxy_memory_budget.snapshot().0 == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the original Put must bind and release its raw budget");
    let original = fixture.state.archive.get(&locator).await.unwrap();
    assert_eq!(original.len(), length);
    assert_eq!(*blake3::hash(&original).as_bytes(), digest);
    assert!(
        std::str::from_utf8(&original)
            .unwrap()
            .contains("provider_secret_field")
    );
    fixture.assert_completed(true).await;
}

async fn lost_source_writer_contract(database_url: String) {
    use crate::gateway_body::request_spool::{MemoryObjectStore, RequestSpoolAdmission};
    use std::time::Duration;

    let store = MemoryObjectStore::new();
    let release = store.pause_next_write();
    let admission = RequestSpoolAdmission::new(std::path::PathBuf::new()).with_memory_store(store);
    let mut fixture =
        Fixture::new_with_source(database_url.clone(), true, Some("openai"), Some(admission)).await;
    let source = fixture.source.take().unwrap();
    let locator = source.object_locator().unwrap().to_owned();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let live: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM archive_staging_attempts WHERE attempt_id = $1 AND state = 'writing'")
                .bind(fixture.request_id.to_string()).fetch_one(&fixture.state.db.pool).await.unwrap();
            if live == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    sqlx::query("UPDATE request_records SET completed_at = $1 WHERE id = $2")
        .bind(unix_millis() - 24 * 60 * 60 * 1000)
        .bind(fixture.request_id.to_string())
        .execute(&fixture.state.db.pool)
        .await
        .unwrap();
    fixture.assert_deferred().await;
    assert!(
        !fixture
            .state
            .db
            .recover_request_source(fixture.identity, &locator)
            .await
            .unwrap()
    );
    source.abort_writer_for_test().await;
    drop(source);
    assert!(release.send(()).is_err());
    sqlx::query("DELETE FROM request_archive_spools WHERE request_id = $1")
        .bind(fixture.request_id.to_string())
        .execute(&fixture.state.db.pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM archive_staging_attempts WHERE attempt_id = $1")
        .bind(fixture.request_id.to_string())
        .execute(&fixture.state.db.pool)
        .await
        .unwrap();
    assert!(
        !fixture
            .state
            .db
            .recover_request_source(fixture.identity, &locator)
            .await
            .unwrap(),
        "missing registration must establish a fence, not declare an immediate gap"
    );
    sqlx::query("UPDATE archive_staging_attempts SET lease_expires_at = 0 WHERE attempt_id = $1 AND state = 'writing'")
        .bind(fixture.request_id.to_string()).execute(&fixture.state.db.pool).await.unwrap();
    fixture.state.db = Database::connect_with_max(&database_url, 4).await.unwrap();
    assert!(fixture.state.archive.get(&locator).await.is_err());
    fixture.assert_completed(true).await;
    fixture
        .state
        .db
        .complete_request_source(
            fixture.identity,
            &locator,
            2,
            *blake3::hash(b"{}").as_bytes(),
            true,
        )
        .await;
    let state: String =
        sqlx::query_scalar("SELECT state FROM request_archive_spools WHERE request_id = $1")
            .bind(fixture.request_id.to_string())
            .fetch_one(&fixture.state.db.pool)
            .await
            .unwrap();
    assert_eq!(state, "gap", "a fenced writer cannot resurrect the source");
}

#[tokio::test]
async fn sqlite_source_failed_state_write_retries_and_lost_writer_converges() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("source-failures.db").display()
    );
    failed_source_metadata_contract(database_url.clone()).await;
    expired_live_source_writer_contract(database_url.clone()).await;
    lost_source_writer_contract(database_url).await;
}

#[tokio::test]
async fn postgres_source_failed_state_write_retries_and_lost_writer_converges() {
    let database_url = std::env::var("MTC_TEST_POSTGRES_URL")
        .expect("source R2 PostgreSQL regression must not silently skip");
    // Projection advisory locks are database scoped, so a private schema
    // still shares contention with other PostgreSQL regression tests.
    let isolated = PostgresTestDatabase::create(&database_url).await;
    failed_source_metadata_contract(isolated.url().to_owned()).await;
    expired_live_source_writer_contract(isolated.url().to_owned()).await;
    lost_source_writer_contract(isolated.url().to_owned()).await;
    isolated.cleanup().await;
}

#[tokio::test]
async fn sqlite_source_identity_recovers_after_request_drop_and_missing_binding() {
    let directory = tempfile::tempdir().unwrap();
    source_contract(format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("source-recovery.db").display()
    ))
    .await;
}

#[tokio::test]
async fn postgres_source_identity_recovers_after_request_drop_and_missing_binding() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    // Projection advisory locks are database scoped, so a private schema
    // still shares contention with other PostgreSQL regression tests.
    let isolated = PostgresTestDatabase::create(&database_url).await;
    source_contract(isolated.url().to_owned()).await;
    isolated.cleanup().await;
}
