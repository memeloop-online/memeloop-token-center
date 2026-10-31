use super::*;
use crate::db::transport_proxy_management::{BindGroup, CreateGroup, Member, UpdateProxySelection};

const KEY: &[u8] = b"managed-proxy-fixture-key-32-bytes";
const PRIMARY: &str = "socks5h://user:password@10.20.30.40:1080";
const BACKUP: &str = "socks5h://10.20.30.41:1080";

async fn contract(database: &Database) {
    let external = format!("network-proxy-{}", Uuid::now_v7());
    database.create_tenant(&external, None).await.unwrap();
    let proxy = database
        .create_network_proxy(
            CreateProxy {
                tenant_external_id: external.clone(),
                name: "Primary".into(),
                proxy_url: PRIMARY.into(),
                probe: ProxyProbe {
                    enabled: false,
                    ..Default::default()
                },
            },
            None,
            KEY,
        )
        .await
        .unwrap();
    let id = parse_uuid(proxy["id"].as_str().unwrap().to_owned()).unwrap();
    assert_eq!(proxy["address"]["has_auth"], true);
    assert!(!proxy.to_string().contains("password"));
    assert!(!proxy.to_string().contains("user:"));
    assert_eq!(proxy["probe"]["target_url"], "https://openai.com/");
    assert!(matches!(
        database.get_network_proxy(id, "other-tenant", KEY).await,
        Err(AppError::NotFound)
    ));
    assert!(matches!(
        database
            .update_network_proxy(
                id,
                UpdateProxy {
                    tenant_external_id: external.clone(),
                    expected_version: 2,
                    name: "stale".into(),
                    proxy_url: Some(BACKUP.into()),
                    probe: None
                },
                None,
                KEY
            )
            .await,
        Err(AppError::ProxyGroupConflict(_))
    ));
    assert!(serde_json::from_value::<CreateGroup>(serde_json::json!({"tenant_external_id":external,"name":"mixed","members":[],"proxy_ids":[id]})).is_err());
    let group = database.create_transport_group(serde_json::from_value(serde_json::json!({"tenant_external_id":external,"name":"Reusable","proxy_ids":[id]})).unwrap(), None, KEY).await.unwrap();
    let group_id = parse_uuid(group["id"].as_str().unwrap().to_owned()).unwrap();
    assert_eq!(group["members"][0]["id"], proxy["id"]);
    let ciphertext: String =
        sqlx::query_scalar("SELECT members_ciphertext FROM transport_proxy_groups WHERE id = $1")
            .bind(group_id.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert!(ciphertext.is_empty());
    let account = database
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: external.clone(),
                name: "Bound account".into(),
                driver: "openai-codex".into(),
                config: serde_json::json!({"base_url":"https://chatgpt.com/backend-api/codex"}),
                credential: UpstreamCredential::OAuth {
                    access_token: "fixture".into(),
                    refresh_token: Some("refresh".into()),
                    expires_at: Some(i64::MAX),
                    header: "authorization".into(),
                    prefix: "Bearer ".into(),
                    adapter_state: None,
                    proxy_url: Some("socks5h://100.64.0.16:1080".into()),
                    proxy_network_scope: Some(crate::network::OutboundScope::Private),
                },
                oauth_session_id: None,
                oauth_driver: None,
                oauth_refresh_url: None,
            },
            KEY,
        )
        .await
        .unwrap();
    normalize_legacy_members(&database.pool, KEY).await.unwrap();
    let (unchanged, credential) = database
        .upstream_account_with_credential(account.id, KEY)
        .await
        .unwrap();
    assert_eq!(
        unchanged.credential_generation,
        account.credential_generation
    );
    assert_eq!(credential.proxy().unwrap().0, "socks5h://100.64.0.16:1080");
    database
        .bind_transport_group(
            account.id,
            BindGroup {
                tenant_external_id: external.clone(),
                group_id,
                expected_group_version: 1,
                initial_member_id: id,
                expected_binding_version: 0,
                expected_credential_generation: account.credential_generation,
                expected_updated_at: account.updated_at,
            },
            None,
            KEY,
        )
        .await
        .unwrap();
    let runtime = TransportProxyGroups::parse("[]", KEY).unwrap();
    runtime
        .synchronize_managed_for_test(&database.pool)
        .await
        .unwrap();
    let (bound, credential) = database
        .upstream_account_with_credential(account.id, KEY)
        .await
        .unwrap();
    let inflight = runtime
        .select_config(
            account.id,
            bound.credential_generation,
            &credential,
            &bound.config,
        )
        .unwrap();
    assert_eq!(inflight.egress_snapshot().unwrap().proxy_version, Some(1));
    let updated = database
        .update_network_proxy(
            id,
            UpdateProxy {
                tenant_external_id: external.clone(),
                expected_version: 1,
                name: "Primary".into(),
                proxy_url: Some(BACKUP.into()),
                probe: None,
            },
            None,
            KEY,
        )
        .await
        .unwrap();
    assert_eq!(updated["version"], 2);
    assert_eq!(
        database
            .get_transport_group(group_id, &external, KEY)
            .await
            .unwrap()["version"],
        2
    );
    runtime
        .synchronize_managed_for_test(&database.pool)
        .await
        .unwrap();
    let (current, credential) = database
        .upstream_account_with_credential(account.id, KEY)
        .await
        .unwrap();
    assert!(current.credential_generation > bound.credential_generation);
    let selected = runtime
        .select_config(
            account.id,
            current.credential_generation,
            &credential,
            &current.config,
        )
        .unwrap();
    assert_eq!(selected.credential.proxy().unwrap().0, BACKUP);
    assert_eq!(selected.egress_snapshot().unwrap().proxy_version, Some(2));
    assert_eq!(inflight.credential.proxy().unwrap().0, PRIMARY);
    assert!(matches!(
        database
            .delete_network_proxy(
                id,
                DeleteProxy {
                    tenant_external_id: external.clone(),
                    expected_version: 2
                },
                None
            )
            .await,
        Err(AppError::ProxyGroupConflict(_))
    ));
    let policy: UpdateProxySelection = serde_json::from_value(serde_json::json!({"tenant_external_id":external,"expected_selection_version":0,"expected_binding_version":1,"expected_credential_generation":current.credential_generation,"expected_updated_at":current.updated_at,"mode":"manual","proxy_id":id})).unwrap();
    policy
        .validate(&crate::plugin::PluginRuntime::default())
        .unwrap();
    database
        .update_network_proxy_selection(account.id, policy, None, KEY)
        .await
        .unwrap();
    sqlx::query("UPDATE network_proxies SET last_probe_status = 'unhealthy', last_probe_checked_at = $1 WHERE id = $2").bind(unix_millis()).bind(id.to_string()).execute(&database.pool).await.unwrap();
    runtime
        .synchronize_managed_for_test(&database.pool)
        .await
        .unwrap();
    runtime
        .evaluate_strategies(&database.pool, &crate::plugin::PluginRuntime::default())
        .await
        .unwrap();
    let (manual, credential) = database
        .upstream_account_with_credential(account.id, KEY)
        .await
        .unwrap();
    let mut selected = runtime
        .select_config(
            account.id,
            manual.credential_generation,
            &credential,
            &manual.config,
        )
        .unwrap();
    assert_eq!(selected.member_count(), 1);
    assert!(selected.select_unattempted(&[0]).unwrap());
    assert_eq!(selected.credential.proxy().unwrap().0, BACKUP);
    let observations = database
        .network_proxy_observations(id, &external, 1, 2, KEY)
        .await
        .unwrap();
    assert!(observations["request_outcomes"]["transport_success_rate"].is_null());
    assert!(observations["traffic"]["request_payload_bytes"].is_null());
    assert!(observations["traffic"]["wire_bytes"].is_null());
    assert!(
        database
            .request_network_proxy_probe(id, &external, 1)
            .await
            .is_err()
    );
    assert_eq!(
        database
            .request_network_proxy_probe(id, &external, 2)
            .await
            .unwrap()["queued"],
        true
    );
    let changed = database
        .update_network_proxy(
            id,
            UpdateProxy {
                tenant_external_id: external.clone(),
                expected_version: 2,
                name: "Primary".into(),
                proxy_url: None,
                probe: None,
            },
            None,
            KEY,
        )
        .await
        .unwrap();
    assert_eq!(changed["version"], 3);
    let (_, credential) = database
        .upstream_account_with_credential(account.id, KEY)
        .await
        .unwrap();
    assert_eq!(credential.proxy().unwrap().0, BACKUP);
    legacy_contract(database, &external).await;
}

