use super::*;
use crate::db::Database;
use crate::provider::UpstreamCredential;
use uuid::Uuid;

const PEPPER: &[u8] = b"effective-route-count-fixture-key";

#[tokio::test]
async fn sqlite_provider_route_counts_follow_effective_group_membership() {
    let directory = tempfile::tempdir().unwrap();
    verify_counts(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("counts.db").display()
    ))
    .await;
}

#[tokio::test]
async fn postgres_provider_route_counts_follow_effective_group_membership() {
    let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    verify_counts(&url).await;
}

async fn account(db: &Database, tenant: &str, name: &str) -> Uuid {
    db.create_upstream_account(
        CreateUpstreamAccountInput {
            tenant_external_id: tenant.into(),
            name: name.into(),
            driver: "http-json".into(),
            config: serde_json::json!({"base_url":"http://127.0.0.1:1"}),
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

async fn verify_counts(url: &str) {
    let db = Database::connect_with_max(url, 1).await.unwrap();
    db.migrate().await.unwrap();
    let tenant = format!("route-counts-{}", Uuid::now_v7());
    let foreign = format!("route-counts-foreign-{}", Uuid::now_v7());
    let a = account(&db, &tenant, "group-account-a").await;
    let b = account(&db, &tenant, "group-account-b").await;
    let outsider = account(&db, &foreign, "group-account-a").await;
    let tenant_id: String =
        sqlx::query_scalar("SELECT tenant_id FROM upstream_accounts WHERE id = $1")
            .bind(a.to_string())
            .fetch_one(&db.pool)
            .await
            .unwrap();
    for id in [a, b] {
        let lease = Uuid::now_v7();
        assert!(
            db.claim_upstream_model_catalog_sync(id, &tenant, 1, lease)
                .await
                .unwrap()
        );
        let models = ["model-one", "model-two"].map(|model| DiscoveredUpstreamModel {
            model_id: model.into(),
            protocol: "openai".into(),
            context_window: None,
            reservation_token_bound: None,
            reservation_bound_source: None,
        });
        assert_eq!(
            db.replace_upstream_model_catalog(id, &tenant, 1, lease, "openai_v1", &models)
                .await
                .unwrap(),
            ReplaceModelCatalogResult::Replaced
        );
    }
    let shared = Uuid::now_v7();
    let only_a = Uuid::now_v7();
    for group in [shared, only_a] {
        sqlx::query("INSERT INTO provider_groups (id,tenant_id,name,normalized_name,created_at,updated_at) VALUES ($1,$2,$1,$1,1,1)")
            .bind(group.to_string()).bind(&tenant_id).execute(&db.pool).await.unwrap();
    }
    for (group, id) in [(shared, a), (shared, b), (only_a, a)] {
        sqlx::query("INSERT INTO upstream_account_provider_groups (tenant_id,provider_group_id,upstream_account_id,created_at) VALUES ($1,$2,$3,1)")
            .bind(&tenant_id).bind(group.to_string()).bind(id.to_string()).execute(&db.pool).await.unwrap();
    }
    let mut routes = Vec::new();
    for model in ["model-one", "model-two", "not-discovered"] {
        let route = Uuid::now_v7();
        sqlx::query("INSERT INTO model_routes (id,tenant_id,public_model,upstream_account_id,upstream_model,protocol,priority,enabled,created_at,updated_at) VALUES ($1,$2,$3,$4,$3,'openai',0,1,1,1)")
            .bind(route.to_string()).bind(&tenant_id).bind(model).bind(a.to_string()).execute(&db.pool).await.unwrap();
        for group in [shared, only_a] {
            sqlx::query("INSERT INTO model_route_included_provider_groups (tenant_id,model_route_id,provider_group_id,created_at) VALUES ($1,$2,$3,1)")
                .bind(&tenant_id).bind(route.to_string()).bind(group.to_string()).execute(&db.pool).await.unwrap();
        }
        routes.push(route);
    }
    // A direct association plus two included groups is still one route.
    sqlx::query("INSERT INTO model_route_upstream_accounts (tenant_id,model_route_id,upstream_account_id,upstream_model,scheduling_weight,created_at,catalog_policy) VALUES ($1,$2,$3,'model-one',100,1,'required')")
        .bind(&tenant_id).bind(routes[0].to_string()).bind(a.to_string()).execute(&db.pool).await.unwrap();
    assert_counts(&db, &tenant, &[(a, 2), (b, 2)]).await;
    // Exclusion overrides both direct and group inclusion.
    sqlx::query("INSERT INTO model_route_excluded_provider_groups (tenant_id,model_route_id,provider_group_id,created_at) VALUES ($1,$2,$3,1)")
        .bind(&tenant_id).bind(routes[0].to_string()).bind(only_a.to_string()).execute(&db.pool).await.unwrap();
    assert_counts(&db, &tenant, &[(a, 1), (b, 2)]).await;
    assert_counts(&db, &foreign, &[(outsider, 0)]).await;
    assert!(
        db.upstream_account_for_reauthorization(a, &foreign)
            .await
            .is_err()
    );
    // No catalog model means no group candidate; changing credential generation
    // invalidates stale discovery evidence exactly as it does in the resolver.
    sqlx::query("UPDATE upstream_accounts SET credential_generation = 2 WHERE id = $1")
        .bind(b.to_string())
        .execute(&db.pool)
        .await
        .unwrap();
    assert_counts(&db, &tenant, &[(a, 1), (b, 0)]).await;
    // Disabled configuration remains counted; archived configuration does not.
    sqlx::query("UPDATE model_routes SET enabled = 0 WHERE id = $1")
        .bind(routes[1].to_string())
        .execute(&db.pool)
        .await
        .unwrap();
    assert_counts(&db, &tenant, &[(a, 1), (b, 0)]).await;
    sqlx::query("UPDATE model_routes SET archived_at = 2 WHERE id = $1")
        .bind(routes[1].to_string())
        .execute(&db.pool)
        .await
        .unwrap();
    assert_counts(&db, &tenant, &[(a, 0), (b, 0)]).await;
}

async fn assert_counts(db: &Database, tenant: &str, expected: &[(Uuid, i64)]) {
    let plain = db.list_upstream_accounts(tenant).await.unwrap();
    let transport = db
        .list_upstream_accounts_page_with_transport(Some(tenant), None, None, 100, PEPPER)
        .await
        .unwrap();
    assert_eq!(plain.len(), expected.len());
    assert_eq!(transport.len(), expected.len());
    for (id, count) in expected {
        assert_eq!(
            plain.iter().find(|row| row.id == *id).unwrap().route_count,
            *count
        );
        assert_eq!(
            transport
                .iter()
                .find(|row| row.id == *id)
                .unwrap()
                .route_count,
            *count
        );
        assert_eq!(
            db.upstream_account_for_reauthorization(*id, tenant)
                .await
                .unwrap()
                .route_count,
            *count
        );
    }
    let first = db
        .list_upstream_accounts_page(Some(tenant), None, None, 1)
        .await
        .unwrap();
    assert_eq!(first.len(), 1);
    let second = db
        .list_upstream_accounts_page(
            Some(tenant),
            Some(first[0].created_at),
            Some(first[0].id),
            1,
        )
        .await
        .unwrap();
    assert_eq!(second.len(), expected.len() - 1);
    for row in first.iter().chain(&second) {
        assert_eq!(
            row.route_count,
            expected.iter().find(|(id, _)| *id == row.id).unwrap().1
        );
    }
}
