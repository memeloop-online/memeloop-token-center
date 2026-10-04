use super::*;

const KEY: &[u8] = b"managed-proxy-fixture-key-32-bytes";
const PRIMARY: &str = "socks5h://10.20.30.40:1080";
const BACKUP: &str = "socks5h://10.20.30.41:1080";

fn input(label: &str, proxy: &str) -> MemberInput {
    MemberInput {
        id: None,
        label: label.into(),
        proxy_url: Some(proxy.into()),
    }
}

#[test]
fn member_identity_validation_and_service_dns_contract() {
    let original = members(vec![input("primary", PRIMARY)], &[]).unwrap();
    assert!(
        members(
            vec![MemberInput {
                id: Some(Uuid::now_v7()),
                label: "unknown".into(),
                proxy_url: None
            }],
            &original
        )
        .is_err()
    );
    assert!(members(vec![input("first", PRIMARY), input("second", PRIMARY)], &[]).is_err());
    assert!(serde_json::from_value::<CreateGroup>(serde_json::json!({"tenant_external_id":"tenant","name":"group","members":[],"unexpected":true})).is_err());
    for address in [
        "socks5h://mihomo.egress.svc:1080",
        "socks5h://mihomo.egress.svc.cluster.local:1080",
    ] {
        crate::provider::validate_codex_proxy_url(address).unwrap();
    }
    for address in [
        "socks5h://public.example:1080",
        "socks5h://169.254.169.254:1080",
        "socks5h://127.0.0.1:1080",
        "socks5://mihomo.egress.svc:1080",
        "socks5h://fake.svc.cluster.local.attacker.example:1080",
    ] {
        assert!(crate::provider::validate_codex_proxy_url(address).is_err());
    }
}

