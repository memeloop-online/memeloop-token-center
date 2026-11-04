use super::*;
use crate::request_history::HistoryIdentity;
use serde_json::json;

fn snapshot(
    tenant: Uuid,
    request: Uuid,
    account: Uuid,
    now: i64,
    revision: i64,
) -> HistorySnapshot {
    HistorySnapshot {
        identity: HistoryIdentity {
            tenant_id: tenant,
            key_id: Uuid::nil(),
            created_at: now,
            request_id: request,
            source: "native".into(),
        },
        revision,
        filter_cost_micros: None,
        statistics: None,
        payload: Some(json!({"model":"allocation-history","protocol":"openai","status_code":200})),
        read_model: json!({"fact":{
            "settled_fact":true,"upstream_account_id":account,"input_tokens":20,
            "cached_input_tokens":4,"cache_write_tokens":2,"output_tokens":6
        }}),
    }
}

async fn register(db: &Database, tenant: Uuid, now: i64, scanned: i64) {
    sqlx::query("UPDATE request_history_state SET mode='clickhouse' WHERE id=1")
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO group_allocation_history_state(tenant_id,scan_from,scan_to,scanned,capture_until,lease,lease_until) VALUES($1,$2,$3,$4,$5,'fixture',$5)")
        .bind(tenant.to_string()).bind((now/HOUR_MS-23)*HOUR_MS).bind(now).bind(scanned).bind(now+CAPTURE_MS)
        .execute(&db.pool).await.unwrap();
}

async fn renew(db: &Database, tenant: Uuid, now: i64) {
    sqlx::query("UPDATE group_allocation_history_state SET lease='fixture',lease_until=$2 WHERE tenant_id=$1")
        .bind(tenant.to_string()).bind(now+LEASE_MS).execute(&db.pool).await.unwrap();
}

async fn counts(
    db: &Database,
    tenant: Uuid,
    accounts: &[String],
    now: i64,
) -> BTreeMap<String, (u64, u64)> {
    let mut result = BTreeMap::new();
    for row in db.allocation_usage(tenant, accounts, now).await.unwrap() {
        let total = result.entry(row.account).or_insert((0, 0));
        total.0 += row.requests;
        total.1 += row.tokens;
    }
    result
}

