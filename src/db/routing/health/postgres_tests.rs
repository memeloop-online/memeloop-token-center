use super::*;

async fn fixture() -> Option<(Database, Uuid, Uuid)> {
    let database_url = std::env::var("MTC_TEST_POSTGRES_URL").ok()?;
    let database = Database::connect(&database_url).await.unwrap();
    database.migrate().await.unwrap();
    let tenant_id = Uuid::now_v7();
    let account_id = Uuid::now_v7();
    let now = unix_millis();
    sqlx::query("INSERT INTO tenants (id, external_id, created_at) VALUES ($1, $2, $3)")
        .bind(tenant_id.to_string())
        .bind(format!("breaker-fence-{tenant_id}"))
        .bind(now)
        .execute(&database.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO upstream_accounts (
             id, tenant_id, name, driver, auth_kind, config_json, status,
             credential_generation, created_at, updated_at
         ) VALUES ($1, $2, $3, 'http-json', 'none', '{}', 'active', 1, $4, $4)",
    )
    .bind(account_id.to_string())
    .bind(tenant_id.to_string())
    .bind(format!("breaker-fence-{account_id}"))
    .bind(now)
    .execute(&database.pool)
    .await
    .unwrap();
    Some((database, tenant_id, account_id))
}

#[tokio::test]
async fn postgres_concurrent_failure_domains_open_one_global_breaker() {
    let Some((database, _tenant_id, account_id)) = fixture().await else {
        return;
    };
    let revision: i64 =
        sqlx::query_scalar("SELECT updated_at FROM upstream_accounts WHERE id = $1")
            .bind(account_id.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap();
    let UpstreamAttemptAdmission::Healthy { failure_epoch } = database
        .claim_upstream_account_attempt(account_id, 1)
        .await
        .unwrap()
    else {
        panic!("healthy admission");
    };
    let health = UpstreamHealthConfig {
        failure_domain_enforcement_enabled: true,
        ..UpstreamHealthConfig::DEFAULT
    };
    let node_a = database.clone();
    let node_b = database.clone();
    let failure = |domain: &'static str, pod: &'static str| AdmittedConnectionFailure {
        request_id: Uuid::now_v7(),
        upstream_account_id: account_id,
        credential_generation: 1,
        transport_revision: revision,
        failure_epoch,
        failure_stage: "proxy_connect",
        gateway_pod: pod,
        gateway_node: Some(domain),
        failure_domain: domain,
    };
    let (first, second) = tokio::join!(
        node_a
            .record_admitted_connection_failure_by_domain(failure("node-a", "gateway-a"), health,),
        node_b
            .record_admitted_connection_failure_by_domain(failure("node-b", "gateway-b"), health,),
    );
    let outcomes = [first.unwrap(), second.unwrap()];
    assert_eq!(
        outcomes
            .iter()
            .filter(|result| result.global_breaker_opened)
            .count(),
        1
    );
    let failures: i64 = sqlx::query_scalar(
        "SELECT consecutive_failures FROM upstream_account_health WHERE upstream_account_id = $1",
    )
    .bind(account_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(failures, 1);
}

#[tokio::test]
async fn failure_upsert_fences_generation_and_active_probe() {
    let Some((database, tenant_id, account_id)) = fixture().await else {
        return;
    };
    assert!(
        database
            .record_upstream_account_failure(account_id, 1, UpstreamFailureKind::Connection)
            .await
            .unwrap()
    );
    sqlx::query(
        "UPDATE upstream_account_health SET cooldown_until = 0, probe_lease_until = 0
         WHERE upstream_account_id = $1",
    )
    .bind(account_id.to_string())
    .execute(&database.pool)
    .await
    .unwrap();
    let UpstreamAttemptAdmission::Probe { lease_token } = database
        .claim_upstream_account_attempt(account_id, 1)
        .await
        .unwrap()
    else {
        panic!("half-open probe lease");
    };
    let (ordinary_failure, probe_renewal) = tokio::join!(
        database.record_upstream_account_failure(
            account_id,
            1,
            UpstreamFailureKind::InvalidResponse,
        ),
        database.renew_upstream_account_probe(account_id, 1, lease_token),
    );
    assert!(!ordinary_failure.unwrap());
    assert!(probe_renewal.unwrap());
    assert!(
        database
            .record_upstream_account_probe_success(account_id, 1, lease_token)
            .await
            .unwrap()
    );

    assert!(
        database
            .record_upstream_account_failure(account_id, 1, UpstreamFailureKind::Connection)
            .await
            .unwrap()
    );
    let mut rotation = database.pool.begin().await.unwrap();
    sqlx::query(
        "SELECT upstream_account_id FROM upstream_account_health
         WHERE upstream_account_id = $1 FOR UPDATE",
    )
    .bind(account_id.to_string())
    .fetch_one(&mut *rotation)
    .await
    .unwrap();
    let stale_database = database.clone();
    let stale_failure = tokio::spawn(async move {
        stale_database
            .record_upstream_account_failure(account_id, 1, UpstreamFailureKind::InvalidResponse)
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(
        !stale_failure.is_finished(),
        "the old-generation terminal must wait on the health-row writer"
    );
    sqlx::query("UPDATE upstream_accounts SET credential_generation = 2 WHERE id = $1")
        .bind(account_id.to_string())
        .execute(&mut *rotation)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE upstream_account_health
         SET credential_generation = 2, probe_lease_until = 0, probe_lease_token = ''
         WHERE upstream_account_id = $1",
    )
    .bind(account_id.to_string())
    .execute(&mut *rotation)
    .await
    .unwrap();
    rotation.commit().await.unwrap();
    assert!(
        !tokio::time::timeout(std::time::Duration::from_secs(3), stale_failure)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        "the blocked old-generation UPSERT cannot downgrade newer health"
    );
    assert!(
        database
            .record_upstream_account_failure(account_id, 2, UpstreamFailureKind::Unavailable)
            .await
            .unwrap()
    );
    let generation: i64 = sqlx::query_scalar(
        "SELECT credential_generation FROM upstream_account_health
         WHERE upstream_account_id = $1",
    )
    .bind(account_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(generation, 2);

    sqlx::query("DELETE FROM upstream_accounts WHERE id = $1")
        .bind(account_id.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM tenants WHERE id = $1")
        .bind(tenant_id.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
}

fn healthy_failure_epoch(admission: UpstreamAttemptAdmission) -> Uuid {
    match admission {
        UpstreamAttemptAdmission::Healthy { failure_epoch } => failure_epoch,
        other => panic!("expected healthy admission, got {other:?}"),
    }
}

async fn concurrent_failure_cohort_is_one_episode(database: Database, peer: Database) {
    database.migrate().await.unwrap();
    let tenant_id = Uuid::now_v7();
    let account_id = Uuid::now_v7();
    let now = unix_millis();
    sqlx::query("INSERT INTO tenants (id, external_id, created_at) VALUES ($1, $2, $3)")
        .bind(tenant_id.to_string())
        .bind(format!("failure-cohort-{tenant_id}"))
        .bind(now)
        .execute(&database.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO upstream_accounts (
             id, tenant_id, name, driver, auth_kind, config_json, status,
             credential_generation, created_at, updated_at
         ) VALUES ($1, $2, $3, 'http-json', 'none', '{}', 'active', 1, $4, $4)",
    )
    .bind(account_id.to_string())
    .bind(tenant_id.to_string())
    .bind(format!("failure-cohort-{account_id}"))
    .bind(now)
    .execute(&database.pool)
    .await
    .unwrap();

    // Model a request wave that all crossed healthy admission before a single
    // proxy/network incident disconnected every in-flight request.
    let mut fences = Vec::new();
    for worker in [
        &database, &peer, &database, &peer, &database, &peer, &database, &peer,
    ] {
        fences.push(healthy_failure_epoch(
            worker
                .claim_upstream_account_attempt(account_id, 1)
                .await
                .unwrap(),
        ));
    }
    assert!(fences.windows(2).all(|pair| pair[0] == pair[1]));
    let late_epoch = fences.pop().unwrap();
    let mut failures = tokio::task::JoinSet::new();
    for (index, failure_epoch) in fences.into_iter().enumerate() {
        let worker = if index % 2 == 0 {
            database.clone()
        } else {
            peer.clone()
        };
        failures.spawn(async move {
            worker
                .record_admitted_upstream_account_failure(
                    account_id,
                    1,
                    UpstreamFailureKind::Connection,
                    UpstreamHealthConfig::DEFAULT,
                    failure_epoch,
                )
                .await
                .unwrap()
        });
    }
    let mut recorded = 0;
    while let Some(result) = failures.join_next().await {
        if result.unwrap() {
            recorded += 1;
        }
    }
    assert_eq!(recorded, 1, "one incident wave is one failure episode");
    let row = sqlx::query(
        "SELECT consecutive_failures, cooldown_until - updated_at AS cooldown
         FROM upstream_account_health WHERE upstream_account_id = $1",
    )
    .bind(account_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(row.get::<i64, _>("consecutive_failures"), 1);
    assert_eq!(
        row.get::<i64, _>("cooldown"),
        UpstreamHealthConfig::DEFAULT.connection_cooldown_millis
    );

    // A validated sibling success proves recovery immediately; operators do
    // not wait out a cooldown created by another member of the same cohort.
    assert!(
        peer.record_upstream_account_success(account_id, 1, late_epoch)
            .await
            .unwrap()
    );
    assert!(
        !database
            .record_admitted_upstream_account_failure(
                account_id,
                1,
                UpstreamFailureKind::Connection,
                UpstreamHealthConfig::DEFAULT,
                late_epoch,
            )
            .await
            .unwrap(),
        "failure -> success -> late same-cohort failure remains healthy"
    );
    assert!(
        database
            .claim_upstream_account_attempt(account_id, 1)
            .await
            .unwrap()
            .is_healthy()
    );

    // A success that wins before a same-cohort failure rotates the epoch, so
    // the late failure cannot reopen a healthy account.
    let success_first_epoch = healthy_failure_epoch(
        database
            .claim_upstream_account_attempt(account_id, 1)
            .await
            .unwrap(),
    );
    assert!(
        peer.record_upstream_account_success(account_id, 1, success_first_epoch)
            .await
            .unwrap()
    );
    assert!(
        !database
            .record_admitted_upstream_account_failure(
                account_id,
                1,
                UpstreamFailureKind::Connection,
                UpstreamHealthConfig::DEFAULT,
                success_first_epoch,
            )
            .await
            .unwrap(),
        "success -> late same-cohort failure remains healthy"
    );

    // Two requests enter an older cohort. Once one succeeds, a new request is
    // admitted under the rotated epoch. The other old request's late success
    // must not rotate that new epoch or hide the new request's real failure.
    let old_success_epoch = healthy_failure_epoch(
        database
            .claim_upstream_account_attempt(account_id, 1)
            .await
            .unwrap(),
    );
    let old_late_success_epoch = healthy_failure_epoch(
        peer.claim_upstream_account_attempt(account_id, 1)
            .await
            .unwrap(),
    );
    assert_eq!(old_success_epoch, old_late_success_epoch);
    assert!(
        database
            .record_upstream_account_success(account_id, 1, old_success_epoch)
            .await
            .unwrap()
    );
    let new_failure_epoch = healthy_failure_epoch(
        peer.claim_upstream_account_attempt(account_id, 1)
            .await
            .unwrap(),
    );
    assert_ne!(old_success_epoch, new_failure_epoch);
    assert!(
        !database
            .record_upstream_account_success(account_id, 1, old_late_success_epoch)
            .await
            .unwrap(),
        "an old success cannot rotate the current healthy cohort"
    );
    assert!(
        database
            .record_admitted_upstream_account_failure(
                account_id,
                1,
                UpstreamFailureKind::Connection,
                UpstreamHealthConfig::DEFAULT,
                new_failure_epoch,
            )
            .await
            .unwrap(),
        "the newer cohort's genuine failure remains authoritative"
    );

    // Recover the new episode before exercising the independent sustained
    // failure path below.
    assert!(
        peer.record_upstream_account_success(account_id, 1, new_failure_epoch)
            .await
            .unwrap()
    );

    // A later, independently admitted episode still opens the breaker, and a
    // failed half-open probe escalates it. Cohort collapse therefore does not
    // hide a sustained outage.
    let fence = healthy_failure_epoch(
        database
            .claim_upstream_account_attempt(account_id, 1)
            .await
            .unwrap(),
    );
    assert!(
        database
            .record_admitted_upstream_account_failure(
                account_id,
                1,
                UpstreamFailureKind::Connection,
                UpstreamHealthConfig::DEFAULT,
                fence,
            )
            .await
            .unwrap()
    );
    sqlx::query(
        "UPDATE upstream_account_health SET cooldown_until = 0, probe_lease_until = 0
         WHERE upstream_account_id = $1",
    )
    .bind(account_id.to_string())
    .execute(&database.pool)
    .await
    .unwrap();
    let UpstreamAttemptAdmission::Probe { lease_token } = peer
        .claim_upstream_account_attempt(account_id, 1)
        .await
        .unwrap()
    else {
        panic!("sustained failure must enter half-open probe");
    };
    assert!(
        peer.record_upstream_account_probe_failure(
            account_id,
            1,
            lease_token,
            UpstreamFailureKind::Connection,
        )
        .await
        .unwrap()
    );
    let consecutive: i64 = sqlx::query_scalar(
        "SELECT consecutive_failures FROM upstream_account_health
         WHERE upstream_account_id = $1",
    )
    .bind(account_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(consecutive, 2);
}

#[tokio::test]
async fn sqlite_concurrent_connection_failures_collapse_to_one_episode() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("connection-cohort.db").display()
    );
    let database = Database::connect(&url).await.unwrap();
    let peer = Database::connect(&url).await.unwrap();
    concurrent_failure_cohort_is_one_episode(database, peer).await;
}

#[tokio::test]
async fn postgres_concurrent_connection_failures_collapse_to_one_episode() {
    let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let database = Database::connect(&url).await.unwrap();
    let peer = Database::connect(&url).await.unwrap();
    concurrent_failure_cohort_is_one_episode(database, peer).await;
}
