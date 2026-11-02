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
    assert_eq!(proxy["group_ids"], serde_json::json!([]));
    assert_eq!(proxy["bound_account_count"], 0);
    assert_list_references(database, &external).await;
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
    let shared_group = database
        .create_transport_group(
            serde_json::from_value(serde_json::json!({
                "tenant_external_id":external,"name":"Shared reference","proxy_ids":[id]
            }))
            .unwrap(),
            None,
            KEY,
        )
        .await
        .unwrap();
    let shared_group_id = parse_uuid(shared_group["id"].as_str().unwrap().to_owned()).unwrap();
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
    assert_list_references(database, &external).await;
    let grouped = database
        .get_network_proxy(id, &external, KEY)
        .await
        .unwrap();
    let mut groups = vec![group_id.to_string(), shared_group_id.to_string()];
    groups.sort();
    assert_eq!(grouped["group_ids"], serde_json::json!(groups));
    assert_eq!(grouped["bound_account_count"], 1);
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
            .get_transport_group(shared_group_id, &external, KEY)
            .await
            .unwrap()["version"],
        2
    );
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
    let mut applied = database
        .get_network_proxy_selection(account.id, &external)
        .await
        .unwrap();
    runtime.describe_network_proxy_selection(account.id, &mut applied);
    assert_eq!(applied["applied"]["selected_proxy_id"], id.to_string());
    assert_eq!(
        database
            .get_network_proxy(id, &external, KEY)
            .await
            .unwrap()["bound_account_count"],
        1
    );
    reference_contract(database, &external, id, account.id, group_id).await;
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
    // Policy version alone is unchanged by entity edits: old applied status must be pending.
    let mut pending = database
        .get_network_proxy_selection(account.id, &external)
        .await
        .unwrap();
    runtime.describe_network_proxy_selection(account.id, &mut pending);
    assert_eq!(pending["applied"]["pending"], true);
    // Restore from DB with a new runtime, not the in-memory selector used before the edit.
    let restarted = TransportProxyGroups::parse("[]", KEY).unwrap();
    restarted
        .synchronize_managed_for_test(&database.pool)
        .await
        .unwrap();
    restarted
        .evaluate_strategies(&database.pool, &crate::plugin::PluginRuntime::default())
        .await
        .unwrap();
    let (restored, credential) = database
        .upstream_account_with_credential(account.id, KEY)
        .await
        .unwrap();
    let restored_selection = restarted
        .select_config(
            account.id,
            restored.credential_generation,
            &credential,
            &restored.config,
        )
        .unwrap();
    assert_eq!(restored_selection.credential.proxy().unwrap().0, BACKUP);
    assert_eq!(
        restored_selection.egress_snapshot().unwrap().proxy_version,
        Some(3)
    );
    let stale_policy = serde_json::json!({"tenant_external_id":external,
        "expected_selection_version":1,"expected_binding_version":pending["binding_version"],
        "expected_credential_generation":manual.credential_generation,"expected_updated_at":manual.updated_at,
        "mode":"manual","proxy_id":id});
    assert!(matches!(
        database
            .update_network_proxy_selection(
                account.id,
                serde_json::from_value(stale_policy).unwrap(),
                None,
                KEY
            )
            .await,
        Err(AppError::ProxyGroupConflict(_))
    ));
    let rejected = serde_json::json!({"tenant_external_id":"other-tenant",
        "expected_selection_version":1,"expected_binding_version":pending["binding_version"],
        "expected_credential_generation":restored.credential_generation,"expected_updated_at":restored.updated_at,
        "mode":"manual","proxy_id":id});
    assert!(matches!(
        database
            .update_network_proxy_selection(
                account.id,
                serde_json::from_value(rejected).unwrap(),
                None,
                KEY
            )
            .await,
        Err(AppError::NotFound)
    ));
    let current_group = database
        .get_transport_group(group_id, &external, KEY)
        .await
        .unwrap();
    database.update_transport_group(group_id, serde_json::from_value(serde_json::json!({
        "tenant_external_id":external,"expected_version":current_group["version"],"name":"Reusable",
        "proxy_ids":[id],"probe_url":"https://example.com/check","probe_attempts":3
    })).unwrap(), None, KEY).await.unwrap();
    let probe_config = database
        .get_network_proxy(id, &external, KEY)
        .await
        .unwrap();
    assert_eq!(
        probe_config["probe"]["target_url"],
        "https://example.com/check"
    );
    assert_eq!(probe_config["probe"]["attempts"], 3);
    assert_eq!(probe_config["probe"]["enabled"], true);
    assert!(probe_config["active_health"]["checked_at"].is_null());
    let latest = database
        .get_network_proxy_selection(account.id, &external)
        .await
        .unwrap();
    let group = database
        .get_transport_group(group_id, &external, KEY)
        .await
        .unwrap();
    let automatic = serde_json::json!({"tenant_external_id":external,
        "expected_selection_version":latest["selection_version"],"expected_binding_version":latest["binding_version"],
        "expected_credential_generation":latest["credential_generation"],"expected_updated_at":latest["account_updated_at"],
        "expected_group_version":group["version"],"mode":"auto","group_id":group_id,
        "config":{"minimum_dwell_ms":0,"recovery_successes":1}});
    let automatic: UpdateProxySelection = serde_json::from_value(automatic).unwrap();
    automatic
        .validate(&crate::plugin::PluginRuntime::default())
        .unwrap();
    database
        .update_network_proxy_selection(account.id, automatic, None, KEY)
        .await
        .unwrap();
    restarted
        .synchronize_managed_for_test(&database.pool)
        .await
        .unwrap();
    restarted
        .evaluate_strategies(&database.pool, &crate::plugin::PluginRuntime::default())
        .await
        .unwrap();
    let mut state = database
        .get_network_proxy_selection(account.id, &external)
        .await
        .unwrap();
    restarted.describe_network_proxy_selection(account.id, &mut state);
    assert_eq!(state["desired"]["mode"], "auto");
    assert_eq!(state["applied"]["selected_proxy_id"], id.to_string());
    signal_contract(database, id, account.id, &external).await;
    legacy_contract(database, &external).await;
}

