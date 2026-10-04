use super::*;

const KEY: &[u8] = b"sticky-proxy-test-key-at-least-32-bytes";
const PRIMARY: &str = "socks5h://10.20.30.40:1080";
const BACKUP: &str = "socks5h://10.20.30.41:1080";

fn configuration(account_id: Uuid, version: i64) -> String {
    serde_json::json!([{
        "account_id": account_id,
        "version": version,
        "proxies": [PRIMARY, BACKUP]
    }])
    .to_string()
}

fn credential() -> UpstreamCredential {
    UpstreamCredential::OAuth {
        access_token: "access-fixture".into(),
        refresh_token: Some("refresh-fixture".into()),
        expires_at: Some(i64::MAX),
        header: "authorization".into(),
        prefix: "Bearer ".into(),
        adapter_state: None,
        proxy_url: Some(PRIMARY.into()),
        proxy_network_scope: Some(crate::network::OutboundScope::Private),
    }
}

#[test]
fn configuration_is_bounded_private_and_redacted() {
    let account_id = Uuid::nil();
    assert!(TransportProxyGroups::parse("[]", KEY).is_ok());
    for input in [
        "[".to_owned(),
        " ".repeat(256 * 1024 + 1),
        configuration(account_id, 0),
        configuration(account_id, 1).replace(BACKUP, PRIMARY),
        configuration(account_id, 1).replace(BACKUP, "socks5h://public.example:1080"),
        configuration(account_id, 1).replace(BACKUP, "socks5://10.20.30.41:1080"),
        serde_json::json!([{"account_id":account_id,"version":1,"proxies":[]}]).to_string(),
        serde_json::json!([{"account_id":account_id,"version":1,"proxies":[PRIMARY,BACKUP,PRIMARY,BACKUP,PRIMARY]}]).to_string(),
    ] {
        let error = TransportProxyGroups::parse(&input, KEY).err().unwrap();
        assert!(!error.to_string().contains("10.20.30"));
    }
    let duplicate = serde_json::json!([
        {"account_id":account_id,"version":1,"proxies":[PRIMARY]},
        {"account_id":account_id,"version":1,"proxies":[BACKUP]}
    ]);
    assert!(TransportProxyGroups::parse(&duplicate.to_string(), KEY).is_err());
    let excessive: Vec<_> = (0..257)
        .map(|index| {
            serde_json::json!({
                "account_id": Uuid::from_u128(index), "version":1, "proxies":[PRIMARY]
            })
        })
        .collect();
    assert!(TransportProxyGroups::parse(&serde_json::to_string(&excessive).unwrap(), KEY).is_err());
}

async fn contract(database: &Database) {
    let account = database
        .create_upstream_account(
            crate::db::CreateUpstreamAccountInput {
                tenant_external_id: format!("sticky-{}", Uuid::now_v7()),
                name: "sticky-proxy-test".into(),
                driver: "http-json".into(),
                config: serde_json::json!({"base_url":"https://example.com"}),
                credential: credential(),
                oauth_session_id: None,
                oauth_driver: None,
                oauth_refresh_url: None,
            },
            KEY,
        )
        .await
        .unwrap();
    let account_id = account.id;
    let current_generation = account.credential_generation;
    let groups = TransportProxyGroups::parse(&configuration(account_id, 1), KEY).unwrap();
    let other_process = TransportProxyGroups::parse(&configuration(account_id, 1), KEY).unwrap();
    let source = credential();
    let first = groups
        .select(database, account_id, current_generation, &source)
        .await
        .unwrap();
    assert_eq!(first.credential.proxy().unwrap().0, PRIMARY);
    let concurrent = other_process
        .select(database, account_id, current_generation, &source)
        .await
        .unwrap();
    assert_eq!(concurrent.generation, first.generation);
    assert!(
        first
            .advance_after_connect_failure(database, &[0])
            .await
            .unwrap()
    );
    assert!(
        !concurrent
            .advance_after_connect_failure(database, &[0])
            .await
            .unwrap()
    );
    let selected = other_process
        .select(database, account_id, current_generation, &source)
        .await
        .unwrap();
    assert_eq!(selected.credential.proxy().unwrap().0, BACKUP);
    assert!(selected.generation > first.generation);
    for _ in 0..3 {
        let healthy = groups
            .select(database, account_id, current_generation, &source)
            .await
            .unwrap();
        assert_eq!(healthy.generation, selected.generation);
        assert_eq!(healthy.credential.proxy().unwrap().0, BACKUP);
    }
    assert!(
        !selected
            .advance_after_connect_failure(database, &[0, 1])
            .await
            .unwrap()
    );
    sqlx::query("UPDATE upstream_accounts SET credential_generation = credential_generation + 1 WHERE id = $1")
        .bind(account_id.to_string()).execute(&database.pool).await.unwrap();
    assert!(
        !selected
            .advance_after_connect_failure(database, &[1])
            .await
            .unwrap()
    );
    assert!(
        groups
            .select(database, account_id, current_generation, &source)
            .await
            .is_err()
    );
    let refreshed = groups
        .select(database, account_id, current_generation + 1, &source)
        .await
        .unwrap();
    assert_eq!(refreshed.credential.proxy().unwrap().0, BACKUP);
    assert!(refreshed.generation > selected.generation);
    let upgraded = TransportProxyGroups::parse(&configuration(account_id, 2), KEY).unwrap();
    let upgraded_selection = upgraded
        .select(database, account_id, current_generation + 1, &source)
        .await
        .unwrap();
    assert!(upgraded_selection.generation > refreshed.generation);
    assert!(
        groups
            .select(database, account_id, current_generation + 1, &source)
            .await
            .is_err()
    );
    assert!(
        !refreshed
            .advance_after_connect_failure(database, &[1])
            .await
            .unwrap()
    );
    let conflicting = TransportProxyGroups::parse(
        &configuration(account_id, 2).replace(BACKUP, "socks5h://10.20.30.42:1080"),
        KEY,
    )
    .unwrap();
    assert!(
        conflicting
            .select(database, account_id, current_generation + 1, &source)
            .await
            .is_err()
    );
    let single = TransportProxyGroups::parse("[]", KEY).unwrap();
    let unchanged = single
        .select(database, account_id, current_generation + 1, &source)
        .await
        .unwrap();
    assert_eq!(unchanged.credential.proxy(), source.proxy());
    assert_eq!(unchanged.generation, 0);
    assert!(
        !unchanged
            .advance_after_connect_failure(database, &[])
            .await
            .unwrap()
    );
    let direct = groups
        .select(
            database,
            account_id,
            current_generation + 1,
            &UpstreamCredential::None,
        )
        .await
        .unwrap();
    assert!(direct.credential.proxy().is_none());
}

#[tokio::test]
async fn sqlite_sticky_selection_is_shared_and_generation_fenced() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("sticky.db").display()
    );
    let database = Database::connect(&url).await.unwrap();
    database.migrate().await.unwrap();
    contract(&database).await;
}

#[tokio::test]
async fn postgres_sticky_selection_is_shared_and_generation_fenced() {
    let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let database = Database::connect(&url).await.unwrap();
    database.migrate().await.unwrap();
    contract(&database).await;
}
