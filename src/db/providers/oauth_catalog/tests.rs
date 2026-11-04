use super::*;
use crate::db::{
    CreateUpstreamAccountInput, Database, DiscoveredUpstreamModel, ReplaceModelCatalogResult,
};
use serde_json::json;

const PEPPER: &[u8] = b"catalog refresh continuity synthetic test pepper";

fn credential(driver: &str, identity: &str) -> UpstreamCredential {
    UpstreamCredential::OAuth {
        access_token: format!("synthetic-access-{}", Uuid::now_v7()),
        refresh_token: Some("synthetic-refresh".into()),
        expires_at: Some(unix_millis() + 3_600_000),
        header: "authorization".into(),
        prefix: "Bearer ".into(),
        adapter_state: Some(if driver == "kimi-oauth" {
            json!({"schema":"kimi-oauth-v1", "token_type":"Bearer", "device_id":"synthetic-device", "scope":identity})
        } else {
            json!({"schema":"openai-codex-oauth-v1", "account_id":identity})
        }),
        proxy_url: None,
        proxy_network_scope: None,
    }
}

async fn publish(
    database: &Database,
    account: Uuid,
    tenant: &str,
    generation: i64,
    models: &[DiscoveredUpstreamModel],
) {
    let lease = Uuid::now_v7();
    assert!(
        database
            .claim_upstream_model_catalog_sync(account, tenant, generation, lease)
            .await
            .unwrap()
    );
    assert_eq!(
        database
            .replace_upstream_model_catalog(account, tenant, generation, lease, "openai_v1", models)
            .await
            .unwrap(),
        ReplaceModelCatalogResult::Replaced
    );
}

async fn refresh(database: &Database, account: Uuid, next: UpstreamCredential) {
    let idempotency = format!("refresh-{}", Uuid::now_v7());
    assert!(
        database
            .begin_upstream_oauth_refresh(account, &idempotency, PEPPER)
            .await
            .unwrap()
            .is_none()
    );
    database
        .mark_upstream_oauth_refresh_request_started(account, &idempotency)
        .await
        .unwrap();
    database
        .finish_upstream_oauth_refresh(account, next, &idempotency, PEPPER)
        .await
        .unwrap();
}

async fn eligible(database: &Database, account: Uuid) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM model_route_eligible_upstream_accounts WHERE upstream_account_id = $1")
        .bind(account.to_string()).fetch_one(&database.pool).await.unwrap()
}

#[tokio::test]
async fn sqlite_refresh_catalog_interleaving_preserves_only_same_identity_evidence() {
    let directory = tempfile::tempdir().unwrap();
    verify_interleaving(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("refresh.db").display()
    ))
    .await;
}

#[tokio::test]
async fn postgres_refresh_catalog_interleaving_preserves_only_same_identity_evidence() {
    let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    verify_interleaving(&url).await;
}

