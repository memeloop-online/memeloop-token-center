use super::*;
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use memeloop_token_center::{
    AppState, api,
    config::{Config, RuntimeRole},
    db::CreateServiceTokenInput,
};
use serde_json::Value;
use tower::ServiceExt;

struct ReconcileFixture {
    _directory: tempfile::TempDir,
    state: AppState,
    pool: AnyPool,
    key: AuthenticatedKey,
    job: Uuid,
    nonce: Uuid,
    tenant: String,
    token: String,
}

impl ReconcileFixture {
    async fn new() -> Self {
        let (directory, database, key, upstream_id, price) = fixture().await;
        let url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("generation.db").display()
        );
        let pool = AnyPool::connect(&url).await.unwrap();
        let tenant: String = sqlx::query_scalar("SELECT external_id FROM tenants WHERE id = $1")
            .bind(key.tenant_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
        let mut config = Config::for_test(url);
        config.key_pepper = String::from_utf8(PEPPER.to_vec()).unwrap();
        let state = AppState::initialize(config).await.unwrap();
        let reservation = reserve(&database, &key, &price).await;
        let job = database
            .create_generation_job(input(&key, upstream_id, reservation, &price))
            .await
            .unwrap()
            .job_id;
        database
            .claim_generation_job("interrupted-worker")
            .await
            .unwrap()
            .unwrap();
        let nonce = Uuid::now_v7();
        database
            .mark_generation_submitting(job, "interrupted-worker", nonce)
            .await
            .unwrap();
        database
            .arm_generation_shutdown_quarantine(job, "interrupted-worker", nonce)
            .await
            .unwrap();
        let token = state
            .db
            .create_service_token(
                CreateServiceTokenInput {
                    name: "tenant-reconciler".into(),
                    scopes: vec![
                        "generations:quarantine:read".into(),
                        "generations:reconcile".into(),
                    ],
                    tenant_external_id: Some(tenant.clone()),
                },
                PEPPER,
            )
            .await
            .unwrap()
            .token;
        Self {
            _directory: directory,
            state,
            pool,
            key,
            job,
            nonce,
            tenant,
            token,
        }
    }

    async fn expire(&self) {
        sqlx::query("UPDATE generation_jobs SET lease_expires_at = 0 WHERE id = $1")
            .bind(self.job.to_string())
            .execute(&self.pool)
            .await
            .unwrap();
    }

    async fn body(&self, action: &str) -> Value {
        let view = self
            .state
            .db
            .generation_quarantine(&self.tenant, self.job)
            .await
            .unwrap();
        let mut body = json!({"tenant_external_id": self.tenant, "action": action,
            "expected_revision": view.revision, "evidence_digest": "ab".repeat(32)});
        if action == "confirmed_submitted" {
            body["upstream_job_id"] = json!("provider-confirmed-123");
        }
        body
    }

