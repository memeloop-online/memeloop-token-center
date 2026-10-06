use super::*;

const KEY: &[u8] = b"sticky-proxy-test-key-at-least-32-bytes";
const PRIMARY: &str = "socks5h://10.20.30.40:1080";
const BACKUP: &str = "socks5h://10.20.30.41:1080";

fn groups(account_id: Uuid, version: i64) -> TransportProxyGroups {
    TransportProxyGroups::parse(
        &serde_json::json!([{
            "account_id": account_id, "version":version, "proxies":[PRIMARY,BACKUP]
        }])
        .to_string(),
        KEY,
    )
    .unwrap()
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
    assert!(TransportProxyGroups::parse("[]", KEY).is_ok());
    for proxies in [
        vec![],
        vec![PRIMARY; 5],
        vec![PRIMARY, PRIMARY],
        vec!["socks5h://public.example:1080"],
        vec!["socks5://10.20.30.41:1080"],
    ] {
        let input = serde_json::json!([{"account_id":Uuid::nil(),"version":1,"proxies":proxies}]);
        let error = TransportProxyGroups::parse(&input.to_string(), KEY)
            .err()
            .unwrap();
        assert!(!error.to_string().contains("10.20.30"));
    }
    for version in [0, -1] {
        let input =
            serde_json::json!([{"account_id":Uuid::nil(),"version":version,"proxies":[PRIMARY]}]);
        assert!(TransportProxyGroups::parse(&input.to_string(), KEY).is_err());
    }
    let duplicated = serde_json::json!([
        {"account_id":Uuid::nil(),"version":1,"proxies":[PRIMARY]},
        {"account_id":Uuid::nil(),"version":1,"proxies":[BACKUP]}
    ]);
    assert!(TransportProxyGroups::parse(&duplicated.to_string(), KEY).is_err());
    assert!(TransportProxyGroups::parse(&" ".repeat(256 * 1024 + 1), KEY).is_err());
    let excessive: Vec<_> = (0..257)
        .map(|index| {
            serde_json::json!({
                "account_id":Uuid::from_u128(index),"version":1,"proxies":[PRIMARY]
            })
        })
        .collect();
    assert!(TransportProxyGroups::parse(&serde_json::to_string(&excessive).unwrap(), KEY).is_err());
}

#[test]
fn selection_and_failover_need_no_database_or_queue() {
    let account_id = Uuid::nil();
    let groups = groups(account_id, 1);
    let source = credential();
    let first = groups.select(account_id, 1, &source).unwrap();
    let concurrent = groups.select(account_id, 1, &source).unwrap();
    assert!(first.advance_after_connect_failure(&[0]).unwrap());
    assert!(!concurrent.advance_after_connect_failure(&[0]).unwrap());
    for _ in 0..1000 {
        let healthy = groups.select(account_id, 1, &source).unwrap();
        assert_eq!(healthy.credential.proxy().unwrap().0, BACKUP);
        assert_eq!(healthy.generation, first.generation + 1);
    }
    let old = groups.select(account_id, 1, &source).unwrap();
    let refreshed = groups.select(account_id, 2, &source).unwrap();
    assert_eq!(refreshed.credential.proxy().unwrap().0, BACKUP);
    assert!(refreshed.generation > old.generation);
    assert!(!old.advance_after_connect_failure(&[1]).unwrap());
    assert!(groups.select(account_id, 1, &source).is_err());
    assert!(
        groups
            .select(account_id, 2, &UpstreamCredential::None)
            .is_err()
    );
    assert!(!refreshed.advance_after_connect_failure(&[0, 1]).unwrap());
    let single = TransportProxyGroups::parse("[]", KEY).unwrap();
    let unchanged = single.select(account_id, 2, &source).unwrap();
    assert_eq!(unchanged.credential.proxy(), source.proxy());
    assert!(!unchanged.advance_after_connect_failure(&[]).unwrap());
    assert!(
        single
            .select(account_id, 2, &UpstreamCredential::None)
            .unwrap()
            .credential
            .proxy()
            .is_none()
    );
}

#[test]
fn packed_generation_and_epoch_cannot_wrap_into_an_old_ticket() {
    let account_id = Uuid::nil();
    let groups = groups(account_id, 1);
    let source = credential();
    assert!(
        groups
            .select(account_id, i64::from(u32::MAX) + 1, &source)
            .is_err()
    );
    let state = Snapshot {
        credential_generation: 1,
        epoch: MAX_EPOCH,
        base: 0,
        selected: 0,
    }
    .encode()
    .unwrap();
    groups.groups[&account_id]
        .state
        .store(state, Ordering::Release);
    let selected = groups.select(account_id, 1, &source).unwrap();
    assert!(selected.advance_after_connect_failure(&[0]).is_err());
    assert!(groups.select(account_id, 2, &source).is_err());
}

