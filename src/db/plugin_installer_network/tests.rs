use super::*;

const KEY: &[u8] = b"installer-network-fixture-key-32-bytes";

fn custom(password: Option<&str>) -> InstallerNetworkInput {
    InstallerNetworkInput::Custom {
        url: "http://203.0.113.2:3128".into(),
        username: Some("fixture-user".into()),
        password: password.map(str::to_owned),
    }
}

#[test]
fn installer_network_schema_and_redaction() {
    let input = custom(Some("fixture-secret"));
    let view = input.view();
    assert_eq!(view["has_password"], true);
    assert!(view.get("password").is_none());
    assert!(!view.to_string().contains("fixture-secret"));
    assert!(
        serde_json::from_value::<InstallerNetworkInput>(
            serde_json::json!({"mode":"direct","url":"http://example.org"})
        )
        .is_err()
    );
    assert!(serde_json::from_value::<InstallerNetworkInput>(serde_json::json!({"mode":"custom","url":"http://example.org","password_file":"/secret"})).is_err());
    assert!(
        serde_json::from_value::<InstallerNetworkInput>(
            serde_json::json!({"mode":"default","password":"unexpected"})
        )
        .is_err()
    );
    assert_ne!(
        selection_digest(&input, KEY).unwrap(),
        selection_digest(&input, b"another-key").unwrap()
    );
}

async fn contract(database: &Database) {
    // Explicit public and private literal proxies work without connecting to
    // them during preference selection. Actual download failure must be surfaced
    // by the downloader and must never select a direct fallback.
    let actor = format!("service:{}", Uuid::now_v7());
    let other_actor = format!("service:{}", Uuid::now_v7());
    let input = InstallerNetworkInput::Custom {
        url: "http://8.8.8.8:3128".into(),
        username: Some("fixture-user".into()),
        password: Some("fixture-secret".into()),
    };
    let view = database
        .put_installer_network_preference(&actor, input.clone(), KEY)
        .await
        .unwrap();
    assert_eq!(view["has_password"], true);
    assert!(view.get("password").is_none());
    let stored_preference: String = sqlx::query_scalar(
        "SELECT preference_ciphertext FROM plugin_installer_network_preferences WHERE actor=$1",
    )
    .bind(&actor)
    .fetch_one(&database.pool)
    .await
    .unwrap();
    // The legacy column name holds plaintext JSON; the public view stays redacted.
    assert_eq!(
        serde_json::from_str::<Value>(&stored_preference).unwrap(),
        serde_json::to_value(&input).unwrap()
    );
    assert!(!view.to_string().contains("fixture-secret"));
    assert!(matches!(
        database
            .installer_network_preference(&other_actor, KEY)
            .await
            .unwrap(),
        InstallerNetworkInput::Default
    ));
    let operation = format!("installer-{}", Uuid::now_v7());
    let snapshot = database
        .freeze_installer_network(&operation, &actor, None, KEY)
        .await
        .unwrap();
    assert_eq!(snapshot.password.as_deref(), Some("fixture-secret"));
    assert_eq!(
        snapshot.proxy_addresses,
        vec!["8.8.8.8:3128".parse::<SocketAddr>().unwrap()]
    );
    database
        .put_installer_network_preference(&actor, InstallerNetworkInput::Direct, KEY)
        .await
        .unwrap();
    let retry = database
        .freeze_installer_network(&operation, &actor, None, KEY)
        .await
        .unwrap();
    assert_eq!(retry.password.as_deref(), Some("fixture-secret"));
    assert_eq!(retry.proxy_addresses, snapshot.proxy_addresses);
    assert!(matches!(
        database
            .freeze_installer_network(&operation, &actor, Some(InstallerNetworkInput::Direct), KEY)
            .await,
        Err(AppError::Conflict(_))
    ));
    assert!(
        database
            .load_installer_network(&operation, &other_actor, KEY)
            .await
            .is_err()
    );
    assert!(
        database
            .freeze_installer_network(&operation, &other_actor, None, KEY)
            .await
            .is_err()
    );
    let copied_operation = format!("installer-{}", Uuid::now_v7());
    let copied = database
        .freeze_installer_network_from(&copied_operation, &actor, &operation, KEY)
        .await
        .unwrap();
    assert_eq!(copied.proxy_url, snapshot.proxy_url);
    assert_eq!(copied.password, snapshot.password);
    assert_eq!(copied.proxy_addresses, snapshot.proxy_addresses);
    // Plaintext reads remain actor-bound and do not depend on the storage key.
    let reloaded = database
        .load_installer_network(&operation, &actor, b"wrong-key")
        .await
        .unwrap();
    assert_eq!(reloaded.password, snapshot.password);
    assert_eq!(reloaded.proxy_addresses, snapshot.proxy_addresses);
    // Explicit selection replay still verifies the keyed selection digest.
    assert!(matches!(
        database
            .freeze_installer_network(&operation, &actor, Some(input.clone()), b"wrong-key")
            .await,
        Err(AppError::Conflict(_))
    ));
    // Password omission keeps a credential only for the same URL and identity.
    database
        .put_installer_network_preference(&actor, input.clone(), KEY)
        .await
        .unwrap();
    let mut omitted = input;
    if let InstallerNetworkInput::Custom { password, .. } = &mut omitted {
        *password = None;
    }
    database
        .put_installer_network_preference(&actor, omitted.clone(), KEY)
        .await
        .unwrap();
    let remembered = database
        .installer_network_preference(&actor, KEY)
        .await
        .unwrap();
    assert_eq!(remembered.view()["has_password"], true);
    if let InstallerNetworkInput::Custom { url, username, .. } = &mut omitted {
        *url = "https://1.1.1.1:443".into();
        *username = None;
    }
    database
        .put_installer_network_preference(&actor, omitted, KEY)
        .await
        .unwrap();
    assert_eq!(
        database
            .installer_network_preference(&actor, KEY)
            .await
            .unwrap()
            .view()["has_password"],
        false
    );
    group_contract(database).await;
    deployment_default_contract(database).await;
}

