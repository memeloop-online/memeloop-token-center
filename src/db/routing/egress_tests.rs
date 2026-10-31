use super::*;

#[test]
fn selected_address_excludes_credentials_paths_queries_and_fragments() {
    let selected = SelectedEgress::capture(Some(
        "socks5h://secret-user:secret-pass@[2001:db8::1]:1081/private?token=secret#secret",
    ))
    .unwrap();
    let encoded = serde_json::to_string(&selected).unwrap();
    assert!(!encoded.contains("secret"));
    assert!(!encoded.contains("private"));
    assert_eq!(selected.scheme.as_deref(), Some("socks5h"));
    assert_eq!(selected.host.as_deref(), Some("[2001:db8::1]"));
    assert_eq!(selected.port, Some(1081));
    assert_eq!(SelectedEgress::capture(None).unwrap().kind, "unknown");
    assert!(SelectedEgress::capture(Some("file:///secret")).is_err());
}

fn attempt() -> RequestEgressAttempt {
    let mut selected = SelectedEgress::capture(Some("https://user:pass@proxy.example")).unwrap();
    selected.group_id = Some(Uuid::now_v7());
    selected.group_version = Some(3);
    selected.group_name = Some("original group".into());
    selected.member_id = Some(Uuid::now_v7());
    selected.member_name = Some("original member".into());
    RequestEgressAttempt {
        request_id: Uuid::now_v7(),
        attempt_id: Uuid::now_v7(),
        outbound_attempt: 1,
        connect_attempt: 1,
        route_id: Uuid::now_v7(),
        upstream_account_id: Uuid::now_v7(),
        credential_generation: 2,
        transport_revision: 3,
        observed_at: 100,
        selected,
        transport: serde_json::json!({"selection_epoch": 1}),
        connection_binding: None,
        request_payload_bytes: None,
        response_payload_bytes: None,
        completed: false,
        final_response: false,
        response_body_complete: false,
        measurement_basis: "application_payload_estimate",
    }
}

