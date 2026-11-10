use super::*;
use crate::{
    AppState,
    archive_reaper::ArchiveReaper,
    config::Config,
    conversation::{ConversationHints, SessionBody, session_body_from_documents},
    db::{CreateKeyInput, FinishProxyRequest, ProxyConversationInput, StartProxyRequest},
    model::{AuthenticatedKey, KeyPolicy, ModelPrice, TokenUsage},
};
use bytes::Bytes;
use rust_decimal::Decimal;
use serde_json::{Value, json};

struct Fixture {
    state: AppState,
    key: AuthenticatedKey,
    price: ModelPrice,
    model: String,
}

fn request(text: &str) -> Value {
    json!({"messages": [{"role": "user", "content": [
        {"type": "text", "text": text},
        {"type": "image_url", "image_url": {"url": "data:image/png;base64,AQIDBA=="}}
    ]}]})
}

fn response() -> Value {
    json!({"choices": [{"message": {"role": "assistant", "content": "media answer"}}]})
}

impl Fixture {
    async fn new(database_url: &str) -> Self {
        let state = AppState::initialize(Config::for_test(database_url.to_owned()))
            .await
            .unwrap();
        let model = Uuid::now_v7().to_string();
        let pepper = state.config.key_pepper.as_bytes();
        let issued = state
            .db
            .create_key(
                CreateKeyInput {
                    tenant_external_id: model.clone(),
                    principal_external_id: "media-member".into(),
                    alias: model.clone(),
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
            .upsert_model_price(&model, "USD", Decimal::ONE, Decimal::ONE)
            .await
            .unwrap();
        Self {
            state,
            key,
            price,
            model,
        }
    }

    async fn enqueue(&self, text: &str) -> (Uuid, Uuid) {
        let request_id = Uuid::now_v7();
        let document = request(text);
        for (side, body) in [("request", document.clone()), ("response", response())] {
            let object = self
                .state
                .archive
                .put_tenant_content(
                    self.key.tenant_id,
                    Bytes::from(serde_json::to_vec(&body).unwrap()),
                )
                .await
                .unwrap();
            self.state
                .archive
                .publish_raw_capture(self.key.tenant_id, request_id, side, &object)
                .await
                .unwrap();
        }
        let request_locator = format!("inline-json:{document}");
        let response_locator = format!("inline-json:{}", response());
        let reservation = self
            .state
            .db
            .start_proxy_request(StartProxyRequest {
                request_id,
                key: &self.key,
                price: &self.price,
                input_token_ceiling: 10,
                output_token_ceiling: 10,
                protocol: "openai",
                model: &self.model,
                request_object: &request_locator,
                upstream_account_id: None,
                model_route_id: None,
            })
            .await
            .unwrap();
        let hints = ConversationHints::default();
        self.state
            .db
            .finish_proxy_request_deferred(FinishProxyRequest {
                terminal_cause: None,
                usage_basis: None,
                first_output_ms: None,
                generation_duration_ms: None,
                request_id,
                tenant_id: self.key.tenant_id,
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
                    key: &self.key,
                    request_json: &document,
                    hints: &hints,
                    client_name: None,
                    upstream_response_id: None,
                }),
            })
            .await
            .unwrap();
        let owner = Uuid::now_v7();
        assert!(
            self.state
                .db
                .claim_terminal_projection_tasks(owner, 32)
                .await
                .unwrap()
                .contains(&request_id)
        );
        assert!(
            self.state
                .db
                .project_claimed_terminal_projection_task(owner, request_id)
                .await
                .unwrap()
        );
        sqlx::query("UPDATE conversation_projection_outbox SET terminal_lease_expires_at = 0 WHERE request_id = $1")
            .bind(request_id.to_string()).execute(&self.state.db.pool).await.unwrap();
        let owner = Uuid::now_v7();
        assert!(
            self.state
                .db
                .claim_conversation_projection_tasks(owner, 32)
                .await
                .unwrap()
                .iter()
                .any(|task| task.request_id == request_id)
        );
        (request_id, owner)
    }