async fn deployment_default_contract(database: &Database) {
    let actor = format!("service:{}", Uuid::now_v7());
    let operation = format!("default-{}", Uuid::now_v7());
    let fallback = InstallerNetworkInput::Custom {
        url: "http://10.20.30.40:3128".into(),
        username: Some("deployment-user".into()),
        password: Some("deployment-secret".into()),
    };
    let frozen = database
        .freeze_installer_network_with_default(
            &operation,
            &actor,
            Some(InstallerNetworkInput::Default),
            Some(fallback),
            KEY,
        )
        .await
        .unwrap();
    assert_eq!(frozen.proxy_url.as_deref(), Some("http://10.20.30.40:3128"));
    assert_eq!(frozen.password.as_deref(), Some("deployment-secret"));
    assert!(matches!(
        database
            .installer_network_preference(&actor, KEY)
            .await
            .unwrap(),
        InstallerNetworkInput::Default
    ));
    let retry = database
        .freeze_installer_network_with_default(
            &operation,
            &actor,
            None,
            Some(InstallerNetworkInput::Direct),
            KEY,
        )
        .await
        .unwrap();
    assert_eq!(retry.password, frozen.password);
    let direct = database
        .freeze_installer_network_with_default(
            &format!("direct-{}", Uuid::now_v7()),
            &actor,
            Some(InstallerNetworkInput::Direct),
            Some(InstallerNetworkInput::Custom {
                url: "http://10.20.30.40:3128".into(),
                username: None,
                password: None,
            }),
            KEY,
        )
        .await
        .unwrap();
    assert!(direct.proxy_url.is_none());
}

