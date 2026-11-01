// Exact-model/grant fixture adapted from the preserved, uncommitted
// McClintock pr693-probe-model-binding-20261026 patch (HEAD 5aaa45e4).
use super::*;
use crate::db::RouteSelectionOptions;
use crate::db::{CreateKeyInput, CreateModelRouteInput, CreateUpstreamAccountInput};
use crate::model::KeyPolicy;
use crate::provider::{ResolvedUpstream, UpstreamCredential};
use rust_decimal::Decimal;

mod r1;

const TENANT: &str = "probe-model-binding";
const PEPPER: &[u8] = b"probe model binding regression key material";

async fn account(database: &Database, name: &str) -> Uuid {
    database
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: TENANT.into(),
                name: name.into(),
                driver: "http-json".into(),
                config: serde_json::json!({"base_url": "http://127.0.0.1:1"}),
                credential: UpstreamCredential::None,
                oauth_session_id: None,
                oauth_driver: None,
                oauth_refresh_url: None,
            },
            PEPPER,
        )
        .await
        .unwrap()
        .id
}

async fn route(database: &Database, account: Uuid, public: &str, model: &str) -> Uuid {
    database
        .create_model_route(CreateModelRouteInput {
            tenant_external_id: TENANT.into(),
            public_model: public.into(),
            upstream_account_id: account,
            upstream_model: model.into(),
            protocol: "openai".into(),
            priority: 0,
        })
        .await
        .unwrap()
        .id
}

async fn grant(database: &Database, key: Uuid, route: Uuid, group: Option<Uuid>) {
    if let Some(group) = group {
        sqlx::query(
            "INSERT INTO model_route_group_memberships
             (tenant_id, route_group_id, model_route_id, created_at)
             SELECT tenant_id, $1, id, 1 FROM model_routes WHERE id = $2",
        )
        .bind(group.to_string())
        .bind(route.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    } else {
        sqlx::query(
            "INSERT INTO routing_grants (tenant_id, key_id, model_route_id, created_at)
             SELECT tenant_id, $1, id, 1 FROM model_routes WHERE id = $2",
        )
        .bind(key.to_string())
        .bind(route.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    }
}

async fn billing_key(database: &Database) -> crate::model::AuthenticatedKey {
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: TENANT.into(),
                principal_external_id: "probe-billing".into(),
                alias: "filter-assistant-billing".into(),
                currency: "USD".into(),
                policy: KeyPolicy::default(),
                initial_balance: Decimal::ZERO,
                idempotency_key: None,
            },
            PEPPER,
        )
        .await
        .unwrap();
    database
        .authenticate_key(&issued.key, PEPPER)
        .await
        .unwrap()
}

async fn exact_model_account_contract(database: &Database, use_group: bool) {
    let key = billing_key(database).await;
    let group = if use_group {
        let group = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO route_groups (id, tenant_id, name, normalized_name, created_at, updated_at)
             VALUES ($1, $2, 'probe', 'probe', 1, 1)",
        ).bind(group.to_string()).bind(key.tenant_id.to_string()).execute(&database.pool).await.unwrap();
        sqlx::query(
            "INSERT INTO routing_grants (tenant_id, key_id, route_group_id, created_at) VALUES ($1,$2,$3,1)",
        ).bind(key.tenant_id.to_string()).bind(key.key_id.to_string()).bind(group.to_string())
            .execute(&database.pool).await.unwrap();
        Some(group)
    } else {
        None
    };
    let target = account(database, "target").await;
    let other = account(database, "other").await;
    let alias = route(database, target, "model-m", "model-n").await;
    grant(database, key.key_id, alias, group).await;
    let other_exact = route(database, other, "other-public", "model-m").await;
    grant(database, key.key_id, other_exact, group).await;
    assert!(
        database
            .probe_billing_route(target, key.key_id, "model-m")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        database
            .probe_billing_route(other, key.key_id, "model-m")
            .await
            .unwrap()
            .unwrap()
            .route_id,
        other_exact
    );
    let exact = route(database, target, "probe-public", "model-m").await;
    assert!(
        database
            .probe_billing_route(target, key.key_id, "model-m")
            .await
            .unwrap()
            .is_none()
    );
    grant(database, key.key_id, exact, group).await;
    let probe = database
        .probe_billing_route(target, key.key_id, "model-m")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(probe.route_id, exact);
    assert_eq!(probe.public_model, "probe-public");
    assert_eq!(
        database
            .probe_billing_route(target, key.key_id, "model-n")
            .await
            .unwrap()
            .unwrap()
            .route_id,
        alias
    );
    sqlx::query("UPDATE model_routes SET enabled = 0 WHERE id = $1")
        .bind(exact.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    assert!(
        database
            .probe_billing_route(target, key.key_id, "model-m")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn direct_and_group_probe_grants_require_exact_model_and_account() {
    for use_group in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let database = Database::connect(&format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("probe.db").display()
        ))
        .await
        .unwrap();
        database.migrate().await.unwrap();
        exact_model_account_contract(&database, use_group).await;
        database.pool.close().await;
    }
}

