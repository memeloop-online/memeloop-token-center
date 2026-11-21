use super::*;

fn quantiles(mut values: Vec<u64>) -> serde_json::Value {
    values.sort_unstable();
    let percentile = |p: usize| {
        values
            .get(values.len().saturating_sub(1) * p / 100)
            .copied()
    };
    serde_json::json!({"p50_us":percentile(50),"p95_us":percentile(95),"p99_us":percentile(99)})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "reference curve requires isolated real PostgreSQL; not 100k qualification"]
async fn postgres_archive_gc_pin_reference_curve() {
    let url = std::env::var("MTC_TEST_POSTGRES_URL").expect("real PostgreSQL required");
    let isolated = crate::db::test_support::PostgresTestDatabase::create(&url).await;
    let db = Database::connect_with_max(isolated.url(), 16)
        .await
        .unwrap();
    db.migrate().await.unwrap();
    for purpose in [ArchiveGcPinPurpose::Reader, ArchiveGcPinPurpose::Writer] {
        for concurrency in [1_000_usize, 10_000] {
            let tenant = Uuid::now_v7();
            let scope = ArchiveGcScope::for_tenant(tenant).unwrap();
            let wal_before: String = sqlx::query_scalar("SELECT pg_current_wal_lsn()::TEXT")
                .fetch_one(&db.pool)
                .await
                .unwrap();
            let before = std::time::Instant::now();
            let mut work = tokio::task::JoinSet::new();
            for _ in 0..concurrency {
                let database = db.clone();
                let scope = scope.clone();
                work.spawn(async move {
                    let start = std::time::Instant::now();
                    let pin = database
                        .acquire_archive_gc_pins(
                            vec![scope],
                            Uuid::now_v7(),
                            "reference-curve",
                            Uuid::now_v7(),
                            purpose,
                        )
                        .await?
                        .ok_or_else(|| AppError::storage("benchmark_unexpected_gc_exclusive"))?;
                    let acquire = start.elapsed().as_micros() as u64;
                    let start = std::time::Instant::now();
                    database.release_archive_gc_pins(&pin).await?;
                    Ok::<_, AppError>((acquire, start.elapsed().as_micros() as u64))
                });
            }
            let mut acquire = Vec::new();
            let mut release = Vec::new();
            let mut failed = 0_usize;
            while let Some(result) = work.join_next().await {
                match result.unwrap() {
                    Ok((a, r)) => {
                        acquire.push(a);
                        release.push(r);
                    }
                    Err(_) => failed += 1,
                }
            }
            let elapsed_ms = before.elapsed().as_millis();
            let wal_delta: String =
                sqlx::query_scalar("SELECT pg_wal_lsn_diff(pg_current_wal_lsn(),$1::pg_lsn)::TEXT")
                    .bind(wal_before)
                    .fetch_one(&db.pool)
                    .await
                    .unwrap();
            let pins: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM archive_gc_pins WHERE tenant_id=$1")
                    .bind(tenant.to_string())
                    .fetch_one(&db.pool)
                    .await
                    .unwrap();
            let active: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM archive_gc_pins WHERE tenant_id=$1 AND active=1",
            )
            .bind(tenant.to_string())
            .fetch_one(&db.pool)
            .await
            .unwrap();
            let relation_bytes: i64 =
                sqlx::query_scalar("SELECT pg_total_relation_size('archive_gc_pins')::BIGINT")
                    .fetch_one(&db.pool)
                    .await
                    .unwrap();
            println!(
                "GC_PIN_REFERENCE {}",
                serde_json::json!({
                    "purpose":purpose.as_str(),"concurrency":concurrency,"elapsed_ms":elapsed_ms,
                    "completed":acquire.len(),"failed":failed,"begin":quantiles(acquire),"release":quantiles(release),
                    "pool_connections":db.pool.size(),"pool_idle":db.pool.num_idle(),"wal_bytes_cluster_delta":wal_delta,
                    "retained_pin_rows":pins,"active_pin_rows":active,"total_pin_relation_bytes":relation_bytes,
                    "transactions_per_lifecycle":2,"scope_row_writes_per_lifecycle":2,
                    "qualification":"reference_only_not_100k_capacity"
                })
            );
            assert_eq!(failed, 0, "reference concurrency has failed pin lifecycles");
            assert_eq!(active, 0, "reference concurrency leaves active owners");
        }
    }
    db.close().await;
    isolated.cleanup().await;
}