async fn verify_interleaving(url: &str) {
    let database = Database::connect_with_max(url, 4).await.unwrap();
    database.migrate().await.unwrap();
    for driver in ["kimi-oauth", "openai-codex"] {
        let tenant = format!("catalog-interleave-{}", Uuid::now_v7());
        let account = database.create_upstream_account(CreateUpstreamAccountInput {
            tenant_external_id: tenant.clone(),
            name: format!("synthetic-{driver}"),
            driver: driver.into(),
            config: json!({"base_url": if driver == "kimi-oauth" { "https://api.kimi.com/coding" } else { "https://chatgpt.com/backend-api/codex" }}),
            credential: credential(driver, "identity-one"),
            oauth_session_id: Some(Uuid::now_v7()),
            oauth_driver: Some(if driver == "kimi-oauth" { "kimi-oauth" } else { "openai_codex_device" }.into()),
            oauth_refresh_url: Some("https://authorization.example/refresh".into()),
        }, PEPPER).await.unwrap();
        let models = [DiscoveredUpstreamModel {
            model_id: "known-model".into(),
            protocol: "openai".into(),
            context_window: Some(100_000),
            reservation_token_bound: Some(100_000),
            reservation_bound_source: Some("catalog".into()),
        }];
        publish(&database, account.id, &tenant, 1, &models).await;
        let route = Uuid::now_v7();
        sqlx::query("INSERT INTO model_routes (id,tenant_id,public_model,upstream_account_id,upstream_model,protocol,priority,enabled,created_at,updated_at) VALUES ($1,$2,'known-model',$3,'known-model','openai',0,1,1,1)")
            .bind(route.to_string()).bind(account.tenant_id.to_string()).bind(account.id.to_string()).execute(&database.pool).await.unwrap();
        sqlx::query("INSERT INTO model_route_upstream_accounts (tenant_id,model_route_id,upstream_account_id,upstream_model,scheduling_weight,catalog_policy,created_at) VALUES ($1,$2,$3,'known-model',100,'required',1)")
            .bind(account.tenant_id.to_string()).bind(route.to_string()).bind(account.id.to_string()).execute(&database.pool).await.unwrap();
        assert_eq!(eligible(&database, account.id).await, 1);
        let fetched_before: i64 = sqlx::query_scalar("SELECT fetched_at FROM upstream_model_catalog_snapshots WHERE upstream_account_id = $1")
            .bind(account.id.to_string()).fetch_one(&database.pool).await.unwrap();

        let mut old_interleaving = database.begin_write_transaction().await.unwrap();
        sqlx::query("UPDATE upstream_accounts SET credential_generation = 2 WHERE id = $1")
            .bind(account.id.to_string())
            .execute(&mut *old_interleaving)
            .await
            .unwrap();
        let old_gap: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM model_route_eligible_upstream_accounts WHERE upstream_account_id = $1")
            .bind(account.id.to_string()).fetch_one(&mut *old_interleaving).await.unwrap();
        assert_eq!(old_gap, 0);
        old_interleaving.rollback().await.unwrap();

        let obsolete_lease = Uuid::now_v7();
        assert!(
            database
                .claim_upstream_model_catalog_sync(account.id, &tenant, 1, obsolete_lease)
                .await
                .unwrap()
        );
        refresh(&database, account.id, credential(driver, "identity-one")).await;
        assert_eq!(eligible(&database, account.id).await, 1);
        let row = sqlx::query("SELECT catalog.status, catalog.credential_generation, snapshot.fetched_at, snapshot.credential_generation AS snapshot_generation FROM upstream_model_catalog_state catalog JOIN upstream_model_catalog_snapshots snapshot ON snapshot.id = catalog.current_snapshot_id WHERE catalog.upstream_account_id = $1")
            .bind(account.id.to_string()).fetch_one(&database.pool).await.unwrap();
        assert_eq!(row.get::<String, _>("status"), "stale");
        assert_eq!(row.get::<i64, _>("credential_generation"), 2);
        assert_eq!(row.get::<i64, _>("snapshot_generation"), 2);
        assert_eq!(row.get::<i64, _>("fetched_at"), fetched_before);
        assert_ne!(
            database
                .replace_upstream_model_catalog(
                    account.id,
                    &tenant,
                    1,
                    obsolete_lease,
                    "openai_v1",
                    &[]
                )
                .await
                .unwrap(),
            ReplaceModelCatalogResult::Replaced
        );
        assert_eq!(eligible(&database, account.id).await, 1);
        let failed_lease = Uuid::now_v7();
        assert!(
            database
                .claim_upstream_model_catalog_sync(account.id, &tenant, 2, failed_lease)
                .await
                .unwrap()
        );
        database
            .record_upstream_model_catalog_failure(
                account.id,
                &tenant,
                2,
                failed_lease,
                "upstream_unavailable",
            )
            .await
            .unwrap();
        assert_eq!(eligible(&database, account.id).await, 1);

        let unauthorized = database
            .list_authorized_upstream_candidates_with_hint(
                Uuid::now_v7(),
                account.tenant_id,
                "known-model",
                "openai",
                crate::db::RouteSelectionOptions {
                    upstream_account_hint: None,
                    avoid_route_account: None,
                    selection_seed: Uuid::now_v7(),
                },
            )
            .await
            .unwrap();
        assert!(unauthorized.is_empty());
        let revision: i64 =
            sqlx::query_scalar("SELECT updated_at FROM upstream_accounts WHERE id = $1")
                .bind(account.id.to_string())
                .fetch_one(&database.pool)
                .await
                .unwrap();
        assert!(
            database
                .reload_prepared_upstream_snapshot(account.id, revision, 2, PEPPER)
                .await
                .unwrap()
                .is_some()
        );
        sqlx::query("UPDATE upstream_credentials SET expires_at = 1 WHERE upstream_account_id = $1 AND generation = 2").bind(account.id.to_string()).execute(&database.pool).await.unwrap();
        assert!(
            database
                .reload_prepared_upstream_snapshot(account.id, revision, 2, PEPPER)
                .await
                .unwrap()
                .is_none()
        );

        let withdrawn_catalog = if driver == "openai-codex" {
            vec![DiscoveredUpstreamModel {
                model_id: "other-known-model".into(),
                ..models[0].clone()
            }]
        } else {
            Vec::new()
        };
        publish(&database, account.id, &tenant, 2, &withdrawn_catalog).await;
        assert_eq!(eligible(&database, account.id).await, 0);
        refresh(&database, account.id, credential(driver, "identity-one")).await;
        assert_eq!(eligible(&database, account.id).await, 0);
        let inherited = database
            .upstream_model_catalog(account.id, &tenant, None, 100)
            .await
            .unwrap();
        assert_eq!(inherited.models.len(), withdrawn_catalog.len());
        assert!(
            inherited
                .models
                .iter()
                .all(|model| model.id != "known-model")
        );
        publish(&database, account.id, &tenant, 3, &models).await;
        refresh(&database, account.id, credential(driver, "identity-two")).await;
        assert_eq!(eligible(&database, account.id).await, 0);
        publish(&database, account.id, &tenant, 4, &models).await;
        let generation = if driver == "openai-codex" {
            verify_transport_edits(&database, account.id, &tenant, route).await;
            7
        } else {
            4
        };
        database
            .rotate_upstream_credential_with_outcome(
                account.id,
                credential(driver, "identity-two"),
                &format!("manual-{}", Uuid::now_v7()),
                Some(generation),
                PEPPER,
            )
            .await
            .unwrap();
        assert_eq!(eligible(&database, account.id).await, 0);
    }
    database.close().await;
}