async fn group_contract(database: &Database) {
    use crate::db::transport_proxy_management::{CreateGroup, MemberInput, UpdateGroup};
    let actor = format!("service:{}", Uuid::now_v7());
    let tenant = format!("installer-{}", Uuid::now_v7());
    let other_tenant = format!("installer-{}", Uuid::now_v7());
    database.create_tenant(&tenant, None).await.unwrap();
    database.create_tenant(&other_tenant, None).await.unwrap();
    let group = database
        .create_transport_group(
            CreateGroup {
                tenant_external_id: tenant.clone(),
                name: "installer".into(),
                members: vec![MemberInput {
                    id: None,
                    label: "primary".into(),
                    proxy_url: Some("socks5h://proxyuser:proxysecret@10.20.30.40:1080".into()),
                }],
                probe_url: None,
                probe_attempts: None,
            },
            None,
            KEY,
        )
        .await
        .unwrap();
    let group_id = Uuid::parse_str(group["id"].as_str().unwrap()).unwrap();
    let member_id = Uuid::parse_str(group["members"][0]["id"].as_str().unwrap()).unwrap();
    let input = InstallerNetworkInput::Group {
        tenant_external_id: tenant.clone(),
        group_id,
        group_version: 1,
        member_id,
    };
    let operation = format!("group-{}", Uuid::now_v7());
    let snapshot = database
        .freeze_installer_network(&operation, &actor, Some(input.clone()), KEY)
        .await
        .unwrap();
    assert_eq!(
        snapshot.proxy_url.as_deref(),
        Some("socks5h://10.20.30.40:1080")
    );
    assert_eq!(snapshot.username.as_deref(), Some("proxyuser"));
    assert_eq!(snapshot.password.as_deref(), Some("proxysecret"));
    assert_eq!(
        snapshot
            .group
            .as_ref()
            .unwrap()
            .reference
            .expected_group_version,
        1
    );
    let cross_tenant = InstallerNetworkInput::Group {
        tenant_external_id: other_tenant,
        group_id,
        group_version: 1,
        member_id,
    };
    assert!(matches!(
        database
            .freeze_installer_network(
                &format!("cross-{}", Uuid::now_v7()),
                &actor,
                Some(cross_tenant),
                KEY
            )
            .await,
        Err(AppError::NotFound)
    ));
    database
        .update_transport_group(
            group_id,
            UpdateGroup {
                tenant_external_id: tenant,
                expected_version: 1,
                name: "installer-renamed".into(),
                members: vec![MemberInput {
                    id: Some(member_id),
                    label: "primary".into(),
                    proxy_url: None,
                }],
                replacement_member_id: None,
                probe_url: None,
                probe_attempts: None,
            },
            None,
            KEY,
        )
        .await
        .unwrap();
    // Existing operations retain their original version/member/credentials;
    // a fresh operation with a stale group version cannot silently reselect.
    let retry = database
        .load_installer_network(&operation, &actor, KEY)
        .await
        .unwrap();
    assert_eq!(
        retry
            .group
            .as_ref()
            .unwrap()
            .reference
            .expected_group_version,
        1
    );
    assert_eq!(retry.password.as_deref(), Some("proxysecret"));
    assert!(
        database
            .freeze_installer_network(
                &format!("stale-{}", Uuid::now_v7()),
                &actor,
                Some(input),
                KEY
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn sqlite_installer_network_plaintext_remember_and_frozen_retry() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("installer-network.db").display()
    );
    let database = Database::connect(&url).await.unwrap();
    database.migrate().await.unwrap();
    contract(&database).await;
    drop(database);
    // A fresh connection reads the durable preference after a simulated Pod
    // replacement, without relying on process state or browser local storage.
    let database = Database::connect(&url).await.unwrap();
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM plugin_installer_network_preferences")
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(count, 3);
}

#[tokio::test]
async fn postgres_installer_network_plaintext_remember_and_frozen_retry() {
    let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let database = Database::connect(&url).await.unwrap();
    database.migrate().await.unwrap();
    contract(&database).await;
}
