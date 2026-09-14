use super::*;
use crate::db::{CreateKeyInput, CreateUpstreamAccountInput};
use crate::{AppState, config::Config, model::KeyPolicy, provider::UpstreamCredential};
use rust_decimal::Decimal;
use serde_json::json;

#[tokio::test]
async fn overlapping_strategies_are_deterministic_scoped_and_do_not_grant_routes() {
    let directory = tempfile::tempdir().unwrap();
    let state = AppState::initialize(Config::for_test(format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("group-selection.db").display()
    )))
    .await
    .unwrap();
    let db = &state.db;
    let pepper = state.config.key_pepper.as_bytes();
    let mut candidates = Vec::new();
    for tenant in ["strategy-selection-a", "strategy-selection-b"] {
        let key = db
            .create_key(
                CreateKeyInput {
                    tenant_external_id: tenant.into(),
                    principal_external_id: "principal".into(),
                    alias: "key".into(),
                    currency: "USD".into(),
                    policy: KeyPolicy::default(),
                    initial_balance: Decimal::TEN,
                    idempotency_key: None,
                },
                pepper,
            )
            .await
            .unwrap();
        let authenticated = db.authenticate_key(&key.key, pepper).await.unwrap();
        let account = db
            .create_upstream_account(
                CreateUpstreamAccountInput {
                    tenant_external_id: tenant.into(),
                    name: "account".into(),
                    driver: "http-json".into(),
                    config: json!({"base_url":"http://127.0.0.1:18081","network_scope":"private"}),
                    credential: UpstreamCredential::None,
                    oauth_session_id: None,
                    oauth_driver: None,
                    oauth_refresh_url: None,
                },
                pepper,
            )
            .await
            .unwrap();
        let route = Uuid::now_v7();
        sqlx::query("INSERT INTO model_routes (id,tenant_id,public_model,upstream_account_id,upstream_model,protocol,priority,enabled,created_at,updated_at) VALUES ($1,$2,'model',$3,'model','openai',0,1,1,1)")
            .bind(route.to_string()).bind(authenticated.tenant_id.to_string()).bind(account.id.to_string())
            .execute(&db.pool).await.unwrap();
        candidates.push((
            authenticated.tenant_id,
            route,
            account.id,
            authenticated.key_id,
        ));
    }
    let (tenant, route, account, key) = candidates[0];
    assert!(!db.has_group_routing_strategies(tenant).await.unwrap());
    // Insert in descending UUID order to prove selection is not insertion order.
    for (id, kind, priority, included) in [
        (40, "provider", 999, false),
        (30, "provider", 10, true),
        (20, "route", 10, true),
        (10, "provider", 5, true),
    ] {
        let id = Uuid::from_u128(id).to_string();
        let table = if kind == "provider" {
            "provider_groups"
        } else {
            "route_groups"
        };
        let sql = format!(
            "INSERT INTO {table} (id,tenant_id,name,normalized_name,created_at,updated_at,routing_strategy,routing_priority,strategy_version) VALUES ($1,$2,$1,$1,1,1,$3,$4,7)"
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(&id)
            .bind(tenant.to_string())
            .bind(json!({"plugin_id":"fixture","config":{}}).to_string())
            .bind(priority)
            .execute(&db.pool)
            .await
            .unwrap();
        if kind == "provider" {
            sqlx::query("INSERT INTO upstream_account_provider_groups (tenant_id,provider_group_id,upstream_account_id,created_at) VALUES ($1,$2,$3,1)")
                .bind(tenant.to_string()).bind(&id).bind(account.to_string()).execute(&db.pool).await.unwrap();
            if included {
                sqlx::query("INSERT INTO model_route_included_provider_groups (tenant_id,model_route_id,provider_group_id,created_at) VALUES ($1,$2,$3,1)")
                    .bind(tenant.to_string()).bind(route.to_string()).bind(&id).execute(&db.pool).await.unwrap();
            }
        } else {
            sqlx::query("INSERT INTO model_route_group_memberships (tenant_id,route_group_id,model_route_id,created_at) VALUES ($1,$2,$3,1)")
                .bind(tenant.to_string()).bind(&id).bind(route.to_string()).execute(&db.pool).await.unwrap();
        }
    }
    assert!(db.has_group_routing_strategies(tenant).await.unwrap());
    let selected = db
        .candidate_group_strategy(tenant, route, account)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(selected.id, format!("{}:route", Uuid::from_u128(20)));
    assert_eq!((selected.priority, selected.version), (10, 7));
    assert_eq!(selected.strategy.plugin_id, "fixture");
    sqlx::query("UPDATE provider_groups SET routing_priority = 11 WHERE id = $1")
        .bind(Uuid::from_u128(30).to_string())
        .execute(&db.pool)
        .await
        .unwrap();
    let selected = db
        .candidate_group_strategy(tenant, route, account)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(selected.id, format!("{}:provider", Uuid::from_u128(30)));
    assert_eq!(selected.priority, 11);
    // Neither route membership nor its strategy adds a credential grant.
    let grants = db
        .credential_routing(key, "strategy-selection-a")
        .await
        .unwrap();
    assert!(grants.effective_route_ids.is_empty());
    assert!(grants.route_group_ids.is_empty());
    let (foreign_tenant, foreign_route, foreign_account, _) = candidates[1];
    assert!(
        !db.has_group_routing_strategies(foreign_tenant)
            .await
            .unwrap()
    );
    for (t, r, a) in [
        (foreign_tenant, route, account),
        (tenant, foreign_route, account),
        (tenant, route, foreign_account),
        (foreign_tenant, foreign_route, foreign_account),
    ] {
        assert!(
            db.candidate_group_strategy(t, r, a)
                .await
                .unwrap()
                .is_none()
        );
    }
}