#[test]
fn stale_failure_cannot_overwrite_a_concurrent_selection_round_trip() {
    let account_id = Uuid::nil();
    let groups = groups(account_id, 1);
    let source = credential();
    let stale = groups.select(account_id, 1, &source).unwrap();
    let concurrent = groups.select(account_id, 1, &source).unwrap();
    assert!(concurrent.advance_after_connect_failure(&[0]).unwrap());
    let backup = groups.select(account_id, 1, &source).unwrap();
    assert!(backup.advance_after_connect_failure(&[1]).unwrap());
    let returned = groups.select(account_id, 1, &source).unwrap();
    assert_eq!(returned.member(), Some(0));
    assert_eq!(returned.generation, stale.generation + 2);
    assert!(!stale.advance_after_connect_failure(&[0]).unwrap());
    assert_eq!(
        stale.advance_after_connect_failure_outcome(&[0]).unwrap(),
        "contended"
    );
    let preserved = groups.select(account_id, 1, &source).unwrap();
    assert_eq!(preserved.member(), returned.member());
    assert_eq!(preserved.generation, returned.generation);
}

#[test]
fn request_local_exclusion_visits_untried_members_without_changing_global_stickiness() {
    let account_id = Uuid::nil();
    let proxies = [
        PRIMARY,
        BACKUP,
        "socks5h://10.20.30.42:1080",
        "socks5h://10.20.30.43:1080",
    ];
    let groups = TransportProxyGroups::parse(
        &serde_json::json!([{
            "account_id": account_id, "version": 1, "proxies": proxies
        }])
        .to_string(),
        KEY,
    )
    .unwrap();
    let source = credential();
    let global = groups.select(account_id, 1, &source).unwrap();
    let mut attempted = Vec::new();
    for (index, proxy) in proxies.into_iter().enumerate() {
        let mut selected = groups.select(account_id, 1, &source).unwrap();
        assert!(selected.select_unattempted(&attempted).unwrap());
        assert_eq!(selected.member(), Some(index));
        assert_eq!(selected.member_index(), Some(index));
        assert_eq!(selected.member_count(), 4);
        assert_eq!(selected.group_selection_version(), Some(1));
        assert_eq!(selected.generation, global.generation);
        assert_eq!(selected.credential.proxy().unwrap().0, proxy);
        assert_eq!(selected.is_request_local(), index != 0);
        attempted.push(index);
        if index != 0 {
            assert_eq!(
                selected
                    .advance_after_connect_failure_outcome(&attempted)
                    .unwrap(),
                "request_local"
            );
        }
        let preserved = groups.select(account_id, 1, &source).unwrap();
        assert_eq!(preserved.member(), Some(0));
        assert_eq!(preserved.generation, global.generation);
    }
    let mut exhausted = groups.select(account_id, 1, &source).unwrap();
    assert!(!exhausted.select_unattempted(&attempted).unwrap());
    assert_eq!(
        exhausted
            .advance_after_connect_failure_outcome(&attempted)
            .unwrap(),
        "no_untried_member"
    );
}

#[test]
fn request_local_exclusion_rejects_invalidated_binding_or_credential_snapshot() {
    for invalidation in ["binding", "observed_generation", "newer_token"] {
        let account_id = Uuid::nil();
        let groups = groups(account_id, 1);
        let source = credential();
        let mut stale = groups.select(account_id, 1, &source).unwrap();
        let entry = &groups.groups[&account_id];
        match invalidation {
            "binding" => entry.blocked.store(true, Ordering::Release),
            "observed_generation" => entry.observed_generation.store(2, Ordering::Release),
            "newer_token" => {
                groups.select(account_id, 2, &source).unwrap();
            }
            _ => unreachable!(),
        }
        let preserved = entry.state.load(Ordering::Acquire);
        assert!(stale.select_unattempted(&[0]).is_err());
        assert!(!stale.advance_after_connect_failure(&[0]).unwrap());
        assert_eq!(entry.state.load(Ordering::Acquire), preserved);
    }
}

#[test]
fn request_local_exclusion_preserves_ungrouped_and_single_member_retry_policy() {
    for proxies in [vec![], vec![PRIMARY]] {
        let account_id = Uuid::nil();
        let input = if proxies.is_empty() {
            serde_json::json!([])
        } else {
            serde_json::json!([{
                "account_id": account_id, "version": 1, "proxies": proxies
            }])
        };
        let groups = TransportProxyGroups::parse(&input.to_string(), KEY).unwrap();
        let mut selected = groups.select(account_id, 1, &credential()).unwrap();
        assert_eq!(selected.member_count(), proxies.len());
        assert_eq!(selected.member(), None);
        assert!(selected.select_unattempted(&[0]).unwrap());
        assert!(!selected.is_request_local());
    }
}

async fn account(database: &Database) -> Uuid {
    database
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
        .unwrap()
        .id
}