    async fn stage(
        &self,
        request_id: Uuid,
        text: &str,
    ) -> (SessionBody, ArchiveStagingWriteLease, String) {
        let mut body = session_body_from_documents(&request(text), &response());
        let (turn, part, bytes) = body.take_media_bytes().pop().unwrap();
        let hash = blake3::hash(&bytes).to_hex().to_string();
        let attempt = crate::proxy_lifecycle::begin_proxy_archive_attempt(
            &self.state.db,
            request_id,
            crate::archive_staging::ArchiveStagingPurpose::Media,
        )
        .await
        .unwrap();
        let locator =
            crate::archive_staging::conversation_media_locator(attempt.lease.key, &hash).unwrap();
        self.state
            .archive
            .put(&locator, Bytes::from(bytes))
            .await
            .unwrap();
        body.set_media_locator(turn, part, locator.clone());
        (body, attempt.lease, locator)
    }

    async fn leaf(&self, request_id: Uuid) -> String {
        sqlx::query_scalar(
            "SELECT leaf_node_hash FROM conversation_observations WHERE request_id = $1",
        )
        .bind(request_id.to_string())
        .fetch_one(&self.state.db.pool)
        .await
        .unwrap()
    }

    async fn retry_cleanup(&self) {
        sqlx::query("UPDATE archive_staging_attempts SET next_cleanup_at = created_at WHERE owner_id IN (SELECT request_id FROM conversation_projection_outbox WHERE tenant_id = $1) AND purpose = 'media' AND state = 'cleanup_pending'")
            .bind(self.key.tenant_id.to_string()).execute(&self.state.db.pool).await.unwrap();
        ArchiveReaper::new(
            self.state.db.clone(),
            self.state.archive.clone(),
            ArchiveStagingLeaseOwner::new("media-test-reaper").unwrap(),
        )
        .reap_once()
        .await
        .unwrap();
    }
}

async fn shared_media_contract(database_url: &str) {
    let fixture = Fixture::new(database_url).await;
    let mut leaves = Vec::new();
    let mut locators = Vec::new();
    for text in ["same turn", "same turn", "another turn"] {
        let (request_id, owner) = fixture.enqueue(text).await;
        assert!(
            crate::worker::archive_session_body(&fixture.state, owner, request_id)
                .await
                .unwrap()
        );
        assert!(
            !fixture
                .state
                .db
                .project_claimed_conversation_projection_task(owner, request_id)
                .await
                .unwrap()
        );
        leaves.push(fixture.leaf(request_id).await);
        let attempts: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM archive_staging_attempts WHERE owner_id = $1 AND purpose = 'media'",
        )
        .bind(request_id.to_string())
        .fetch_one(&fixture.state.db.pool)
        .await
        .unwrap();
        assert_eq!(attempts, 0, "new streamed media use immutable tenant CAS");
        let turns = fixture
            .state
            .db
            .export_training_dialogue(fixture.key.tenant_id, leaves.last().unwrap())
            .await
            .unwrap();
        let locator = turns[0].parts[1]["object_locator"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(crate::archive::is_tenant_cas_location(
            fixture.key.tenant_id,
            &locator
        ));
        locators.push(locator);
    }
    assert_eq!(leaves[0], leaves[1]);
    assert_ne!(leaves[0], leaves[2]);
    let references: Vec<String> = sqlx::query_scalar(
        "SELECT object_locator FROM conversation_media_parts WHERE tenant_id = $1",
    )
    .bind(fixture.key.tenant_id.to_string())
    .fetch_all(&fixture.state.db.pool)
    .await
    .unwrap();
    assert_eq!(references, vec![locators[0].clone(), locators[0].clone()]);
    for leaf in &leaves {
        let turns = fixture
            .state
            .db
            .export_training_dialogue(fixture.key.tenant_id, leaf)
            .await
            .unwrap();
        assert_eq!(turns.last().unwrap().role, "assistant");
        let media = &turns[0].parts[1];
        assert_eq!(
            media["content_blake3"],
            blake3::hash(&[1, 2, 3, 4]).to_hex().to_string()
        );
        assert_eq!(media["object_locator"], locators[0]);
        assert_eq!(
            fixture
                .state
                .archive
                .get(media["object_locator"].as_str().unwrap())
                .await
                .unwrap(),
            Bytes::from_static(&[1, 2, 3, 4])
        );
    }
    fixture.retry_cleanup().await;
    assert_eq!(locators[0], locators[1]);
    assert_eq!(locators[0], locators[2]);
    assert!(fixture.state.archive.get(&locators[1]).await.is_ok());
    assert!(fixture.state.archive.get(&locators[2]).await.is_ok());
    assert!(fixture.state.archive.get(&locators[0]).await.is_ok());
    sqlx::query("DELETE FROM conversation_media_parts WHERE tenant_id = $1 AND turn_id = (SELECT turn_id FROM conversation_media_parts WHERE tenant_id = $1 ORDER BY turn_id LIMIT 1)")
        .bind(fixture.key.tenant_id.to_string()).execute(&fixture.state.db.pool).await.unwrap();
    fixture.retry_cleanup().await;
    assert!(fixture.state.archive.get(&locators[0]).await.is_ok());
    sqlx::query("DELETE FROM conversation_media_parts WHERE tenant_id = $1")
        .bind(fixture.key.tenant_id.to_string())
        .execute(&fixture.state.db.pool)
        .await
        .unwrap();
    fixture.retry_cleanup().await;
    assert!(
        fixture.state.archive.get(&locators[0]).await.is_ok(),
        "durable training media cannot be retired by cache-reference cleanup"
    );
}