async fn contract(database: &Database) {
    let tenant = format!("managed-proxy-{}", Uuid::now_v7());
    let account = database
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: tenant.clone(),
                name: "managed-proxy".into(),
                driver: "openai-codex".into(),
                config: serde_json::json!({"base_url":"https://chatgpt.com/backend-api/codex"}),
                credential: UpstreamCredential::OAuth {
                    access_token: "fixture-access".into(),
                    refresh_token: Some("fixture-refresh".into()),
                    expires_at: Some(i64::MAX),
                    header: "authorization".into(),
                    prefix: "Bearer ".into(),
                    adapter_state: None,
                    proxy_url: Some(PRIMARY.into()),
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
    let group = database
        .create_transport_group(
            CreateGroup {
                tenant_external_id: tenant.clone(),
                name: "egress".into(),
                members: vec![input("first", PRIMARY), input("second", BACKUP)],
            },
            None,
            KEY,
        )
        .await
        .unwrap();
    assert!(!group.to_string().contains("10.20.30"));
    let unrelated = database
        .create_transport_group(
            CreateGroup {
                tenant_external_id: tenant.clone(),
                name: "revision-gap".into(),
                members: vec![input("primary", PRIMARY)],
            },
            None,
            KEY,
        )
        .await
        .unwrap();
    database
        .delete_transport_group(
            Uuid::parse_str(unrelated["id"].as_str().unwrap()).unwrap(),
            DeleteGroup {
                tenant_external_id: tenant.clone(),
                expected_version: 1,
            },
            None,
        )
        .await
        .unwrap();
    let id = Uuid::parse_str(group["id"].as_str().unwrap()).unwrap();
    let primary = Uuid::parse_str(group["members"][0]["id"].as_str().unwrap()).unwrap();
    let backup = Uuid::parse_str(group["members"][1]["id"].as_str().unwrap()).unwrap();
    assert!(matches!(
        database.get_transport_group(id, "other-tenant", KEY).await,
        Err(AppError::NotFound)
    ));
    let binding = database
        .bind_transport_group(
            account.id,
            BindGroup {
                tenant_external_id: tenant.clone(),
                group_id: id,
                expected_group_version: 1,
                initial_member_id: primary,
                expected_binding_version: 0,
                expected_credential_generation: account.credential_generation,
                expected_updated_at: account.updated_at,
            },
            None,
            KEY,
        )
        .await
        .unwrap();
    assert_eq!(binding["binding_version"], 1);
    let runtime = TransportProxyGroups::parse("[]", KEY).unwrap();
    let (bound_account, bound_credential) = database
        .upstream_account_with_credential(account.id, KEY)
        .await
        .unwrap();
    assert_ne!(
        bound_account.config[CONFIG_KEY]["selection_version"],
        bound_account.credential_generation
    );
    assert!(
        runtime
            .select_config(
                account.id,
                bound_account.credential_generation,
                &bound_credential,
                &bound_account.config
            )
            .is_err()
    );
    runtime
        .synchronize_managed_for_test(&database.pool)
        .await
        .unwrap();
    let first = runtime
        .select_config(
            account.id,
            bound_account.credential_generation,
            &bound_credential,
            &bound_account.config,
        )
        .unwrap();
    assert_eq!(first.credential.proxy().unwrap().0, PRIMARY);
    assert!(first.advance_after_connect_failure(&[0]).unwrap());
    runtime
        .synchronize_managed_for_test(&database.pool)
        .await
        .unwrap();
    let fingerprint: String = sqlx::query_scalar(
        "SELECT group_fingerprint FROM upstream_transport_proxy_selections WHERE account_id = $1",
    )
    .bind(account.id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    let refreshed_account = database
        .rotate_upstream_credential(
            account.id,
            bound_credential.clone(),
            &format!("fixture-{}", Uuid::now_v7()),
            KEY,
        )
        .await
        .unwrap();
    runtime
        .synchronize_managed_for_test(&database.pool)
        .await
        .unwrap();
    let refreshed_fingerprint: String = sqlx::query_scalar(
        "SELECT group_fingerprint FROM upstream_transport_proxy_selections WHERE account_id = $1",
    )
    .bind(account.id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(fingerprint, refreshed_fingerprint);
    assert_eq!(
        refreshed_account.config[CONFIG_KEY],
        bound_account.config[CONFIG_KEY]
    );
    let cold_pod = TransportProxyGroups::parse("[]", KEY).unwrap();
    cold_pod.refresh_managed(&database.pool).await.unwrap();
    assert_eq!(
        cold_pod
            .select_config(
                account.id,
                refreshed_account.credential_generation,
                &bound_credential,
                &bound_account.config
            )
            .unwrap()
            .credential
            .proxy()
            .unwrap()
            .0,
        BACKUP
    );
    assert_eq!(
        binding["credential_generation"],
        account.credential_generation + 1
    );
    assert!(matches!(
        database
            .rotate_codex_transport_proxy(
                account.id,
                &tenant,
                BACKUP.into(),
                binding["updated_at"].as_i64().unwrap(),
                binding["credential_generation"].as_i64().unwrap(),
                "bypass",
                None,
                KEY
            )
            .await,
        Err(AppError::ProxyGroupConflict("proxy_group_in_use"))
    ));
    let unchanged = |replacement| UpdateGroup {
        tenant_external_id: tenant.clone(),
        expected_version: 1,
        name: "egress".into(),
        members: vec![MemberInput {
            id: Some(backup),
            label: "second".into(),
            proxy_url: None,
        }],
        replacement_member_id: replacement,
    };
    assert!(
        database
            .update_transport_group(id, unchanged(None), None, KEY)
            .await
            .is_err()
    );
    assert_eq!(
        database
            .get_transport_group(id, &tenant, KEY)
            .await
            .unwrap()["version"],
        1
    );
    database
        .update_transport_group(id, unchanged(Some(backup)), None, KEY)
        .await
        .unwrap();
    assert!(matches!(
        database
            .update_transport_group(id, unchanged(Some(backup)), None, KEY)
            .await,
        Err(AppError::ProxyGroupConflict("proxy_group_version_conflict"))
    ));
    let binding = database
        .get_transport_binding(account.id, &tenant)
        .await
        .unwrap();
    assert_eq!(binding["initial_member_id"], backup.to_string());
    assert_eq!(binding["group_version"], 2);
    let (edited_account, edited_credential) = database
        .upstream_account_with_credential(account.id, KEY)
        .await
        .unwrap();
    assert!(
        runtime
            .select_config(
                account.id,
                edited_account.credential_generation,
                &edited_credential,
                &edited_account.config
            )
            .is_err()
    );
    runtime
        .synchronize_managed_for_test(&database.pool)
        .await
        .unwrap();
    assert!(
        runtime
            .select_config(
                account.id,
                bound_account.credential_generation,
                &bound_credential,
                &bound_account.config
            )
            .is_err()
    );
    assert_eq!(
        runtime
            .select_config(
                account.id,
                edited_account.credential_generation,
                &edited_credential,
                &edited_account.config
            )
            .unwrap()
            .credential
            .proxy()
            .unwrap()
            .0,
        BACKUP
    );
    assert_eq!(first.credential.proxy().unwrap().0, PRIMARY);
    assert!(!first.advance_after_connect_failure(&[0]).unwrap());
    let cold_after_edit = TransportProxyGroups::parse("[]", KEY).unwrap();
    cold_after_edit
        .refresh_managed(&database.pool)
        .await
        .unwrap();
    assert_eq!(
        cold_after_edit
            .select_config(
                account.id,
                edited_account.credential_generation,
                &edited_credential,
                &edited_account.config
            )
            .unwrap()
            .credential
            .proxy()
            .unwrap()
            .0,
        BACKUP
    );
    let unbound = database
        .unbind_transport_group(
            account.id,
            UnbindGroup {
                tenant_external_id: tenant.clone(),
                expected_group_version: 2,
                expected_binding_version: 1,
                expected_credential_generation: binding["credential_generation"].as_i64().unwrap(),
                expected_updated_at: binding["updated_at"].as_i64().unwrap(),
                single_proxy_member_id: backup,
            },
            None,
            KEY,
        )
        .await
        .unwrap();
    assert_eq!(unbound["binding_version"], 2);
    assert!(unbound["group_id"].is_null());
    let (snapshot, credential) = database
        .upstream_account_with_credential(account.id, KEY)
        .await
        .unwrap();
    assert_eq!(credential.proxy().unwrap().0, BACKUP);
    assert_eq!(snapshot.config[CONFIG_KEY]["binding_version"], 2);
    let legacy = TransportProxyGroups::parse(
        &serde_json::json!([{"account_id":account.id,"version":999,"proxies":[PRIMARY,BACKUP]}])
            .to_string(),
        KEY,
    )
    .unwrap();
    let selection = legacy
        .select_config(
            account.id,
            snapshot.credential_generation,
            &credential,
            &snapshot.config,
        )
        .unwrap();
    assert_eq!(selection.credential.proxy().unwrap().0, BACKUP);
    assert!(selection.member().is_none());
    database
        .delete_transport_group(
            id,
            DeleteGroup {
                tenant_external_id: tenant,
                expected_version: 2,
            },
            None,
        )
        .await
        .unwrap();
    let audits: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM transport_proxy_management_audit WHERE resource_id = $1",
    )
    .bind(account.id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(audits, 2);
}

#[tokio::test]
async fn sqlite_management_transaction_and_tombstone_contract() {
    let directory = tempfile::tempdir().unwrap();
    let database = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("management.db").display()
    ))
    .await
    .unwrap();
    database.migrate().await.unwrap();
    contract(&database).await;
    database.migrate().await.unwrap();
}

#[tokio::test]
async fn postgres_management_transaction_and_tombstone_contract() {
    let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let database = Database::connect(&url).await.unwrap();
    database.migrate().await.unwrap();
    contract(&database).await;
}
