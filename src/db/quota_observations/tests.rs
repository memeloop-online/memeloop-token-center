use super::*;
use crate::{
    AppState, config::Config, db::UpstreamFailureKind, provider::UpstreamCredential,
    upstream_quota::observations::RoutingQuotaWindow,
};
use serde_json::json;

#[tokio::test]
async fn sqlite_quota_observations_include_fenced_codex_recovery_and_opt_in_groups() {
    let directory = tempfile::tempdir().unwrap();
    verify(Config::for_test(format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("quota.db").display()
    )))
    .await;
}

#[tokio::test]
async fn postgres_quota_observations_include_fenced_codex_recovery_and_opt_in_groups() {
    if let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") {
        verify(Config::for_test(url)).await;
    }
}

async fn verify(config: Config) {
    let state = AppState::initialize(config).await.unwrap();
    let db = &state.db;
    let tenant = format!("quota-test-{}", Uuid::now_v7());
    let key = db
        .create_key(
            CreateKeyInput {
                tenant_external_id: tenant.clone(),
                principal_external_id: "principal".into(),
                alias: "key".into(),
                currency: "USD".into(),
                policy: KeyPolicy::default(),
                initial_balance: Decimal::TEN,
                idempotency_key: None,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let auth = db
        .authenticate_key(&key.key, state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    let account = db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: tenant.clone(),
                name: "account".into(),
                driver: "http-json".into(),
                config: json!({"base_url":"http://127.0.0.1:18081","network_scope":"private"}),
                credential: UpstreamCredential::None,
                oauth_session_id: None,
                oauth_driver: None,
                oauth_refresh_url: None,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let route = db
        .create_model_route(CreateModelRouteInput {
            tenant_external_id: tenant.clone(),
            public_model: "quota-model".into(),
            upstream_account_id: account.id,
            upstream_model: "quota-model".into(),
            protocol: "openai".into(),
            priority: 0,
        })
        .await
        .unwrap();

    let recovering = db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: tenant.clone(),
                name: "recovering-codex".into(),
                driver: "http-json".into(),
                config: json!({"base_url":"http://127.0.0.1:18082","network_scope":"private"}),
                credential: UpstreamCredential::None,
                oauth_session_id: None,
                oauth_driver: None,
                oauth_refresh_url: None,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    sqlx::query("UPDATE upstream_accounts SET driver='openai-codex' WHERE id=$1")
        .bind(recovering.id.to_string())
        .execute(&db.pool)
        .await
        .unwrap();
    db.record_upstream_account_failure(
        recovering.id,
        recovering.credential_generation,
        UpstreamFailureKind::RateLimitedUntil {
            until: 10_000,
            exhausted: true,
        },
    )
    .await
    .unwrap();
    let recovery_targets = db
        .quota_observation_targets(&[], &[], 100, 4)
        .await
        .unwrap();
    assert_eq!(recovery_targets.len(), 1);
    assert_eq!(recovery_targets[0].account_id, recovering.id);
    assert!(recovery_targets[0].recovering_quota);
    let health_updated_at: i64 = sqlx::query_scalar(
        "SELECT updated_at FROM upstream_account_health WHERE upstream_account_id=$1",
    )
    .bind(recovering.id.to_string())
    .fetch_one(&db.pool)
    .await
    .unwrap();
    let prior_attempt = health_updated_at + 1;
    sqlx::query("INSERT INTO upstream_quota_observations (upstream_account_id,tenant_id,credential_generation,config_revision,last_attempt_at,next_refresh_at) VALUES ($1,$2,$3,$4,$5,$6)")
        .bind(recovering.id.to_string()).bind(recovering.tenant_id.to_string())
        .bind(recovering.credential_generation).bind(recovering.updated_at)
        .bind(prior_attempt).bind(prior_attempt + 300_000)
        .execute(&db.pool).await.unwrap();
    assert!(
        db.quota_observation_targets(&[], &[], prior_attempt + 1, 4)
            .await
            .unwrap()
            .is_empty(),
        "an existing recovery episode respects its persisted retry backoff"
    );
    sqlx::query("UPDATE upstream_account_health SET updated_at=$1 WHERE upstream_account_id=$2")
        .bind(prior_attempt + 2)
        .bind(recovering.id.to_string())
        .execute(&db.pool)
        .await
        .unwrap();
    let renewed_recovery = db
        .quota_observation_targets(&[], &[], prior_attempt + 3, 4)
        .await
        .unwrap();
    assert_eq!(renewed_recovery.len(), 1);
    assert_eq!(renewed_recovery[0].account_id, recovering.id);
    assert_eq!(renewed_recovery[0].recovery_mark, prior_attempt + 2);

    // Fixture only: no credential is used and no supplier request is made.
    sqlx::query("UPDATE upstream_accounts SET driver='kimi-oauth' WHERE id=$1")
        .bind(account.id.to_string())
        .execute(&db.pool)
        .await
        .unwrap();
    let group = Uuid::now_v7();
    let plugin_id = format!("quota-fixture-{group}");
    sqlx::query("INSERT INTO route_groups (id,tenant_id,name,normalized_name,created_at,updated_at,routing_strategy,routing_priority,strategy_version) VALUES ($1,$2,'quota','quota',1,1,$3,0,1)")
        .bind(group.to_string()).bind(auth.tenant_id.to_string()).bind(json!({"plugin_id":plugin_id,"config":{}}).to_string()).execute(&db.pool).await.unwrap();
    sqlx::query("INSERT INTO model_route_group_memberships (tenant_id,route_group_id,model_route_id,created_at) VALUES ($1,$2,$3,1)")
        .bind(auth.tenant_id.to_string()).bind(group.to_string()).bind(route.id.to_string()).execute(&db.pool).await.unwrap();
    let non_opted_in = db
        .quota_observation_targets(&["not-enabled".into()], &[], 100, 4)
        .await
        .unwrap();
    assert_eq!(non_opted_in.len(), 1);
    assert_eq!(non_opted_in[0].account_id, recovering.id);
    assert!(non_opted_in[0].recovering_quota);
    let recovery_lease = Uuid::now_v7();
    assert!(
        db.claim_quota_observation(
            &renewed_recovery[0],
            recovery_lease,
            prior_attempt + 1,
            prior_attempt + 33,
        )
        .await
        .unwrap(),
        "a new hard-quota episode bypasses the prior observation backoff exactly once"
    );
    db.finish_quota_observation(
        &renewed_recovery[0],
        recovery_lease,
        None,
        prior_attempt + 100,
    )
    .await
    .unwrap();
    assert!(
        !db.claim_quota_observation(
            &renewed_recovery[0],
            Uuid::now_v7(),
            prior_attempt + 3,
            prior_attempt + 34,
        )
        .await
        .unwrap(),
        "the persisted episode mark prevents a faster gateway clock from bypassing twice"
    );
    sqlx::query("DELETE FROM upstream_account_health WHERE upstream_account_id=$1")
        .bind(recovering.id.to_string())
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(
        !db.claim_quota_observation(
            &renewed_recovery[0],
            Uuid::now_v7(),
            prior_attempt + 101,
            prior_attempt + 131,
        )
        .await
        .unwrap(),
        "a stale recovery target cannot probe after the matching health episode is gone"
    );
    let opted_in = db
        .quota_observation_targets(
            std::slice::from_ref(&plugin_id),
            std::slice::from_ref(&plugin_id),
            100,
            4,
        )
        .await
        .unwrap();
    assert!(
        opted_in
            .iter()
            .find(|target| target.account_id == account.id)
            .unwrap()
            .allocation_history
    );
    let target = db
        .quota_observation_targets(&[plugin_id], &[], 100, 4)
        .await
        .unwrap()
        .into_iter()
        .find(|target| target.account_id == account.id)
        .unwrap();
    assert!(!target.recovering_quota);
    assert!(!target.allocation_history);
    let lease = Uuid::now_v7();
    assert!(
        db.claim_quota_observation(&target, lease, 100, 130)
            .await
            .unwrap()
    );
    assert!(
        !db.claim_quota_observation(&target, Uuid::now_v7(), 101, 131)
            .await
            .unwrap()
    );
    let value = RoutingQuotaObservation {
        account_id: account.id,
        generation: target.generation,
        config_revision: target.config_revision,
        provider: "kimi-oauth".into(),
        ordinary_code_allowance: None,
        started_at: 90,
        observed_at: 100,
        valid_until: 200,
        windows: vec![RoutingQuotaWindow {
            id: "summary".into(),
            period_seconds: Some(604800),
            reset_at: Some(190),
            reset_is_estimated: false,
            remaining_fraction: Some(0.5),
            exhausted: Some(false),
        }],
    };
    db.finish_quota_observation(&target, Uuid::now_v7(), Some(&value), 110)
        .await
        .unwrap();
    let candidate = AuthorizedUpstreamCandidate {
        route_id: route.id,
        account_id: account.id,
        driver: "kimi-oauth".into(),
        upstream_model: "fixture-model".into(),
        transport_revision: target.config_revision,
        credential_generation: target.generation,
    };
    let candidates = [candidate];
    assert!(
        db.routing_quota_observations(auth.tenant_id, &candidates, 105)
            .await
            .unwrap()
            .is_empty()
    );
    db.finish_quota_observation(&target, lease, Some(&value), 110)
        .await
        .unwrap();
    assert_eq!(
        db.routing_quota_observations(auth.tenant_id, &candidates, 105)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        db.routing_quota_observations(Uuid::now_v7(), &candidates, 105)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        db.routing_quota_observations(auth.tenant_id, &candidates, 190)
            .await
            .unwrap()
            .is_empty()
    );
    let metadata = db
        .quota_window_metadata(&[account.id], &tenant)
        .await
        .unwrap();
    assert_eq!(metadata.len(), 1);
    assert_eq!(metadata[0].provider, "kimi-oauth");
    assert_eq!(metadata[0].windows[0].id, "summary");
    assert_eq!(metadata[0].windows[0].period_seconds, Some(604800));
    assert!(
        db.quota_window_metadata(&[account.id], "other-tenant")
            .await
            .unwrap()
            .is_empty()
    );
    let failure_lease = Uuid::now_v7();
    assert!(
        db.claim_quota_observation(&target, failure_lease, 111, 141)
            .await
            .unwrap()
    );
    db.finish_quota_observation(&target, failure_lease, None, 150)
        .await
        .unwrap();
    assert!(
        db.routing_quota_observations(auth.tenant_id, &candidates, 112)
            .await
            .unwrap()
            .is_empty()
    );
    let stale_lease = Uuid::now_v7();
    assert!(
        db.claim_quota_observation(&target, stale_lease, 151, 181)
            .await
            .unwrap()
    );
    sqlx::query("UPDATE upstream_accounts SET updated_at=updated_at+1,credential_generation=credential_generation+1 WHERE id=$1").bind(account.id.to_string()).execute(&db.pool).await.unwrap();
    db.finish_quota_observation(&target, stale_lease, Some(&value), 160)
        .await
        .unwrap();
    assert!(
        db.routing_quota_observations(auth.tenant_id, &candidates, 155)
            .await
            .unwrap()
            .is_empty()
    );
    let metadata = db
        .quota_window_metadata(&[account.id], &tenant)
        .await
        .unwrap();
    assert!(metadata[0].windows.is_empty());
    verify_scoped_supplier_usage_recovery(db, &tenant, state.config.key_pepper.as_bytes()).await;
}

async fn verify_scoped_supplier_usage_recovery(db: &Database, tenant: &str, pepper: &[u8]) {
    use crate::upstream_quota::observations::OrdinaryCodeAllowance;
    let account = db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: tenant.into(),
                name: "supplier-only-recovery".into(),
                driver: "http-json".into(),
                config: json!({"base_url":"http://127.0.0.1:18083","network_scope":"private"}),
                credential: UpstreamCredential::None,
                oauth_session_id: None,
                oauth_driver: None,
                oauth_refresh_url: None,
            },
            pepper,
        )
        .await
        .unwrap();
    sqlx::query("UPDATE upstream_accounts SET driver='openai-codex' WHERE id=$1")
        .bind(account.id.to_string())
        .execute(&db.pool)
        .await
        .unwrap();
    let now = unix_millis();
    let started_at = now - 100;
    for (model, scope, generation, revision, failed_at) in [
        (
            "covered",
            "ordinary_code",
            account.credential_generation,
            account.updated_at,
            started_at - 1,
        ),
        (
            "during-read",
            "ordinary_code",
            account.credential_generation,
            account.updated_at,
            started_at + 1,
        ),
        (
            "review",
            "code_review",
            account.credential_generation,
            account.updated_at,
            started_at - 1,
        ),
        (
            "specific",
            "model_specific",
            account.credential_generation,
            account.updated_at,
            started_at - 1,
        ),
        (
            "old-generation",
            "ordinary_code",
            account.credential_generation - 1,
            account.updated_at,
            started_at - 1,
        ),
        (
            "old-revision",
            "ordinary_code",
            account.credential_generation,
            account.updated_at - 1,
            started_at - 1,
        ),
    ] {
        sqlx::query("INSERT INTO upstream_supplier_availability (upstream_account_id,upstream_model,credential_generation,transport_revision,failure_kind,reset_at,reset_source,observed_at,request_id,capacity_scope,hard_observed_at,reset_observed_at) VALUES ($1,$2,$3,$4,'quota_exhausted',$5,'provider_reset',$6,'fixture',$7,$6,$6)")
            .bind(account.id.to_string()).bind(model).bind(generation).bind(revision)
            .bind(now + 86_400_000).bind(failed_at).bind(scope).execute(&db.pool).await.unwrap();
    }
    // Old writers/migrated rows have no proved capacity scope; the default must
    // remain unknown rather than becoming account-wide evidence.
    sqlx::query("INSERT INTO upstream_supplier_availability (upstream_account_id,upstream_model,credential_generation,transport_revision,failure_kind,reset_at,reset_source,observed_at,request_id) VALUES ($1,'unknown',$2,$3,'quota_exhausted',$4,'provider_reset',$5,'legacy-fixture')")
        .bind(account.id.to_string()).bind(account.credential_generation).bind(account.updated_at)
        .bind(now + 86_400_000).bind(started_at - 1).execute(&db.pool).await.unwrap();
    let target = db
        .quota_observation_targets(&[], &[], now, 100)
        .await
        .unwrap()
        .into_iter()
        .find(|target| target.account_id == account.id)
        .unwrap();
    assert!(
        target.recovering_quota,
        "scoped-only failures need independent quota recovery"
    );
    let lease = Uuid::now_v7();
    assert!(
        db.claim_quota_observation(&target, lease, now, now + 30_000)
            .await
            .unwrap()
    );
    sqlx::query("INSERT INTO upstream_account_health (upstream_account_id,credential_generation,transport_revision,consecutive_failures,cooldown_until,probe_lease_until,last_failure_kind,updated_at) VALUES ($1,$2,$3,1,$4,0,'quota_exhausted',$5)")
        .bind(account.id.to_string()).bind(account.credential_generation).bind(account.updated_at)
        .bind(now+86_400_000).bind(started_at-1).execute(&db.pool).await.unwrap();
    let route = db
        .create_model_route(CreateModelRouteInput {
            tenant_external_id: tenant.into(),
            public_model: "quota-recovery-contract".into(),
            upstream_account_id: account.id,
            upstream_model: "covered".into(),
            protocol: "openai".into(),
            priority: 0,
        })
        .await
        .unwrap();
    let key = db
        .create_key_with_routing(
            CreateKeyInput {
                tenant_external_id: tenant.into(),
                principal_external_id: "recovery-contract".into(),
                alias: "recovery-contract".into(),
                currency: "USD".into(),
                policy: KeyPolicy::default(),
                initial_balance: Decimal::TEN,
                idempotency_key: None,
            },
            &[route.id],
            &[],
            pepper,
        )
        .await
        .unwrap();
    let tenant_id = db
        .authenticate_key(&key.key, pepper)
        .await
        .unwrap()
        .tenant_id;
    let selection = RouteSelectionOptions {
        upstream_account_hint: None,
        avoid_route_account: None,
        selection_seed: Uuid::now_v7(),
    };
    assert!(
        db.list_authorized_upstream_candidates_with_hint(
            key.key_id,
            tenant_id,
            "quota-recovery-contract",
            "openai",
            selection,
        )
        .await
        .unwrap()
        .is_empty(),
        "same-source legacy and scoped exhaustion blocks actual dispatch"
    );
    let value = RoutingQuotaObservation {
        account_id: account.id,
        generation: account.credential_generation,
        config_revision: account.updated_at,
        provider: "openai-codex".into(),
        started_at,
        observed_at: now - 10,
        valid_until: now + 30_000,
        windows: vec![RoutingQuotaWindow {
            id: "code:primary_window".into(),
            period_seconds: Some(604800),
            reset_at: Some(now + 86_400_000),
            reset_is_estimated: false,
            remaining_fraction: Some(0.98),
            exhausted: Some(false),
        }],
        ordinary_code_allowance: Some(OrdinaryCodeAllowance {
            started_at,
            window_ids: vec!["code:primary_window".into()],
            legacy_fence: Some((1, started_at - 1)),
        }),
    };
    // Publication and both recovery projections share the caller transaction.
    // Aborting after every mutation must retain the original lease and blockers.
    let mut aborted = db.begin_write_transaction().await.unwrap();
    db.publish_accepted_quota_observation(&mut aborted, &value)
        .await
        .unwrap();
    aborted.rollback().await.unwrap();
    let unpublished: Option<String> = sqlx::query_scalar(
        "SELECT observation_json FROM upstream_quota_observations WHERE upstream_account_id=$1",
    )
    .bind(account.id.to_string())
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert!(
        unpublished.is_none(),
        "aborted publication cannot leak a routing observation"
    );
    assert!(
        db.list_authorized_upstream_candidates_with_hint(
            key.key_id,
            tenant_id,
            "quota-recovery-contract",
            "openai",
            selection,
        )
        .await
        .unwrap()
        .is_empty(),
        "aborted recovery retains the dispatch blockers"
    );
    db.finish_quota_observation(&target, lease, Some(&value), now + 10_000)
        .await
        .unwrap();
    let legacy: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM upstream_account_health WHERE upstream_account_id=$1",
    )
    .bind(account.id.to_string())
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        legacy, 0,
        "same accepted proof clears the fenced legacy blocker atomically"
    );
    let dispatched = db
        .list_authorized_upstream_candidates_with_hint(
            key.key_id,
            tenant_id,
            "quota-recovery-contract",
            "openai",
            selection,
        )
        .await
        .unwrap();
    assert_eq!(
        dispatched.len(),
        1,
        "accepted proof restores actual dispatch"
    );
    assert_eq!(dispatched[0].account_id, account.id);
    let mut newer = value.clone();
    newer.started_at = started_at + 10;
    newer.observed_at = now;
    newer.valid_until = now + 40_000;
    let old_background_lease = Uuid::now_v7();
    sqlx::query("UPDATE upstream_quota_observations SET lease_id=$2,lease_until=$3 WHERE upstream_account_id=$1")
        .bind(account.id.to_string()).bind(old_background_lease.to_string()).bind(now+30_000).execute(&db.pool).await.unwrap();
    db.publish_manual_quota_observation(&newer).await.unwrap();
    let retained_lease: Option<String> = sqlx::query_scalar(
        "SELECT lease_id FROM upstream_quota_observations WHERE upstream_account_id=$1",
    )
    .bind(account.id.to_string())
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        retained_lease,
        Some(old_background_lease.to_string()),
        "manual publication never steals a background lease"
    );
    let mut late_old = value.clone();
    late_old.observed_at = unix_millis();
    late_old.windows[0].remaining_fraction = Some(0.01);
    db.finish_quota_observation(&target, old_background_lease, Some(&late_old), now + 10_000)
        .await
        .unwrap();
    let accepted: i64 = sqlx::query_scalar(
        "SELECT observed_at FROM upstream_quota_observations WHERE upstream_account_id=$1",
    )
    .bind(account.id.to_string())
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        accepted, now,
        "older publication cannot replace newer authority"
    );
    let accepted_json: String = sqlx::query_scalar(
        "SELECT observation_json FROM upstream_quota_observations WHERE upstream_account_id=$1",
    )
    .bind(account.id.to_string())
    .fetch_one(&db.pool)
    .await
    .unwrap();
    let accepted_value: RoutingQuotaObservation = serde_json::from_str(&accepted_json).unwrap();
    assert_eq!(accepted_value.started_at, newer.started_at);
    assert_eq!(
        accepted_value.windows[0].remaining_fraction,
        Some(0.98),
        "late older supplier read cannot overwrite newer manual windows"
    );
    let rows = sqlx::query("SELECT upstream_model,failure_kind,capacity_scope,request_id FROM upstream_supplier_availability WHERE upstream_account_id=$1")
        .bind(account.id.to_string()).fetch_all(&db.pool).await.unwrap();
    assert_eq!(rows.len(), 7);
    for row in rows {
        let model: String = row.get("upstream_model");
        assert_eq!(
            row.get::<String, _>("failure_kind"),
            if model == "covered" {
                "available"
            } else {
                "quota_exhausted"
            },
            "scope/time fence for {model}"
        );
        assert_eq!(
            row.get::<String, _>("request_id"),
            if model == "unknown" {
                "legacy-fixture"
            } else {
                "fixture"
            }
        );
        if model == "unknown" {
            assert_eq!(row.get::<String, _>("capacity_scope"), "unknown");
        }
    }
    let decisions = db
        .supplier_availability_decisions(account.id)
        .await
        .unwrap();
    assert_eq!(
        decisions.len(),
        5,
        "presentation excludes other generation/revision facts"
    );
    for decision in decisions {
        assert_eq!(
            decision.dispatch_eligible,
            decision.upstream_model == "covered",
            "shared scheduling decision for {}",
            decision.upstream_model
        );
    }
    db.migrate().await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM upstream_supplier_availability WHERE upstream_account_id=$1"
        )
        .bind(account.id.to_string())
        .fetch_one(&db.pool)
        .await
        .unwrap(),
        7,
        "migration replay retains all facts"
    );
}