async fn fenced_media_contract(database_url: &str) {
    for expire_writer in [false, true] {
        let mut fixture = Fixture::new(database_url).await;
        let (request_id, owner) = fixture.enqueue("fenced media").await;
        let (body, lease, locator) = fixture.stage(request_id, "fenced media").await;
        if expire_writer {
            sqlx::query(
                "UPDATE archive_staging_attempts SET lease_expires_at = 0 WHERE attempt_id = $1",
            )
            .bind(lease.key.attempt_id.to_string())
            .execute(&fixture.state.db.pool)
            .await
            .unwrap();
        } else {
            sqlx::query("UPDATE conversation_projection_outbox SET terminal_lease_owner = $1 WHERE request_id = $2")
                .bind(Uuid::now_v7().to_string()).bind(request_id.to_string()).execute(&fixture.state.db.pool).await.unwrap();
        }
        assert!(
            !fixture
                .state
                .db
                .prepare_staged_session_body_archive(
                    owner,
                    request_id,
                    body,
                    std::slice::from_ref(&lease)
                )
                .await
                .unwrap()
        );
        let references: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM conversation_media_parts WHERE tenant_id = $1",
        )
        .bind(fixture.key.tenant_id.to_string())
        .fetch_one(&fixture.state.db.pool)
        .await
        .unwrap();
        assert_eq!(references, 0);
        assert_eq!(
            fixture
                .state
                .db
                .archive_staging_attempt(lease.key.attempt_id)
                .await
                .unwrap()
                .unwrap()
                .state,
            ArchiveStagingState::Writing
        );
        fixture.state.db = Database::connect_with_max(database_url, 4).await.unwrap();
        sqlx::query(
            "UPDATE archive_staging_attempts SET lease_expires_at = 0 WHERE attempt_id = $1",
        )
        .bind(lease.key.attempt_id.to_string())
        .execute(&fixture.state.db.pool)
        .await
        .unwrap();
        assert_eq!(
            fixture
                .state
                .db
                .promote_stale_archive_staging_attempts()
                .await
                .unwrap(),
            1
        );
        fixture.retry_cleanup().await;
        assert!(fixture.state.archive.get(&locator).await.is_err());
    }
}

async fn concurrent_media_contract(database_url: &str) {
    let fixture = Fixture::new(database_url).await;
    let (first, first_owner) = fixture.enqueue("concurrent media").await;
    let (second, second_owner) = fixture.enqueue("concurrent media").await;
    let (first_result, second_result) = tokio::join!(
        crate::worker::archive_session_body(&fixture.state, first_owner, first),
        crate::worker::archive_session_body(&fixture.state, second_owner, second),
    );
    assert!(first_result.unwrap());
    assert!(second_result.unwrap());
    assert_eq!(fixture.leaf(first).await, fixture.leaf(second).await);
    let references: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM conversation_media_parts WHERE tenant_id = $1")
            .bind(fixture.key.tenant_id.to_string())
            .fetch_one(&fixture.state.db.pool)
            .await
            .unwrap();
    assert_eq!(references, 1);
}