async fn exercise_coverage(url: &str) {
    let db = Database::connect(url).await.unwrap();
    db.migrate().await.unwrap();
    let tenant = Uuid::now_v7();
    let a = Uuid::now_v7();
    let b = Uuid::now_v7();
    let accounts = vec![a.to_string(), b.to_string()];
    let now = super::super::unix_millis();
    register(&db, tenant, now, 1).await;
    let request = Uuid::now_v7();
    let unfinished = Uuid::now_v7();
    for id in [request, unfinished] {
        assert!(
            db.allocation_reserve(tenant, None, id, None, (&a.to_string(), 1), now)
                .await
                .unwrap()
        );
    }
    let original = snapshot(tenant, request, a, now, 1);
    let mut pending = snapshot(tenant, unfinished, a, now, 2);
    pending.read_model = json!({});
    db.history_acknowledge(&[original.clone(), pending.clone()])
        .await
        .unwrap();
    db.apply_allocation_history(tenant, "fixture", now)
        .await
        .unwrap();
    assert_eq!(
        counts(&db, tenant, &accounts, now)
            .await
            .get(&a.to_string()),
        Some(&(2, 20))
    );
    let reservations: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM group_allocation_reservations WHERE tenant_id=$1")
            .bind(tenant.to_string())
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(
        reservations, 1,
        "only the exact settled contribution covers a reservation"
    );

    let mut correction = original.clone();
    correction.revision = 3;
    correction.read_model["fact"]["upstream_account_id"] = json!(b);
    correction.read_model["fact"]["output_tokens"] = json!(16);
    db.history_acknowledge(&[correction.clone(), original.clone(), correction.clone()])
        .await
        .unwrap();
    db.close().await;
    let db = Database::connect(url).await.unwrap();
    renew(&db, tenant, now).await;
    sqlx::query(
        "ALTER TABLE group_allocation_history_hours RENAME TO allocation_hours_unavailable",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    assert!(
        db.apply_allocation_history(tenant, "fixture", now)
            .await
            .is_err()
    );
    assert_eq!(
        counts(&db, tenant, &accounts, now)
            .await
            .get(&a.to_string()),
        Some(&(2, 20))
    );
    sqlx::query(
        "ALTER TABLE allocation_hours_unavailable RENAME TO group_allocation_history_hours",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    db.apply_allocation_history(tenant, "fixture", now)
        .await
        .unwrap();
    let result = counts(&db, tenant, &accounts, now).await;
    assert_eq!(result.get(&a.to_string()), Some(&(1, 0)));
    assert_eq!(
        result.get(&b.to_string()),
        Some(&(1, 30)),
        "restart/replay and failed apply retain one corrected contribution"
    );

    // A delayed old bootstrap page follows a newer acknowledged correction.
    let mut tx = db.begin_write_transaction().await.unwrap();
    receipts(
        &mut tx,
        &[AllocationContribution::from_snapshot(&original).unwrap()],
        now,
        true,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    renew(&db, tenant, now).await;
    db.apply_allocation_history(tenant, "fixture", now)
        .await
        .unwrap();
    assert_eq!(counts(&db, tenant, &accounts, now).await, result);
    let mut deleted = correction;
    deleted.revision = 4;
    deleted.payload = None;
    pending.revision = 5;
    pending.read_model = json!({"fact":{"settled_fact":true,"upstream_account_id":a}});
    let mut generation = snapshot(tenant, Uuid::now_v7(), b, now, 6);
    generation.identity.source = "generation".into();
    let mut other = snapshot(Uuid::now_v7(), Uuid::now_v7(), b, now, 7);
    other.identity.source = "archive".into();
    db.history_acknowledge(&[deleted, generation, pending, other])
        .await
        .unwrap();
    renew(&db, tenant, now).await;
    db.apply_allocation_history(tenant, "fixture", now)
        .await
        .unwrap();
    let result = counts(&db, tenant, &accounts, now).await;
    assert_eq!(result.get(&a.to_string()), Some(&(1, 0)));
    assert_eq!(
        result.get(&b.to_string()),
        Some(&(1, 0)),
        "generation matches the existing hourly count with zero text tokens"
    );
    let reservations: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM group_allocation_reservations WHERE tenant_id=$1")
            .bind(tenant.to_string())
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(reservations, 0);
    let fresh = Uuid::now_v7();
    register(&db, fresh, now, 1).await;
    let old = snapshot(fresh, Uuid::now_v7(), a, now, 10);
    let mut newer = old.clone();
    newer.revision = 11;
    newer.read_model["fact"]["upstream_account_id"] = json!(b);
    let mut tx = db.begin_write_transaction().await.unwrap();
    receipts(
        &mut tx,
        &[AllocationContribution::from_snapshot(&newer).unwrap()],
        now,
        true,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    db.history_acknowledge(&[old]).await.unwrap();
    db.apply_allocation_history(fresh, "fixture", now)
        .await
        .unwrap();
    let fresh_counts = counts(&db, fresh, &accounts, now).await;
    assert!(!fresh_counts.contains_key(&a.to_string()));
    assert_eq!(
        fresh_counts.get(&b.to_string()),
        Some(&(1, 20)),
        "bootstrap priority preserves the maximum revision within a mixed receipt batch"
    );
    // Source changes must not publish a stale history batch or retire its pending count.
    let switch_tenant = Uuid::now_v7();
    register(&db, switch_tenant, now, 1).await;
    let switch_request = Uuid::now_v7();
    assert!(
        db.allocation_reserve(
            switch_tenant,
            None,
            switch_request,
            None,
            (&a.to_string(), 1),
            now
        )
        .await
        .unwrap()
    );
    db.history_acknowledge(&[snapshot(switch_tenant, switch_request, b, now, 100)])
        .await
        .unwrap();
    sqlx::query("UPDATE request_history_state SET mode='shadow' WHERE id=1")
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(
        !db.apply_allocation_history(switch_tenant, "fixture", now)
            .await
            .unwrap()
    );
    let unchanged = counts(&db, switch_tenant, &accounts, now).await;
    assert_eq!(unchanged.get(&a.to_string()), Some(&(1, 0)));
    assert!(!unchanged.contains_key(&b.to_string()));
    sqlx::query("UPDATE request_history_state SET mode='clickhouse' WHERE id=1")
        .execute(&db.pool)
        .await
        .unwrap();
    db.refresh_allocation_usage(switch_tenant, a, now)
        .await
        .unwrap();
    assert_eq!(counts(&db, switch_tenant, &accounts, now).await, unchanged);
    db.apply_allocation_history(switch_tenant, "fixture", now)
        .await
        .unwrap();
    assert_eq!(
        counts(&db, switch_tenant, &accounts, now)
            .await
            .get(&b.to_string()),
        Some(&(1, 20))
    );

    // A batch is bounded by exact keys, never by a global revision watermark.
    let paged = Uuid::now_v7();
    register(&db, paged, now, 1).await;
    let same_request = Uuid::now_v7();
    let mut high = snapshot(paged, same_request, b, now, 100_000);
    let low = (1..=256)
        .map(|revision| snapshot(paged, same_request, a, now, revision))
        .collect::<Vec<_>>();
    let mut tx = db.begin_write_transaction().await.unwrap();
    receipts(
        &mut tx,
        &[AllocationContribution::from_snapshot(&high).unwrap()],
        now,
        true,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    db.history_acknowledge(&low).await.unwrap();
    assert!(
        db.apply_allocation_history(paged, "fixture", now)
            .await
            .unwrap()
    );
    let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM group_allocation_history_receipts WHERE tenant_id=$1 AND revision=256")
        .bind(paged.to_string()).fetch_one(&db.pool).await.unwrap();
    assert_eq!(
        remaining, 1,
        "higher bootstrap revision does not retire an unconsumed key"
    );
    db.apply_allocation_history(paged, "fixture", now)
        .await
        .unwrap();
    let paged_counts = counts(&db, paged, &accounts, now).await;
    assert!(!paged_counts.contains_key(&a.to_string()));
    assert_eq!(paged_counts.get(&b.to_string()), Some(&(1, 20)));
    high.identity.source = "generation".into();
    db.history_acknowledge(&[high]).await.unwrap();
    renew(&db, paged, now).await;
    db.apply_allocation_history(paged, "fixture", now)
        .await
        .unwrap();
    assert_eq!(
        counts(&db, paged, &accounts, now).await.get(&b.to_string()),
        Some(&(2, 20)),
        "source is part of the contribution identity"
    );
    let skipped = Uuid::now_v7();
    db.history_acknowledge(&[snapshot(skipped, Uuid::now_v7(), a, now, 1)])
        .await
        .unwrap();
    let skipped_receipts: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM group_allocation_history_receipts WHERE tenant_id=$1",
    )
    .bind(skipped.to_string())
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        skipped_receipts, 0,
        "tenants without active capture incur no receipt storage"
    );
    db.prune_allocation_history(now + DAY_MS + HOUR_MS)
        .await
        .unwrap();
    let retained: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM group_allocation_history_contributions WHERE tenant_id=$1",
    )
    .bind(fresh.to_string())
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        retained, 0,
        "retired tenants expire without another quota refresh target"
    );
    db.close().await;
}

#[tokio::test]
async fn sqlite_allocation_history_exact_coverage_and_restart() {
    let directory = tempfile::tempdir().unwrap();
    exercise_coverage(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("history.db").display()
    ))
    .await;
}

