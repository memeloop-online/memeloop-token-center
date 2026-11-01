//! Task-local transport/barrier seams only. The test runs the real durable
//! confirm, shared cache flight, normalization and fenced database effects.
use super::*;
use crate::{
    db::{
        CreateKeyInput, CreateModelRouteInput, CreateUpstreamAccountInput, RouteSelectionOptions,
        SupplierQuotaEvidence,
    },
    model::KeyPolicy,
    provider::ResolvedUpstream,
};
use sqlx::Row;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use tokio::sync::Notify;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

struct FlightFixture {
    base: String,
    evicted: Notify,
    resume_reset: Notify,
    owner_ready: Notify,
    resume_owner: Notify,
    waiter: Notify,
    started_at: AtomicI64,
    pause_owner: AtomicBool,
}

tokio::task_local! {
    static FIXTURE: Arc<FlightFixture>;
}

pub(super) fn fixture_url(url: &str) -> Option<String> {
    FIXTURE
        .try_with(|fixture| {
            format!(
                "{}{}",
                fixture.base,
                url.strip_prefix("https://chatgpt.com").unwrap()
            )
        })
        .ok()
}

pub(super) fn observation_started(at: i64) {
    let _ = FIXTURE.try_with(|fixture| fixture.started_at.store(at, Ordering::SeqCst));
}

pub(super) fn waiting_for_flight(trigger: QuotaReadTrigger) {
    if trigger == QuotaReadTrigger::ResetWorkflow {
        let _ = FIXTURE.try_with(|fixture| fixture.waiter.notify_one());
    }
}

pub(super) async fn before_publish() {
    if let Ok(fixture) = FIXTURE.try_with(Arc::clone)
        && fixture.pause_owner.swap(false, Ordering::SeqCst)
    {
        fixture.owner_ready.notify_one();
        fixture.resume_owner.notified().await;
    }
}

pub(super) async fn after_reset_eviction() {
    if let Ok(fixture) = FIXTURE.try_with(Arc::clone) {
        fixture.evicted.notify_one();
        fixture.resume_reset.notified().await;
    }
}

#[tokio::test]
async fn accepted_reset_coalesced_nonreset_owner_applies_supplier_start_fence() {
    tokio::time::timeout(Duration::from_secs(30), async {
        for relevant_usage in [true, false] {
            concurrent_reset(relevant_usage).await;
        }
    })
    .await
    .unwrap();
}