    async fn request(
        &self,
        method: &str,
        path: &str,
        token: &str,
        keys: &[&str],
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header("authorization", format!("Bearer {token}"));
        for key in keys {
            request = request.header("idempotency-key", *key);
        }
        let request = request
            .header("content-type", "application/json")
            .body(
                body.map(|value| Body::from(value.to_string()))
                    .unwrap_or_else(Body::empty),
            )
            .unwrap();
        let response = api::router_for_role(self.state.clone(), RuntimeRole::Control)
            .oneshot(request)
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    fn resolution_path(&self) -> String {
        format!(
            "/internal/v1/generations/quarantine/{}/resolutions",
            self.job
        )
    }

    async fn token(&self, scopes: &[&str], tenant: Option<&str>) -> String {
        self.state
            .db
            .create_service_token(
                CreateServiceTokenInput {
                    name: Uuid::now_v7().to_string(),
                    scopes: scopes.iter().map(|s| s.to_string()).collect(),
                    tenant_external_id: tenant.map(str::to_owned),
                },
                PEPPER,
            )
            .await
            .unwrap()
            .token
    }

    async fn assert_reserved(&self) {
        assert_eq!(
            self.state
                .db
                .key_view(&self.key)
                .await
                .unwrap()
                .available_balance,
            "9.75"
        );
        let status: String = sqlx::query_scalar("SELECT r.status FROM usage_reservations r JOIN generation_jobs j ON j.reservation_id = r.id WHERE j.id = $1")
            .bind(self.job.to_string()).fetch_one(&self.pool).await.unwrap();
        assert_eq!(status, "reserved");
    }
}

#[tokio::test]
async fn submitted_resolution_is_atomic_audited_and_exactly_replayable() {
    let f = ReconcileFixture::new().await;
    f.expire().await;
    let body = f.body("confirmed_submitted").await;
    let path = f.resolution_path();
    let (status, receipt) = f
        .request(
            "POST",
            &path,
            &f.token,
            &["confirmation-1"],
            Some(body.clone()),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    assert_eq!(receipt["resulting_status"], "running");
    assert_eq!(receipt["upstream_job_id"], "provider-confirmed-123");
    let (replay_status, replay) = f
        .request(
            "POST",
            &path,
            &f.token,
            &["confirmation-1"],
            Some(body.clone()),
        )
        .await;
    assert_eq!(replay_status, StatusCode::OK);
    assert_eq!(receipt, replay);
    assert_eq!(
        f.request(
            "POST",
            &path,
            &f.token,
            &["another-key"],
            Some(body.clone())
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let mut changed = body;
    changed["evidence_digest"] = json!("cd".repeat(32));
    assert_eq!(
        f.request("POST", &path, &f.token, &["confirmation-1"], Some(changed))
            .await
            .0,
        StatusCode::CONFLICT
    );
    let row = sqlx::query("SELECT action, evidence_digest, result_json FROM generation_quarantine_resolutions WHERE job_id = $1")
        .bind(f.job.to_string()).fetch_one(&f.pool).await.unwrap();
    assert_eq!(row.get::<String, _>("action"), "confirmed_submitted");
    assert_eq!(row.get::<String, _>("evidence_digest"), "ab".repeat(32));
    assert!(
        !row.get::<String, _>("result_json")
            .contains("confirmation-1")
    );
    assert!(
        f.state
            .db
            .mark_generation_submitted(f.job, "interrupted-worker", f.nonce, "late-old-ack")
            .await
            .is_err()
    );
    let claimed = f
        .state
        .db
        .claim_generation_job("normal-poller")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.status, "running");
    assert_eq!(
        claimed.upstream_job_id.as_deref(),
        Some("provider-confirmed-123")
    );
    f.assert_reserved().await;
}

#[tokio::test]
async fn not_submitted_assertion_cannot_release_quarantine_or_reserved_credit() {
    let f = ReconcileFixture::new().await;
    f.expire().await;
    let body = f.body("confirmed_not_submitted").await;
    assert_eq!(
        f.request(
            "POST",
            &f.resolution_path(),
            &f.token,
            &["not-delivered"],
            Some(body)
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let row = sqlx::query("SELECT status, error_code, submission_nonce, upstream_job_id, attempt_count FROM generation_jobs WHERE id = $1")
        .bind(f.job.to_string()).fetch_one(&f.pool).await.unwrap();
    assert_eq!(row.get::<String, _>("status"), "submitting");
    assert_eq!(
        row.get::<Option<String>, _>("error_code").as_deref(),
        Some("shutdown_delivery_unknown")
    );
    assert_eq!(
        row.get::<Option<String>, _>("submission_nonce").as_deref(),
        Some(f.nonce.to_string().as_str())
    );
    assert!(row.get::<Option<String>, _>("upstream_job_id").is_none());
    assert_eq!(row.get::<i64, _>("attempt_count"), 1);
    f.assert_reserved().await;
}

#[tokio::test]
async fn live_lease_stale_revision_and_malformed_confirmation_are_rejected() {
    let f = ReconcileFixture::new().await;
    let live = f.body("confirmed_submitted").await;
    let path = f.resolution_path();
    assert_eq!(
        f.request("POST", &path, &f.token, &["live"], Some(live.clone()))
            .await
            .0,
        StatusCode::CONFLICT
    );
    f.expire().await;
    assert_eq!(
        f.request("POST", &path, &f.token, &["stale"], Some(live))
            .await
            .0,
        StatusCode::CONFLICT
    );
    let valid = f.body("confirmed_submitted").await;
    for keys in [vec![], vec!["one", "two"]] {
        assert_eq!(
            f.request("POST", &path, &f.token, &keys, Some(valid.clone()))
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
    }
    for (field, value) in [
        ("action", json!("unknown")),
        ("evidence_digest", json!("raw provider secret")),
        ("upstream_job_id", json!("https://provider.example/secret")),
        ("unexpected", json!(true)),
    ] {
        let mut invalid = valid.clone();
        invalid[field] = value;
        assert_eq!(
            f.request("POST", &path, &f.token, &["invalid"], Some(invalid))
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
    }
    let mut missing = valid.clone();
    missing.as_object_mut().unwrap().remove("upstream_job_id");
    assert_eq!(
        f.request("POST", &path, &f.token, &["missing"], Some(missing))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    let mut conflicting = valid;
    conflicting["action"] = json!("confirmed_not_submitted");
    assert_eq!(
        f.request("POST", &path, &f.token, &["conflicting"], Some(conflicting))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM generation_quarantine_resolutions")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    f.assert_reserved().await;
}

#[tokio::test]
async fn authorization_tenant_binding_and_safe_metadata_are_enforced() {
    let f = ReconcileFixture::new().await;
    f.expire().await;
    let body = f.body("confirmed_submitted").await;
    let path = f.resolution_path();
    let read = f
        .token(&["generations:quarantine:read"], Some(&f.tenant))
        .await;
    let broad = f.token(&["generations:write"], Some(&f.tenant)).await;
    let global = f.token(&["generations:reconcile"], None).await;
    for token in [&read, &broad, &global, &f.state.config.service_token] {
        assert_eq!(
            f.request("POST", &path, token, &["forbidden"], Some(body.clone()))
                .await
                .0,
            StatusCode::FORBIDDEN
        );
    }
    let mut cross = body.clone();
    cross["tenant_external_id"] = json!("another-tenant");
    assert_eq!(
        f.request("POST", &path, &f.token, &["cross"], Some(cross))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let unknown = format!(
        "/internal/v1/generations/quarantine/{}/resolutions",
        Uuid::now_v7()
    );
    assert_eq!(
        f.request("POST", &unknown, &f.token, &["unknown"], Some(body))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    let list = format!(
        "/internal/v1/generations/quarantine?tenant_external_id={}&limit=1",
        f.tenant
    );
    let (status, page) = f.request("GET", &list, &read, &[], None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page.as_array().unwrap().len(), 1);
    let metadata = page.to_string();
    for forbidden in [
        "request_object",
        "submission_nonce",
        "credential",
        "reservation",
        "upstream_job_id",
    ] {
        assert!(!metadata.contains(forbidden));
    }
    let detail = format!(
        "/internal/v1/generations/quarantine/{}?tenant_external_id={}",
        f.job, f.tenant
    );
    let (status, exact) = f.request("GET", &detail, &read, &[], None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(exact, page[0]);
    f.assert_reserved().await;
}

#[tokio::test]
async fn concurrent_conflicting_decisions_have_one_durable_winner() {
    let f = ReconcileFixture::new().await;
    f.expire().await;
    let submitted = f.body("confirmed_submitted").await;
    let mut conflicting = submitted.clone();
    conflicting["upstream_job_id"] = json!("other-provider-confirmation");
    let path = f.resolution_path();
    let (a, b) = tokio::join!(
        f.request("POST", &path, &f.token, &["race-a"], Some(submitted)),
        f.request("POST", &path, &f.token, &["race-b"], Some(conflicting)),
    );
    let mut statuses = [a.0.as_u16(), b.0.as_u16()];
    statuses.sort_unstable();
    assert_eq!(statuses, [200, 409]);
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM generation_quarantine_resolutions WHERE job_id = $1",
    )
    .bind(f.job.to_string())
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
    f.assert_reserved().await;
}

#[tokio::test]
async fn audit_failure_rolls_back_the_transition_and_does_not_consume_idempotency() {
    let f = ReconcileFixture::new().await;
    f.expire().await;
    let body = f.body("confirmed_submitted").await;
    sqlx::query("CREATE TRIGGER reject_reconcile_audit BEFORE INSERT ON generation_quarantine_resolutions BEGIN SELECT RAISE(ABORT, 'test audit unavailable'); END")
        .execute(&f.pool).await.unwrap();
    let path = f.resolution_path();
    assert!(
        f.request(
            "POST",
            &path,
            &f.token,
            &["audit-retry"],
            Some(body.clone())
        )
        .await
        .0
        .is_server_error()
    );
    assert!(
        f.state
            .db
            .generation_quarantine(&f.tenant, f.job)
            .await
            .is_ok()
    );
    f.assert_reserved().await;
    sqlx::query("DROP TRIGGER reject_reconcile_audit")
        .execute(&f.pool)
        .await
        .unwrap();
    assert_eq!(
        f.request("POST", &path, &f.token, &["audit-retry"], Some(body))
            .await
            .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn old_quarantine_confirmation_gets_a_bounded_poll_window_without_rewriting_admission() {
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };
    let f = ReconcileFixture::new().await;
    f.expire().await;
    let provider = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/history/provider-confirmed-123"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&provider)
        .await;
    sqlx::query("UPDATE upstream_accounts SET config_json = $1 WHERE id = (SELECT upstream_account_id FROM generation_jobs WHERE id = $2)")
        .bind(json!({"base_url": provider.uri()}).to_string()).bind(f.job.to_string()).execute(&f.pool).await.unwrap();
    sqlx::query("UPDATE generation_jobs SET created_at = 0 WHERE id = $1")
        .bind(f.job.to_string())
        .execute(&f.pool)
        .await
        .unwrap();
    let body = f.body("confirmed_submitted").await;
    let path = f.resolution_path();
    let (status, receipt) = f
        .request(
            "POST",
            &path,
            &f.token,
            &["late-confirmation"],
            Some(body.clone()),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let confirmed = receipt["delivery_confirmed_at"].as_i64().unwrap();
    let deadline = receipt["reconciliation_deadline_at"].as_i64().unwrap();
    assert_eq!(deadline - confirmed, 24 * 60 * 60 * 1_000);
    assert!(
        memeloop_token_center::generation::process_one(&f.state, "confirmed-poller")
            .await
            .unwrap()
    );
    let current = f
        .state
        .db
        .generation_job(f.key.key_id, f.job)
        .await
        .unwrap();
    assert_eq!(current.created_at, 0);
    assert_eq!(current.status, "running");
    f.assert_reserved().await;
    // The same receipt never extends the confirmation window after a poll.
    let (_, replay) = f
        .request("POST", &path, &f.token, &["late-confirmation"], Some(body))
        .await;
    assert_eq!(receipt, replay);
    sqlx::query("UPDATE generation_jobs SET reconciliation_deadline_at = 0, next_attempt_at = 0 WHERE id = $1")
        .bind(f.job.to_string()).execute(&f.pool).await.unwrap();
    assert!(
        memeloop_token_center::generation::process_one(&f.state, "deadline-poller")
            .await
            .unwrap()
    );
    let expired = f
        .state
        .db
        .generation_job(f.key.key_id, f.job)
        .await
        .unwrap();
    assert_eq!(expired.status, "failed");
    assert_eq!(expired.error_code.as_deref(), Some("generation_timeout"));
    provider.verify().await;
}

#[tokio::test]
async fn postgres_quarantine_reconciliation_serializes_conflicting_decisions() {
    use memeloop_token_center::db::ResolveGenerationQuarantine;
    let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let f = atomic_generation_fixture(&url).await;
    let pool = AnyPool::connect(&url).await.unwrap();
    let tenant: String = sqlx::query_scalar("SELECT external_id FROM tenants WHERE id = $1")
        .bind(f.key.tenant_id.to_string())
        .fetch_one(&pool)
        .await
        .unwrap();
    let actor = f
        .database
        .create_service_token(
            CreateServiceTokenInput {
                name: Uuid::now_v7().to_string(),
                scopes: vec!["generations:reconcile".into()],
                tenant_external_id: Some(tenant.clone()),
            },
            PEPPER,
        )
        .await
        .unwrap();
    let reservation = reserve(&f.database, &f.key, &f.price).await;
    let mut create = input(&f.key, f.upstream_id, reservation, &f.price);
    create.public_model = f.model.clone();
    let job = f
        .database
        .create_generation_job(create)
        .await
        .unwrap()
        .job_id;
    // Do not use the global claim queue in this shared PostgreSQL CI database.
    sqlx::query("UPDATE generation_jobs SET status = 'submitting', error_code = 'shutdown_delivery_unknown', submission_nonce = $1, lease_expires_at = 0 WHERE id = $2")
        .bind(Uuid::now_v7().to_string()).bind(job.to_string()).execute(&pool).await.unwrap();
    let revision = f
        .database
        .generation_quarantine(&tenant, job)
        .await
        .unwrap()
        .revision;
    let digest_a = "aa".repeat(32);
    let digest_b = "bb".repeat(32);
    let evidence = "cc".repeat(32);
    let decision = |first: bool| ResolveGenerationQuarantine {
        tenant_external_id: &tenant,
        job_id: job,
        actor_service_id: actor.service_id,
        idempotency_hash: if first { &digest_a } else { &digest_b },
        expected_revision: &revision,
        action: "confirmed_submitted",
        upstream_job_id: Some(if first {
            "postgres-provider-id-a"
        } else {
            "postgres-provider-id-b"
        }),
        evidence_digest: &evidence,
    };
    let (a, b) = tokio::join!(
        f.database.resolve_generation_quarantine(decision(true)),
        f.database.resolve_generation_quarantine(decision(false))
    );
    let first_won = a.is_ok();
    let winner = match (a, b) {
        (Ok(result), Err(AppError::Conflict(_))) | (Err(AppError::Conflict(_)), Ok(result)) => {
            result
        }
        other => panic!("expected one winner and one conflict: {other:?}"),
    };
    assert_eq!(
        f.database
            .resolve_generation_quarantine(decision(first_won))
            .await
            .unwrap(),
        winner
    );
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM generation_quarantine_resolutions WHERE job_id = $1",
    )
    .bind(job.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
    assert_eq!(
        f.database.key_view(&f.key).await.unwrap().available_balance,
        "9.75"
    );
    // This CI database is shared with later integration-test binaries, whose
    // real global queue claims must not pick up this fixture's resumed job.
    sqlx::query("UPDATE generation_jobs SET next_attempt_at = $1 WHERE id = $2")
        .bind(i64::MAX)
        .bind(job.to_string())
        .execute(&pool)
        .await
        .unwrap();
}