#[tokio::test]
async fn postgres_allocation_history_exact_coverage_and_restart() {
    let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let admin = sqlx::PgPool::connect(&url).await.unwrap();
    let schema = format!("allocation_history_{}", Uuid::now_v7().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    let mut isolated = url::Url::parse(&url).unwrap();
    isolated
        .query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema}"));
    exercise_coverage(isolated.as_str()).await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}

#[tokio::test]
#[ignore = "CI supplies PostgreSQL and ClickHouse"]
async fn allocation_history_real_services_acceptance() {
    let url = std::env::var("MTC_TEST_POSTGRES_URL").expect("PostgreSQL acceptance URL");
    let endpoint = std::env::var("MTC_TEST_CLICKHOUSE_URL").expect("ClickHouse acceptance URL");
    let admin = sqlx::PgPool::connect(&url).await.unwrap();
    let schema = format!("allocation_ch_{}", Uuid::now_v7().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    let mut isolated = url::Url::parse(&url).unwrap();
    isolated
        .query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema}"));
    let db = Database::connect(isolated.as_str()).await.unwrap();
    db.migrate().await.unwrap();
    let client = ClickHouse::new(
        db.http_clients
            .handle(crate::http_settings::HttpPurpose::General),
        &endpoint,
        "default".into(),
        "default".into(),
        String::new(),
    )
    .unwrap();
    client.initialize().await.unwrap();
    let tenant = Uuid::now_v7();
    let a = Uuid::now_v7();
    let b = Uuid::now_v7();
    let now = super::super::unix_millis();
    let values = (0..ALLOCATION_PAGE + 3)
        .map(|index| {
            // UUID halves run in opposite directions to expose string/native UUID order mismatches.
            let request = Uuid::from_u128(
                ((index as u128 + 1) << 64) | (ALLOCATION_PAGE + 3 - index) as u128,
            );
            snapshot(tenant, request, a, now - 1000, index as i64 + 1)
        })
        .collect::<Vec<_>>();
    client.append(&values).await.unwrap();
    sqlx::query("UPDATE request_history_state SET mode='clickhouse' WHERE id=1")
        .execute(&db.pool)
        .await
        .unwrap();
    let request = values[0].identity.request_id;
    assert!(
        db.allocation_reserve(tenant, None, request, None, (&a.to_string(), 1), now - 1000)
            .await
            .unwrap()
    );
    db.refresh_allocation_clickhouse_batches(&client, tenant, now - 2000, 1)
        .await
        .unwrap();
    let ready: i64 =
        sqlx::query_scalar("SELECT ready FROM group_allocation_history_state WHERE tenant_id=$1")
            .bind(tenant.to_string())
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(
        ready, 0,
        "partial bootstrap retains the existing snapshot and pending reservation"
    );
    assert_eq!(
        counts(&db, tenant, &[a.to_string()], now)
            .await
            .get(&a.to_string()),
        Some(&(1, 0))
    );
    let mut correction = values[0].clone();
    correction.revision = 10_000;
    correction.read_model["fact"]["upstream_account_id"] = json!(b);
    client.append(&[correction.clone()]).await.unwrap();
    db.history_acknowledge(&[correction.clone()]).await.unwrap();
    client.append(&[values[0].clone()]).await.unwrap();
    db.history_acknowledge(&[values[0].clone()]).await.unwrap();
    db.close().await;
    let db = Database::connect(isolated.as_str()).await.unwrap();
    for _ in 0..4 {
        db.refresh_allocation_clickhouse(&client, tenant, now)
            .await
            .unwrap();
    }
    let result = counts(&db, tenant, &[a.to_string(), b.to_string()], now).await;
    assert_eq!(
        result.get(&a.to_string()),
        Some(&(
            (ALLOCATION_PAGE + 2) as u64,
            20 * (ALLOCATION_PAGE + 2) as u64
        ))
    );
    assert_eq!(result.get(&b.to_string()), Some(&(1, 20)));
    let reservations: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM group_allocation_reservations WHERE tenant_id=$1")
            .bind(tenant.to_string())
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(reservations, 0);
    eprintln!(
        "real ClickHouse FINAL bootstrap + PostgreSQL exact ACK coverage: 259 requests, correction/replay/restart, one accounted revision each"
    );
    db.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}