async fn legacy_contract(database: &Database, external: &str) {
    let tenant: String = sqlx::query_scalar("SELECT id FROM tenants WHERE external_id = $1")
        .bind(external)
        .fetch_one(&database.pool)
        .await
        .unwrap();
    let group_id = Uuid::now_v7();
    let member = Member {
        id: Uuid::now_v7(),
        label: "Old proxy".into(),
        proxy_url: PRIMARY.into(),
        version: 1,
    };
    let ciphertext = crate::plain_json::encode_json(&vec![member.clone()]).unwrap();
    sqlx::query("INSERT INTO transport_proxy_groups (id, tenant_id, name, version, members_ciphertext, created_at, updated_at) VALUES ($1,$2,$3,1,$4,$5,$5)")
        .bind(group_id.to_string()).bind(&tenant).bind(group_id.to_string()).bind(ciphertext).bind(unix_millis()).execute(&database.pool).await.unwrap();
    for _ in 0..2 {
        normalize_legacy_members(&database.pool, KEY).await.unwrap();
    }
    let proxy = database
        .get_network_proxy(member.id, external, KEY)
        .await
        .unwrap();
    assert_eq!(proxy["probe"]["enabled"], false);
    let loaded = load_group_members(&database.pool, &group_id.to_string(), KEY)
        .await
        .unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].proxy_url, PRIMARY);
    assert_eq!(loaded[0].id, member.id);
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM network_proxy_group_members WHERE group_id = $1")
            .bind(group_id.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn probe_targets_reject_credentials_fragments_and_unbounded_configuration() {
    assert!(validate_probe(&ProxyProbe::default()).is_ok());
    for target in [
        "https://user:secret@example.com/",
        "https://example.com/#secret",
        "file:///tmp/probe",
    ] {
        assert!(
            validate_probe(&ProxyProbe {
                target_url: target.into(),
                ..Default::default()
            })
            .is_err()
        );
    }
}

#[tokio::test]
async fn sqlite_network_proxy_canonical_crud_manual_and_migration_contract() {
    let directory = tempfile::tempdir().unwrap();
    let database = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("network-proxies.db").display()
    ))
    .await
    .unwrap();
    database.migrate().await.unwrap();
    contract(&database).await;
}

#[tokio::test]
async fn postgres_network_proxy_canonical_crud_manual_and_migration_contract() {
    let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let database = Database::connect(&url).await.unwrap();
    database.migrate().await.unwrap();
    contract(&database).await;
}