async fn assert_list_references(database: &Database, external: &str) {
    let listed = database.list_network_proxies(external, KEY).await.unwrap();
    for item in listed["items"].as_array().unwrap() {
        let id = parse_uuid(item["id"].as_str().unwrap().to_owned()).unwrap();
        let individual = database.get_network_proxy(id, external, KEY).await.unwrap();
        assert!(item["group_ids"].is_array());
        assert!(item["bound_account_count"].is_i64());
        assert_eq!(item, &individual);
    }
}

async fn reference_contract(
    database: &Database,
    external: &str,
    proxy: Uuid,
    account: Uuid,
    group: Uuid,
) {
    assert_list_references(database, external).await;
    sqlx::query("UPDATE transport_proxy_bindings SET group_id = $1 WHERE account_id = $2")
        .bind(group.to_string())
        .bind(account.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    assert_eq!(
        database
            .get_network_proxy(proxy, external, KEY)
            .await
            .unwrap()["bound_account_count"],
        1
    );
    assert_list_references(database, external).await;
    sqlx::query("UPDATE transport_proxy_bindings SET group_id = NULL WHERE account_id = $1")
        .bind(account.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    let unused = database
        .create_network_proxy(
            CreateProxy {
                tenant_external_id: external.into(),
                name: "Unbound".into(),
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
    assert_eq!(unused["group_ids"], serde_json::json!([]));
    assert_eq!(unused["bound_account_count"], 0);
    let other = format!("other-{}", Uuid::now_v7());
    database.create_tenant(&other, None).await.unwrap();
    let foreign_group = database
        .create_transport_group(
            serde_json::from_value(serde_json::json!({
                "tenant_external_id":other, "name":"Other tenant",
                "members":[{"label":"Other", "proxy_url":PRIMARY}]
            }))
            .unwrap(),
            None,
            KEY,
        )
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO network_proxy_group_members (group_id, proxy_id, position) VALUES ($1, $2, 1)",
    )
    .bind(foreign_group["id"].as_str().unwrap())
    .bind(proxy.to_string())
    .execute(&database.pool)
    .await
    .unwrap();
    let own = database
        .get_network_proxy(proxy, external, KEY)
        .await
        .unwrap();
    assert_eq!(own["group_ids"].as_array().unwrap().len(), 2);
    assert_eq!(own["bound_account_count"], 1);
    assert_list_references(database, external).await;
    assert_list_references(database, &other).await;
    let own_list = database.list_network_proxies(external, KEY).await.unwrap();
    assert_eq!(own_list["items"].as_array().unwrap().len(), 2);
    let other_list = database.list_network_proxies(&other, KEY).await.unwrap();
    assert_eq!(other_list["items"].as_array().unwrap().len(), 1);
    assert_ne!(other_list["items"][0]["id"], proxy.to_string());
    assert_eq!(other_list["items"][0]["bound_account_count"], 0);
    sqlx::query("DELETE FROM network_proxy_group_members WHERE group_id = $1 AND proxy_id = $2")
        .bind(foreign_group["id"].as_str().unwrap())
        .bind(proxy.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    let missing = database
        .list_network_proxies("missing-tenant", KEY)
        .await
        .unwrap();
    assert_eq!(missing["items"], serde_json::json!([]));
}

async fn signal_contract(database: &Database, proxy: Uuid, account: Uuid, external: &str) {
    use crate::db::egress::{RequestEgressAttempt, SelectedEgress};
    let tenant: String = sqlx::query_scalar("SELECT id FROM tenants WHERE external_id = $1")
        .bind(external)
        .fetch_one(&database.pool)
        .await
        .unwrap();
    let version = database
        .get_network_proxy(proxy, external, KEY)
        .await
        .unwrap()["version"]
        .as_i64()
        .unwrap();
    for (status, error, complete, connection) in [
        (200_i64, None, true, false),
        (429, Some("upstream_rate_limited"), true, false),
        (499, Some("client_cancelled"), true, false),
        (200, None, false, false),
        (502, Some("upstream_transport_error"), false, true),
    ] {
        let request = Uuid::now_v7();
        let mut selected = SelectedEgress::capture(Some(BACKUP)).unwrap();
        selected.member_id = Some(proxy);
        selected.proxy_version = Some(version);
        let attempt = RequestEgressAttempt {
            request_id: request,
            attempt_id: Uuid::now_v7(),
            outbound_attempt: 1,
            connect_attempt: 1,
            route_id: Uuid::now_v7(),
            upstream_account_id: account,
            credential_generation: 1,
            transport_revision: 1,
            observed_at: 10,
            selected,
            transport: serde_json::json!({}),
            connection_binding: None,
            request_payload_bytes: Some(10),
            response_payload_bytes: Some(20),
            completed: true,
            final_response: true,
            response_body_complete: complete,
            measurement_basis: "application_payload_estimate",
        };
        database
            .record_request_egress_attempt(&attempt)
            .await
            .unwrap();
        database
            .record_request_egress_attempt(&attempt)
            .await
            .unwrap();
        sqlx::query("INSERT INTO proxy_operational_terminals (request_id, request_created_at, tenant_id, key_id, account_id, reservation_id, owner_token, fence, contract_digest, receipt_id, operational_digest, status_code, error_code, completed_at, duration_ms, response_object, upstream_account_id, model_route_id, recorded_at) VALUES ($1,10,$2,'fixture','fixture',$1,'fixture','fixture','fixture','fixture','fixture',$3,$4,11,1,'fixture',$5,$6,11)")
            .bind(request.to_string()).bind(&tenant).bind(status).bind(error).bind(account.to_string()).bind(attempt.route_id.to_string()).execute(&database.pool).await.unwrap();
        if connection {
            sqlx::query("INSERT INTO request_upstream_transport_diagnostics (request_id, route_id, upstream_account_id, credential_generation, transport_revision, failure_kind, failure_stage, gateway_pod, failure_domain, observed_at) VALUES ($1,$2,$3,1,1,'connection','proxy_connect','fixture','fixture',11)")
                .bind(request.to_string()).bind(attempt.route_id.to_string()).bind(account.to_string()).execute(&database.pool).await.unwrap();
        }
    }
    // The actual request outcomes never overwrite the independent probe signal.
    sqlx::query("UPDATE network_proxies SET last_probe_status = 'unhealthy' WHERE id = $1")
        .bind(proxy.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    signals::merge_pending(&database.pool).await.unwrap();
    signals::merge_pending(&database.pool).await.unwrap();
    let result = database
        .network_proxy_observations(proxy, external, 1, 20, KEY)
        .await
        .unwrap();
    assert_eq!(result["active_health"]["status"], "unhealthy");
    for key in [
        "succeeded",
        "provider_failed",
        "transport_failed",
        "cancelled",
        "unknown",
    ] {
        assert_eq!(result["request_outcomes"][key], 1, "{key}: {result}");
    }
    assert_eq!(result["request_outcomes"]["sample_count"], 3);
    assert_eq!(
        result["request_outcomes"]["transport_success_rate"],
        2.0 / 3.0
    );
    assert_eq!(result["traffic"]["recorded_attempts"], 5);
    assert_eq!(result["traffic"]["request_payload_bytes"], 50);
    assert_eq!(result["traffic"]["response_payload_bytes"], 100);
    assert!(matches!(
        database
            .network_proxy_observations(proxy, "other-tenant", 1, 20, KEY)
            .await,
        Err(AppError::NotFound)
    ));
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