async fn concurrent_reset(relevant_usage: bool) {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/backend-api/wham/rate-limit-reset-credits"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "available_count": 1,
            "credits": [{"reset_type":"codex_rate_limits", "status":"available",
                "granted_at":"2020-01-01T00:00:00Z", "expires_at":"2099-01-01T00:00:00Z"}]
        })))
        .expect(3)
        .mount(&server)
        .await;
    let usage = if relevant_usage {
        json!({"rate_limit":{"allowed":true,"limit_reached":false,"primary_window":{"used_percent":0}}})
    } else {
        json!({"code_review_rate_limit":{"allowed":true,"limit_reached":false,"primary_window":{"used_percent":0}},
            "credits":{"has_credits":true}})
    };
    Mock::given(method("GET"))
        .and(path("/backend-api/wham/usage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(usage))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/backend-api/wham/rate-limit-reset-credits/consume"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    let hooks = Arc::new(FlightFixture {
        base: server.uri(),
        evicted: Notify::new(),
        resume_reset: Notify::new(),
        owner_ready: Notify::new(),
        resume_owner: Notify::new(),
        waiter: Notify::new(),
        started_at: AtomicI64::new(0),
        pause_owner: AtomicBool::new(true),
    });
    FIXTURE.scope(hooks.clone(), async {
        let directory = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}?mode=rwc", directory.path().join("reset-flight.db").display());
        let state = AppState::initialize(crate::config::Config::for_test(url.clone())).await.unwrap();
        let pool = sqlx::AnyPool::connect(&url).await.unwrap();
        let tenant = "r2-reset-flight";
        let credential = UpstreamCredential::OAuth {
            access_token: "fixture-only".into(), refresh_token: None, expires_at: None,
            header: "authorization".into(), prefix: "Bearer ".into(),
            adapter_state: Some(json!({"schema":"openai-codex-oauth-v1","account_id":"fixture-account"})),
            proxy_url: None, proxy_network_scope: None,
        };
        let account = state.db.create_upstream_account(CreateUpstreamAccountInput {
            tenant_external_id: tenant.into(), name: "flight".into(), driver: "openai-codex".into(),
            config: json!({"base_url":"https://chatgpt.com/backend-api/codex"}),
            credential: credential.clone(), oauth_session_id: None, oauth_driver: None, oauth_refresh_url: None,
        }, state.config.key_pepper.as_bytes()).await.unwrap();
        let route_id = state.db.create_model_route(CreateModelRouteInput {
            tenant_external_id: tenant.into(), public_model: "exact".into(), upstream_model: "exact".into(),
            upstream_account_id: account.id, protocol: "openai".into(), priority: 0,
        }).await.unwrap().id;
        let key = state.db.create_key(CreateKeyInput {
            tenant_external_id: tenant.into(), principal_external_id: "billing".into(), alias: "billing".into(),
            currency: "USD".into(), policy: KeyPolicy::default(), initial_balance: rust_decimal::Decimal::ZERO,
            idempotency_key: None,
        }, state.config.key_pepper.as_bytes()).await.unwrap();
        let key = state.db.authenticate_key(&key.key, state.config.key_pepper.as_bytes()).await.unwrap();
        sqlx::query("INSERT INTO routing_grants (tenant_id,key_id,model_route_id,created_at) VALUES ($1,$2,$3,1)")
            .bind(account.tenant_id.to_string()).bind(key.key_id.to_string()).bind(route_id.to_string())
            .execute(&pool).await.unwrap();
        let route = ResolvedUpstream {
            route_id, account_id: account.id, upstream_model: "exact".into(),
            credential_generation: account.credential_generation, transport_revision: account.updated_at,
            driver: account.driver.clone(), base_url: "https://chatgpt.com/backend-api/codex".into(),
            config: account.config.clone(), credential: credential.clone(),
        };
        let before = unix_millis() - 1000;
        let deadline = before + 3_600_000;
        let evidence = SupplierQuotaEvidence { exhausted: true, reset_at: Some(deadline), reset_source: "retry_after" };
        for model in ["exact", "unrelated"] {
            let mut target = route.clone();
            target.upstream_model = model.into();
            state.db.store_supplier_quota(&target, Uuid::now_v7(), before, evidence).await.unwrap();
        }
        // Stale identity rows are deliberately present alongside the live row.
        for (generation, revision) in [(account.credential_generation + 1, account.updated_at), (account.credential_generation, account.updated_at + 1)] {
            sqlx::query("INSERT INTO upstream_supplier_availability
                (upstream_account_id,upstream_model,credential_generation,transport_revision,failure_kind,reset_at,reset_source,observed_at,request_id,hard_observed_at,reset_observed_at)
                VALUES ($1,'exact',$2,$3,'quota_exhausted',$4,'retry_after',$5,'stale',$5,$5)")
                .bind(account.id.to_string()).bind(generation).bind(revision).bind(deadline).bind(before)
                .execute(&pool).await.unwrap();
        }
        let prepared = serde_json::to_value(reset::prepare(&state, &account, &credential, tenant, "actor", "prepare").await.unwrap()).unwrap();
        let operation = prepared["operation"]["id"].as_str().unwrap().to_owned();
        let token = prepared["confirmation_token"].as_str().unwrap().to_owned();
        let reset_task = {
            let (state, account, credential, hooks, operation, token) =
                (state.clone(), account.clone(), credential.clone(), hooks.clone(), operation.clone(), token.clone());
            tokio::spawn(FIXTURE.scope(hooks, async move {
                reset::confirm(&state, &account, &credential, tenant, "actor", &operation, &token, "confirm").await.unwrap()
            }))
        };
        hooks.evicted.notified().await;
        let owner_task = {
            let (state, account, credential, hooks) = (state.clone(), account.clone(), credential.clone(), hooks.clone());
            tokio::spawn(FIXTURE.scope(hooks, async move {
                state.upstream_quota.read_fresh(&state, &account, &credential, tenant, QuotaReadTrigger::Manual).await
            }))
        };
        hooks.owner_ready.notified().await;
        let started = hooks.started_at.load(Ordering::SeqCst);
        assert!(started > before);
        // Both failures predate waiter completion; only the actual supplier
        // observation start protects them from accidental permission.
        for (model, at) in [("equal", started), ("newer", started + 1)] {
            let mut target = route.clone();
            target.upstream_model = model.into();
            state.db.store_supplier_quota(&target, Uuid::now_v7(), at, evidence).await.unwrap();
        }
        hooks.resume_reset.notify_one();
        hooks.waiter.notified().await;
        assert!(!owner_task.is_finished());
        assert!(!reset_task.is_finished());
        tokio::time::sleep(Duration::from_millis(3)).await;
        assert!(unix_millis() > started + 1);
        hooks.resume_owner.notify_one();
        let owner = owner_task.await.unwrap();
        assert!(!owner.cache_hit);
        assert_eq!(owner.error_code, None);
        assert_eq!(owner.verification_observation.as_ref().unwrap().started_at, started);
        let operation_result = reset_task.await.unwrap();
        assert_eq!(operation_result.state, "accepted");
        // Replay traverses the durable replay branch without any supplier read/POST.
        let replay = reset::confirm(&state, &account, &credential, tenant, "actor", &operation, &token, "confirm").await.unwrap();
        assert_eq!(replay.state, operation_result.state);
        let rows = sqlx::query("SELECT * FROM upstream_supplier_availability WHERE upstream_account_id=$1")
            .bind(account.id.to_string()).fetch_all(&pool).await.unwrap();
        for row in rows {
            let model = row.get::<String, _>("upstream_model");
            let matching = row.get::<i64, _>("credential_generation") == account.credential_generation
                && row.get::<i64, _>("transport_revision") == account.updated_at;
            let permission = (relevant_usage && matching && matches!(model.as_str(), "exact" | "unrelated")).then_some(started);
            assert_eq!(row.get::<Option<i64>, _>("verification_after"), permission, "{model}");
            assert_eq!(row.get::<String, _>("failure_kind"), "quota_exhausted", "{model}");
            assert_eq!(row.get::<Option<i64>, _>("reset_at"), Some(deadline));
        }
        assert_eq!(state.db.supplier_quota_probe_deadline(account.id, "exact").await.unwrap(), (!relevant_usage).then_some(deadline));
        let recovery = state.db.supplier_quota_recovery_models(account.id).await.unwrap();
        assert_eq!(recovery, if relevant_usage { vec!["exact".to_owned(), "unrelated".to_owned()] } else { vec![] });
        let selection = RouteSelectionOptions { upstream_account_hint: Some(account.id), avoid_route_account: None, selection_seed: Uuid::now_v7() };
        assert!(state.db.list_authorized_upstream_candidates_with_hint(key.key_id, key.tenant_id, "exact", "openai", selection).await.unwrap().is_empty());
        state.db.recover_supplier_quota_from_probe(account.id, &json!({
            "status":"healthy","upstream_status":200,"actual_model":"exact",
            "credential_generation":account.credential_generation,"transport_revision":account.updated_at,
            "started_at":started + 2,"request_id":Uuid::now_v7()
        })).await.unwrap();
        assert_eq!(state.db.list_authorized_upstream_candidates_with_hint(key.key_id, key.tenant_id, "exact", "openai", selection).await.unwrap().len(), 1);
        let unrelated: String = sqlx::query_scalar("SELECT failure_kind FROM upstream_supplier_availability WHERE upstream_account_id=$1 AND upstream_model='unrelated'")
            .bind(account.id.to_string()).fetch_one(&pool).await.unwrap();
        assert_eq!(unrelated, "quota_exhausted");
        assert_eq!(server.received_requests().await.unwrap().iter().filter(|request| request.method.as_str() == "POST").count(), 1);
        server.verify().await;
        pool.close().await;
    }).await;
}