async fn verify_transport_edits(database: &Database, account_id: Uuid, tenant: &str, route: Uuid) {
    use crate::db::transport_proxy_management::{
        BindGroup, CreateGroup, MemberInput, UnbindGroup, UpdateGroup,
    };
    let primary = "socks5h://10.20.30.40:1080";
    let backup = "socks5h://10.20.30.41:1080";
    let issued = database
        .create_key(
            crate::db::CreateKeyInput {
                tenant_external_id: tenant.into(),
                principal_external_id: "catalog-client".into(),
                alias: "catalog-client".into(),
                currency: "USD".into(),
                policy: crate::model::KeyPolicy::default(),
                initial_balance: rust_decimal::Decimal::ZERO,
                idempotency_key: None,
            },
            PEPPER,
        )
        .await
        .unwrap();
    let grants = database
        .credential_routing(issued.key_id, tenant)
        .await
        .unwrap();
    database
        .replace_credential_routing(
            issued.key_id,
            crate::db::ReplaceCredentialRoutingInput {
                tenant_external_id: tenant.into(),
                route_ids: vec![route],
                route_group_ids: vec![],
                expected_grant_revision: grants.grant_revision,
            },
        )
        .await
        .unwrap();
    let group = database
        .create_transport_group(
            CreateGroup {
                tenant_external_id: tenant.into(),
                name: "catalog-egress".into(),
                members: vec![MemberInput {
                    id: None,
                    label: "only exit".into(),
                    proxy_url: Some(primary.into()),
                }],
                probe_url: None,
                probe_attempts: None,
            },
            None,
            PEPPER,
        )
        .await
        .unwrap();
    let group_id = Uuid::parse_str(group["id"].as_str().unwrap()).unwrap();
    let member_id = Uuid::parse_str(group["members"][0]["id"].as_str().unwrap()).unwrap();
    let fetched_before: i64 = sqlx::query_scalar(
        "SELECT fetched_at FROM upstream_model_catalog_snapshots WHERE upstream_account_id = $1",
    )
    .bind(account_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    let obsolete_lease = Uuid::now_v7();
    assert!(
        database
            .claim_upstream_model_catalog_sync(account_id, tenant, 4, obsolete_lease)
            .await
            .unwrap()
    );
    for operation in ["bind", "edit", "unbind"] {
        let (before, _) = database
            .upstream_account_with_credential(account_id, PEPPER)
            .await
            .unwrap();
        let binding = database
            .get_transport_binding(account_id, tenant)
            .await
            .unwrap();
        match operation {
            "bind" => {
                database
                    .bind_transport_group(
                        account_id,
                        BindGroup {
                            tenant_external_id: tenant.into(),
                            group_id,
                            expected_group_version: 1,
                            initial_member_id: member_id,
                            expected_binding_version: 0,
                            expected_credential_generation: before.credential_generation,
                            expected_updated_at: before.updated_at,
                        },
                        None,
                        PEPPER,
                    )
                    .await
                    .unwrap();
            }
            "edit" => {
                database
                    .update_transport_group(
                        group_id,
                        UpdateGroup {
                            tenant_external_id: tenant.into(),
                            expected_version: 1,
                            name: "catalog-egress".into(),
                            members: vec![MemberInput {
                                id: Some(member_id),
                                label: "edited exit".into(),
                                proxy_url: Some(backup.into()),
                            }],
                            replacement_member_id: None,
                            probe_url: None,
                            probe_attempts: None,
                        },
                        None,
                        PEPPER,
                    )
                    .await
                    .unwrap();
            }
            "unbind" => {
                database
                    .unbind_transport_group(
                        account_id,
                        UnbindGroup {
                            tenant_external_id: tenant.into(),
                            expected_group_version: 2,
                            expected_binding_version: binding["binding_version"].as_i64().unwrap(),
                            expected_credential_generation: before.credential_generation,
                            expected_updated_at: before.updated_at,
                            single_proxy_member_id: member_id,
                        },
                        None,
                        PEPPER,
                    )
                    .await
                    .unwrap();
            }
            _ => unreachable!(),
        }
        let (after, credential) = database
            .upstream_account_with_credential(account_id, PEPPER)
            .await
            .unwrap();
        assert_eq!(
            after.credential_generation,
            before.credential_generation + 1
        );
        assert_eq!(
            credential.proxy().unwrap().0,
            if operation == "bind" { primary } else { backup }
        );
        if operation != "unbind" {
            let runtime =
                crate::db::transport_proxy_groups::TransportProxyGroups::parse("[]", PEPPER)
                    .unwrap();
            runtime
                .synchronize_managed_for_test(&database.pool)
                .await
                .unwrap();
            let selected = runtime
                .select_config(
                    account_id,
                    after.credential_generation,
                    &credential,
                    &after.config,
                )
                .unwrap();
            assert_eq!(selected.member_count(), 1);
            assert!(!runtime.advance_sticky_after_probe(account_id));
            let retained = runtime
                .select_config(
                    account_id,
                    after.credential_generation,
                    &credential,
                    &after.config,
                )
                .unwrap();
            assert_eq!(retained.credential.proxy(), credential.proxy());
        }
        assert_eq!(eligible(database, account_id).await, 1, "{operation}");
        let mut candidates = database
            .list_authorized_upstream_candidates_with_hint(
                issued.key_id,
                after.tenant_id,
                "known-model",
                "openai",
                crate::db::RouteSelectionOptions {
                    upstream_account_hint: None,
                    avoid_route_account: None,
                    selection_seed: Uuid::now_v7(),
                },
            )
            .await
            .unwrap();
        assert_eq!(candidates.len(), 1);
        let current = candidates.pop().unwrap();
        assert_eq!(current.account_id, account_id);
        assert_eq!(current.credential_generation, after.credential_generation);
        let materialized = database
            .materialize_authorized_upstream_candidate(&current, PEPPER)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(materialized.upstream_model, "known-model");
        let stale = crate::provider::AuthorizedUpstreamCandidate {
            transport_revision: before.updated_at,
            credential_generation: before.credential_generation,
            ..current
        };
        assert!(
            database
                .materialize_authorized_upstream_candidate(&stale, PEPPER)
                .await
                .unwrap()
                .is_none()
        );
        let catalog = sqlx::query("SELECT catalog.status, catalog.credential_generation, snapshot.fetched_at, snapshot.credential_generation AS snapshot_generation FROM upstream_model_catalog_state catalog JOIN upstream_model_catalog_snapshots snapshot ON snapshot.id = catalog.current_snapshot_id WHERE catalog.upstream_account_id = $1")
            .bind(account_id.to_string()).fetch_one(&database.pool).await.unwrap();
        assert_eq!(catalog.get::<String, _>("status"), "stale");
        assert_eq!(
            catalog.get::<i64, _>("credential_generation"),
            after.credential_generation
        );
        assert_eq!(
            catalog.get::<i64, _>("snapshot_generation"),
            after.credential_generation
        );
        assert_eq!(catalog.get::<i64, _>("fetched_at"), fetched_before);
        let catalog_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM upstream_model_catalog_snapshots WHERE upstream_account_id = $1",
        )
        .bind(account_id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap();
        assert_eq!(catalog_count, 1);
        for model in ["known-model", "unknown-model"] {
            let unauthorized = database
                .list_authorized_upstream_candidates_with_hint(
                    Uuid::now_v7(),
                    after.tenant_id,
                    model,
                    "openai",
                    crate::db::RouteSelectionOptions {
                        upstream_account_hint: None,
                        avoid_route_account: None,
                        selection_seed: Uuid::now_v7(),
                    },
                )
                .await
                .unwrap();
            assert!(unauthorized.is_empty());
        }
        assert!(
            database
                .list_authorized_upstream_candidates_with_hint(
                    issued.key_id,
                    after.tenant_id,
                    "unknown-model",
                    "openai",
                    crate::db::RouteSelectionOptions {
                        upstream_account_hint: None,
                        avoid_route_account: None,
                        selection_seed: Uuid::now_v7()
                    },
                )
                .await
                .unwrap()
                .is_empty()
        );
    }
    assert_ne!(
        database
            .replace_upstream_model_catalog(account_id, tenant, 4, obsolete_lease, "openai_v1", &[])
            .await
            .unwrap(),
        ReplaceModelCatalogResult::Replaced
    );
    assert_eq!(eligible(database, account_id).await, 1);
}

#[test]
fn expired_refresh_and_changed_identity_never_inherit_catalog() {
    for driver in ["kimi-oauth", "openai-codex"] {
        let original = credential(driver, "identity-one");
        assert!(same_refresh_identity(
            driver,
            &original,
            &credential(driver, "identity-one")
        ));
        assert!(!same_refresh_identity(
            driver,
            &original,
            &credential(driver, "identity-two")
        ));
        let mut expired = credential(driver, "identity-one");
        if let UpstreamCredential::OAuth { expires_at, .. } = &mut expired {
            *expires_at = Some(1);
        }
        assert!(!same_refresh_identity(driver, &original, &expired));
    }
}