async fn projection_contract(database: &Database) {
    let mut first = attempt();
    database
        .record_request_egress_attempt(&first)
        .await
        .unwrap();
    first.request_payload_bytes = Some(10);
    first.response_payload_bytes = Some(5);
    first.completed = true;
    database
        .record_request_egress_attempt(&first)
        .await
        .unwrap();
    database
        .record_request_egress_attempt(&first)
        .await
        .unwrap();
    let mut stale = first.clone();
    stale.request_payload_bytes = Some(2);
    stale.response_payload_bytes = None;
    stale.completed = false;
    database
        .record_request_egress_attempt(&stale)
        .await
        .unwrap();

    let mut retry = first.clone();
    retry.attempt_id = Uuid::now_v7();
    retry.request_payload_bytes = Some(20);
    retry.response_payload_bytes = Some(30);
    retry.final_response = true;
    database
        .record_request_egress_attempt(&retry)
        .await
        .unwrap();
    let mut unknown = first.clone();
    unknown.attempt_id = Uuid::now_v7();
    unknown.connect_attempt = 3;
    unknown.request_payload_bytes = None;
    unknown.response_payload_bytes = None;
    unknown.completed = false;
    database
        .record_request_egress_attempt(&unknown)
        .await
        .unwrap();

    let mut changed = first.clone();
    changed.selected.member_name = Some("later current member".into());
    assert!(
        database
            .record_request_egress_attempt(&changed)
            .await
            .is_err()
    );
    let attempts = database
        .request_egress_attempts(first.request_id)
        .await
        .unwrap();
    assert_eq!(attempts.len(), 3);
    let recorded_first = attempts
        .iter()
        .find(|value| value.attempt_id == first.attempt_id)
        .unwrap();
    let recorded_retry = attempts
        .iter()
        .find(|value| value.attempt_id == retry.attempt_id)
        .unwrap();
    assert_eq!(recorded_first.request_payload_bytes, Some(10));
    assert_eq!(recorded_first.response_payload_bytes, Some(5));
    assert!(recorded_first.completed);
    assert!(!recorded_first.final_response);
    assert!(recorded_retry.final_response);
    assert_eq!(
        recorded_first.connect_attempt,
        recorded_retry.connect_attempt
    );
    assert_eq!(recorded_first.selected, first.selected);
    assert!(
        database
            .request_egress_attempts(Uuid::now_v7())
            .await
            .unwrap()
            .is_empty()
    );

    let summaries = database.request_egress_summary(100, 101).await.unwrap();
    let summary = summaries
        .iter()
        .find(|value| value.selected == first.selected)
        .unwrap();
    assert_eq!(summary.recorded_attempts, 3);
    assert_eq!(summary.completed_attempts, 2);
    assert_eq!(summary.final_response_attempts, 1);
    assert_eq!(summary.request_known_attempts, 2);
    assert_eq!(summary.response_known_attempts, 2);
    assert_eq!(summary.request_payload_bytes, Some(30));
    assert_eq!(summary.response_payload_bytes, Some(35));
    assert_eq!(summary.measurement_basis, "application_payload_estimate");
    assert_eq!(summary.wire_bytes, None);
    assert_eq!(summary.supplier_billed_bytes, None);
    assert!(
        database
            .request_egress_summary(101, 102)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(database.request_egress_summary(101, 100).await.is_err());

    let mut unknown_only = attempt();
    unknown_only.observed_at = 200;
    database
        .record_request_egress_attempt(&unknown_only)
        .await
        .unwrap();
    let summaries = database.request_egress_summary(200, 201).await.unwrap();
    let summary = summaries
        .iter()
        .find(|value| value.selected == unknown_only.selected)
        .unwrap();
    assert_eq!(summary.request_known_attempts, 0);
    assert_eq!(summary.request_payload_bytes, None);
    assert_eq!(summary.response_payload_bytes, None);
}

async fn tenant_summary_contract(database: &Database) {
    let external = format!("egress-query-{}", Uuid::now_v7());
    let other = format!("egress-other-{}", Uuid::now_v7());
    database.create_tenant(&external, None).await.unwrap();
    database.create_tenant(&other, None).await.unwrap();
    let tenant: String = sqlx::query_scalar("SELECT id FROM tenants WHERE external_id = $1")
        .bind(&external)
        .fetch_one(&database.pool)
        .await
        .unwrap();
    let other_tenant: String = sqlx::query_scalar("SELECT id FROM tenants WHERE external_id = $1")
        .bind(&other)
        .fetch_one(&database.pool)
        .await
        .unwrap();
    let prefix = Uuid::now_v7().to_string();
    sqlx::query("WITH RECURSIVE numbered(value) AS (SELECT 1 UNION ALL SELECT value + 1 FROM numbered WHERE value < 20000) INSERT INTO request_record_locators (id, created_at, tenant_id, key_id) SELECT $1 || CAST(value AS TEXT), 0, $2, 'egress-query-fixture' FROM numbered")
        .bind(&prefix).bind(&tenant).execute(&database.pool).await.unwrap();
    let start = unix_millis();
    let mut included = attempt();
    included.observed_at = start;
    included.request_payload_bytes = Some(42);
    let mut excluded = included.clone();
    excluded.request_id = Uuid::now_v7();
    excluded.request_payload_bytes = Some(777);
    let mut boundary = included.clone();
    boundary.request_id = Uuid::now_v7();
    boundary.observed_at = start + 1;
    for (attempt, owner) in [
        (&included, &tenant),
        (&excluded, &other_tenant),
        (&boundary, &tenant),
    ] {
        sqlx::query("INSERT INTO request_record_locators (id, created_at, tenant_id, key_id) VALUES ($1,0,$2,'egress-query-fixture')")
            .bind(attempt.request_id.to_string()).bind(owner).execute(&database.pool).await.unwrap();
        database
            .record_request_egress_attempt(attempt)
            .await
            .unwrap();
    }
    let started = std::time::Instant::now();
    let summaries = database
        .request_egress_summary_for_tenant(start, start + 1, Some(&external))
        .await
        .unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].recorded_attempts, 1);
    assert_eq!(summaries[0].request_payload_bytes, Some(42));
    assert!(summaries[0].response_payload_bytes.is_none());
    println!(
        "egress tenant equality query: 20000 unrelated locators, 1 included/1 other tenant/1 end-boundary, elapsed={:?}",
        started.elapsed()
    );
    let global = database
        .request_egress_summary(start, start + 1)
        .await
        .unwrap();
    let matching = global
        .iter()
        .find(|summary| summary.selected == included.selected)
        .unwrap();
    assert_eq!(matching.recorded_attempts, 2);
    assert_eq!(matching.request_payload_bytes, Some(819));
    assert!(
        database
            .request_egress_summary_for_tenant(start, start + 1, Some("missing-tenant"))
            .await
            .unwrap()
            .is_empty()
    );
    if matches!(database.backend, DatabaseBackend::PostgreSql) {
        let plan = sqlx::query("EXPLAIN (ANALYZE, BUFFERS, FORMAT TEXT) SELECT e.snapshot_json, COUNT(*) FROM request_egress_attempts e JOIN request_record_locators locator ON locator.id = e.request_id JOIN tenants tenant ON tenant.id = locator.tenant_id WHERE e.observed_at >= $1 AND e.observed_at < $2 AND tenant.external_id = $3 GROUP BY e.snapshot_json")
            .bind(start).bind(start + 1).bind(&external).fetch_all(&database.pool).await.unwrap();
        for row in plan {
            println!("{}", row.try_get::<String, _>("QUERY PLAN").unwrap());
        }
    }
    sqlx::query("DELETE FROM request_record_locators WHERE tenant_id = $1 OR tenant_id = $2")
        .bind(&tenant)
        .bind(&other_tenant)
        .execute(&database.pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn sqlite_egress_summary_is_tenant_scoped_and_half_open_with_many_unrelated_locators() {
    let directory = tempfile::tempdir().unwrap();
    let database = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("egress-tenants.db").display()
    ))
    .await
    .unwrap();
    database.migrate().await.unwrap();
    tenant_summary_contract(&database).await;
}

#[tokio::test]
async fn postgres_egress_summary_is_tenant_scoped_and_records_the_actual_query_plan() {
    let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let database = Database::connect(&url).await.unwrap();
    database.migrate().await.unwrap();
    tenant_summary_contract(&database).await;
}

#[tokio::test]
async fn sqlite_egress_attempts_preserve_retries_snapshots_and_unknown_coverage() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("egress.db").display()
    );
    let database = Database::connect(&database_url).await.unwrap();
    database.migrate().await.unwrap();
    projection_contract(&database).await;
}

#[tokio::test]
async fn postgres_egress_attempts_preserve_retries_snapshots_and_unknown_coverage() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let database = Database::connect(&database_url).await.unwrap();
    database.migrate().await.unwrap();
    projection_contract(&database).await;
}