async fn persistence_contract(database: &Database) {
    let account_id = account(database).await;
    let groups = groups(account_id, 1);
    let source = credential();
    let first = groups.select(account_id, 1, &source).unwrap();
    groups.groups[&account_id]
        .synchronize(&database.pool)
        .await
        .unwrap();
    assert!(first.advance_after_connect_failure(&[0]).unwrap());
    groups.groups[&account_id]
        .synchronize(&database.pool)
        .await
        .unwrap();
    let restarted = super::tests::groups(account_id, 1);
    restarted.groups[&account_id]
        .synchronize(&database.pool)
        .await
        .unwrap();
    assert_eq!(
        restarted
            .select(account_id, 1, &source)
            .unwrap()
            .credential
            .proxy()
            .unwrap()
            .0,
        BACKUP
    );
    let already_active = super::tests::groups(account_id, 1);
    assert_eq!(
        already_active
            .select(account_id, 1, &source)
            .unwrap()
            .credential
            .proxy()
            .unwrap()
            .0,
        PRIMARY
    );
    already_active.groups[&account_id]
        .synchronize(&database.pool)
        .await
        .unwrap();
    assert_eq!(
        already_active
            .select(account_id, 1, &source)
            .unwrap()
            .credential
            .proxy()
            .unwrap()
            .0,
        PRIMARY
    );
    assert_eq!(
        groups
            .select(account_id, 1, &source)
            .unwrap()
            .credential
            .proxy()
            .unwrap()
            .0,
        BACKUP
    );
    sqlx::query("UPDATE upstream_accounts SET credential_generation = 2 WHERE id = $1")
        .bind(account_id.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    groups.groups[&account_id]
        .synchronize(&database.pool)
        .await
        .unwrap();
    assert!(groups.select(account_id, 1, &source).is_err());
    assert_eq!(
        groups
            .select(account_id, 2, &source)
            .unwrap()
            .credential
            .proxy()
            .unwrap()
            .0,
        BACKUP
    );
    groups.groups[&account_id]
        .synchronize(&database.pool)
        .await
        .unwrap();
    let newer = super::tests::groups(account_id, 2);
    newer.select(account_id, 2, &source).unwrap();
    newer.groups[&account_id]
        .synchronize(&database.pool)
        .await
        .unwrap();
    groups.groups[&account_id]
        .synchronize(&database.pool)
        .await
        .unwrap();
    assert!(groups.select(account_id, 2, &source).is_err());
    let conflict = TransportProxyGroups::parse(
        &serde_json::json!([{
            "account_id":account_id,"version":2,"proxies":[PRIMARY,"socks5h://10.20.30.42:1080"]
        }])
        .to_string(),
        KEY,
    )
    .unwrap();
    conflict.groups[&account_id]
        .synchronize(&database.pool)
        .await
        .unwrap();
    assert!(conflict.select(account_id, 2, &source).is_err());
}

#[tokio::test]
async fn sqlite_recovery_preserves_active_stickiness_and_rejects_stale_versions() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("sticky.db").display()
    );
    let database = Database::connect(&url).await.unwrap();
    database.migrate().await.unwrap();
    persistence_contract(&database).await;
}

#[tokio::test]
async fn postgres_recovery_preserves_active_stickiness_and_rejects_stale_versions() {
    let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let database = Database::connect(&url).await.unwrap();
    database.migrate().await.unwrap();
    persistence_contract(&database).await;
}

#[tokio::test]
async fn locked_database_cannot_block_cached_selection_or_local_failover() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("locked.db").display()
    );
    let database = Database::connect(&url).await.unwrap();
    database.migrate().await.unwrap();
    let account_id = account(&database).await;
    let groups = groups(account_id, 1);
    let source = credential();
    let first = groups.select(account_id, 1, &source).unwrap();
    groups.groups[&account_id]
        .synchronize(&database.pool)
        .await
        .unwrap();
    let writer = database.begin_write_transaction().await.unwrap();
    assert!(first.advance_after_connect_failure(&[0]).unwrap());
    let entry = &groups.groups[&account_id];
    let mut blocked = Box::pin(entry.synchronize(&database.pool));
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut blocked)
            .await
            .is_err()
    );
    for _ in 0..1000 {
        assert_eq!(
            groups
                .select(account_id, 1, &source)
                .unwrap()
                .credential
                .proxy()
                .unwrap()
                .0,
            BACKUP
        );
    }
    let backup = groups.select(account_id, 1, &source).unwrap();
    assert!(backup.advance_after_connect_failure(&[1]).unwrap());
    writer.rollback().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), blocked)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        groups
            .select(account_id, 1, &source)
            .unwrap()
            .credential
            .proxy()
            .unwrap()
            .0,
        PRIMARY
    );
    entry.synchronize(&database.pool).await.unwrap();
    let saved: i64 = sqlx::query_scalar(
        "SELECT selected_index FROM upstream_transport_proxy_selections WHERE account_id = $1",
    )
    .bind(account_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(saved, 0);
    database.pool.close().await;
    let primary = groups.select(account_id, 1, &source).unwrap();
    assert!(primary.advance_after_connect_failure(&[0]).unwrap());
    assert_eq!(
        groups
            .select(account_id, 1, &source)
            .unwrap()
            .credential
            .proxy()
            .unwrap()
            .0,
        BACKUP
    );
}