fn selection(hint: Option<Uuid>) -> RouteSelectionOptions {
    RouteSelectionOptions {
        upstream_account_hint: hint,
        avoid_route_account: None,
        selection_seed: Uuid::now_v7(),
    }
}

async fn scoped_evidence_contract(database: &Database) {
    let a = account(database, "quota-a").await;
    let b = account(database, "healthy-b").await;
    let key = billing_key(database).await;
    let ar = route(database, a, "public", "expensive").await;
    let br = route(database, b, "public", "expensive").await;
    grant(database, key.key_id, ar, None).await;
    grant(database, key.key_id, br, None).await;
    let cheap_route = route(database, a, "public-cheap", "cheap").await;
    grant(database, key.key_id, cheap_route, None).await;
    let candidates = database
        .resolve_authorized_upstream_candidates_with_hint(
            key.key_id,
            key.tenant_id,
            "public",
            "openai",
            selection(Some(a)),
            PEPPER,
        )
        .await
        .unwrap();
    let route: ResolvedUpstream = candidates.into_iter().find(|r| r.account_id == a).unwrap();
    let now = super::super::unix_millis();
    let evidence = SupplierQuotaEvidence {
        exhausted: true,
        reset_at: None,
        reset_source: "unknown",
    };
    database
        .store_supplier_quota(&route, Uuid::now_v7(), now, evidence)
        .await
        .unwrap();
    let remaining = database
        .list_authorized_upstream_candidates_with_hint(
            key.key_id,
            key.tenant_id,
            "public",
            "openai",
            selection(Some(a)),
        )
        .await
        .unwrap();
    assert_eq!(remaining.len(), 1, "quota excludes even a hinted account");
    assert_eq!(remaining[0].account_id, b);
    let cheap = database
        .list_authorized_upstream_candidates_with_hint(
            key.key_id,
            key.tenant_id,
            "public-cheap",
            "openai",
            selection(None),
        )
        .await
        .unwrap();
    assert_eq!(
        cheap.len(),
        1,
        "model quota must not disable the whole account"
    );
    assert_eq!(cheap[0].account_id, a);
    let recovery = database
        .list_upstream_candidates_for_dispatch(
            key.key_id,
            key.tenant_id,
            "public",
            "openai",
            selection(Some(a)),
            true,
        )
        .await
        .unwrap();
    assert_eq!(
        recovery.len(),
        2,
        "background pinned recovery remains reachable"
    );

    let mut probe = serde_json::json!({
        "account_id": a, "status": "healthy", "upstream_status": 200,
        "actual_model": "cheap", "selected_model": "cheap", "request_id": Uuid::now_v7(),
        "started_at": now + 1, "checked_at": now + 2,
        "credential_generation": route.credential_generation,
        "transport_revision": route.transport_revision,
        "source": "model_probe", "trigger": "periodic",
    });
    database
        .store_upstream_service_probe(a, &probe)
        .await
        .unwrap();
    assert_eq!(
        database.supplier_quota_recovery_models(a).await.unwrap(),
        vec!["expensive"]
    );
    let mut skipped = probe.clone();
    skipped["status"] = serde_json::json!("skipped");
    skipped["error_code"] = serde_json::json!("probe_route_unavailable");
    skipped["checked_at"] = serde_json::json!(now + 3);
    database
        .store_upstream_service_probe(a, &skipped)
        .await
        .unwrap();
    let saved = database.upstream_service_probe(a).await.unwrap().unwrap();
    assert_eq!(
        saved["checked_at"],
        now + 2,
        "skipping retains the last real result"
    );
    assert_eq!(
        saved["last_attempt"]["error_code"],
        "probe_route_unavailable"
    );
    assert_eq!(saved["actual_model"], "cheap");
    probe["credential_generation"] = serde_json::json!(route.credential_generation - 1);
    probe["actual_model"] = serde_json::json!("expensive");
    probe["checked_at"] = serde_json::json!(now + 4);
    database
        .store_upstream_service_probe(a, &probe)
        .await
        .unwrap();
    assert_eq!(
        database.upstream_service_probe(a).await.unwrap().unwrap()["actual_model"],
        "cheap"
    );
    probe["credential_generation"] = serde_json::json!(route.credential_generation);
    probe["started_at"] = serde_json::json!(now - 1);
    database
        .store_upstream_service_probe(a, &probe)
        .await
        .unwrap();
    assert_eq!(
        database.supplier_quota_recovery_models(a).await.unwrap(),
        vec!["expensive"],
        "late success predating the failure cannot heal quota"
    );
    probe["started_at"] = serde_json::json!(now + 5);
    probe["checked_at"] = serde_json::json!(now + 6);
    database
        .store_upstream_service_probe(a, &probe)
        .await
        .unwrap();
    assert!(
        database
            .supplier_quota_recovery_models(a)
            .await
            .unwrap()
            .is_empty()
    );
    database
        .store_supplier_quota(&route, Uuid::now_v7(), now, evidence)
        .await
        .unwrap();
    assert!(
        database
            .supplier_quota_recovery_models(a)
            .await
            .unwrap()
            .is_empty(),
        "late persistence of an older failure cannot erase recovery"
    );
    let restored = database
        .list_authorized_upstream_candidates_with_hint(
            key.key_id,
            key.tenant_id,
            "public",
            "openai",
            selection(None),
        )
        .await
        .unwrap();
    assert_eq!(restored.len(), 2);

    let known = SupplierQuotaEvidence {
        exhausted: true,
        reset_at: Some(now + 60_000),
        reset_source: "retry_after",
    };
    database
        .store_supplier_quota(&route, Uuid::now_v7(), now + 7, known)
        .await
        .unwrap();
    assert_eq!(
        database
            .supplier_quota_probe_deadline(a, "expensive")
            .await
            .unwrap(),
        known.reset_at
    );
    assert!(
        database
            .supplier_quota_probe_deadline(a, "cheap")
            .await
            .unwrap()
            .is_none()
    );
    database
        .store_supplier_quota(
            &route,
            Uuid::now_v7(),
            now + 8,
            SupplierQuotaEvidence {
                exhausted: false,
                reset_at: None,
                reset_source: "unknown",
            },
        )
        .await
        .unwrap();
    assert_eq!(
        database
            .supplier_quota_probe_deadline(a, "expensive")
            .await
            .unwrap(),
        known.reset_at,
        "an ordinary rate limit without timing cannot clear known exhaustion"
    );

    let mut stale = route.clone();
    stale.credential_generation -= 1;
    database
        .store_supplier_quota(&stale, Uuid::now_v7(), now + 10, evidence)
        .await
        .unwrap();
    stale = route.clone();
    stale.transport_revision -= 1;
    database
        .store_supplier_quota(&stale, Uuid::now_v7(), now + 10, evidence)
        .await
        .unwrap();
    assert!(
        database
            .supplier_quota_recovery_models(a)
            .await
            .unwrap()
            .is_empty()
    );
    sqlx::query("UPDATE upstream_accounts SET status = 'disabled' WHERE id = $1")
        .bind(a.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    database
        .store_supplier_quota(&route, Uuid::now_v7(), now + 10, evidence)
        .await
        .unwrap();
    assert!(database.probe_account_context(a).await.is_err());
    assert!(
        !database
            .upstream_service_probes_due(i64::MAX, i64::MAX, 32)
            .await
            .unwrap()
            .contains(&a)
    );
    let recovery = database
        .list_upstream_candidates_for_dispatch(
            key.key_id,
            key.tenant_id,
            "public",
            "openai",
            selection(Some(a)),
            true,
        )
        .await
        .unwrap();
    assert!(recovery.iter().all(|candidate| candidate.account_id != a));
}

#[tokio::test]
async fn scoped_quota_alternate_dispatch_probe_provenance_and_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let database = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("quota.db").display()
    ))
    .await
    .unwrap();
    database.migrate().await.unwrap();
    scoped_evidence_contract(&database).await;
    database.pool.close().await;
}

#[tokio::test]
async fn postgres_scoped_quota_alternate_dispatch_and_probe_fences() {
    let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let database = Database::connect(&url).await.unwrap();
    database.migrate().await.unwrap();
    scoped_evidence_contract(&database).await;
    database.pool.close().await;
}