async fn media_reference_rollback_contract(database_url: &str) {
    let fixture = Fixture::new(database_url).await;
    let (request_id, owner) = fixture.enqueue("media transaction rollback").await;
    let (body, lease, locator) = fixture
        .stage(request_id, "media transaction rollback")
        .await;
    let atoms_before: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM semantic_atoms WHERE tenant_id = $1")
            .bind(fixture.key.tenant_id.to_string())
            .fetch_one(&fixture.state.db.pool)
            .await
            .unwrap();
    match fixture.state.db.backend {
        DatabaseBackend::Sqlite => {
            sqlx::query("CREATE TRIGGER reject_media_part BEFORE INSERT ON conversation_media_parts BEGIN SELECT RAISE(ABORT, 'injected media reference failure'); END")
                .execute(&fixture.state.db.pool).await.unwrap();
        }
        DatabaseBackend::PostgreSql => {
            sqlx::query("CREATE FUNCTION reject_media_part() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected media reference failure'; END $$")
                .execute(&fixture.state.db.pool).await.unwrap();
            sqlx::query("CREATE TRIGGER reject_media_part BEFORE INSERT ON conversation_media_parts FOR EACH ROW EXECUTE FUNCTION reject_media_part()")
                .execute(&fixture.state.db.pool).await.unwrap();
        }
    }
    assert!(
        fixture
            .state
            .db
            .prepare_staged_session_body_archive(
                owner,
                request_id,
                body.clone(),
                std::slice::from_ref(&lease)
            )
            .await
            .is_err()
    );
    let attempt = fixture
        .state
        .db
        .archive_staging_attempt(lease.key.attempt_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(attempt.state, ArchiveStagingState::Writing);
    assert!(attempt.bound_at.is_none());
    assert!(attempt.bound_locator.is_none());
    let references: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM conversation_media_parts WHERE tenant_id = $1")
            .bind(fixture.key.tenant_id.to_string())
            .fetch_one(&fixture.state.db.pool)
            .await
            .unwrap();
    let atoms_after: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM semantic_atoms WHERE tenant_id = $1")
            .bind(fixture.key.tenant_id.to_string())
            .fetch_one(&fixture.state.db.pool)
            .await
            .unwrap();
    assert_eq!(references, 0);
    assert_eq!(atoms_before, atoms_after);
    assert!(fixture.state.archive.get(&locator).await.is_ok());
    match fixture.state.db.backend {
        DatabaseBackend::Sqlite => {
            sqlx::query("DROP TRIGGER reject_media_part")
                .execute(&fixture.state.db.pool)
                .await
                .unwrap();
        }
        DatabaseBackend::PostgreSql => {
            sqlx::query("DROP TRIGGER reject_media_part ON conversation_media_parts")
                .execute(&fixture.state.db.pool)
                .await
                .unwrap();
            sqlx::query("DROP FUNCTION reject_media_part()")
                .execute(&fixture.state.db.pool)
                .await
                .unwrap();
        }
    }
    assert!(
        fixture
            .state
            .db
            .prepare_staged_session_body_archive(
                owner,
                request_id,
                body,
                std::slice::from_ref(&lease)
            )
            .await
            .unwrap()
    );
    assert_eq!(
        fixture
            .state
            .db
            .archive_staging_attempt(lease.key.attempt_id)
            .await
            .unwrap()
            .unwrap()
            .state,
        ArchiveStagingState::CleanupPending
    );
    assert!(
        fixture
            .state
            .db
            .project_claimed_conversation_projection_task(owner, request_id)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn sqlite_media_lifecycle() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("media.db").display()
    );
    shared_media_contract(&database_url).await;
    fenced_media_contract(&database_url).await;
    concurrent_media_contract(&database_url).await;
    media_reference_rollback_contract(&database_url).await;
}

#[tokio::test]
async fn postgres_media_lifecycle() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let admin = sqlx::PgPool::connect(&database_url).await.unwrap();
    let schema = format!("media_lifecycle_{}", Uuid::now_v7().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    let mut isolated = url::Url::parse(&database_url).unwrap();
    isolated
        .query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema}"));
    let database_url = isolated.to_string();
    shared_media_contract(&database_url).await;
    fenced_media_contract(&database_url).await;
    concurrent_media_contract(&database_url).await;
    media_reference_rollback_contract(&database_url).await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}
