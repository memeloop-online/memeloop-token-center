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

#[test]
fn concurrency_identity_aggregates_actual_endpoints_without_credentials_or_member_names() {
    let mut first = SelectedEgress::capture(Some("http://alice:secret@localhost:8080")).unwrap();
    let mut same = SelectedEgress::capture(Some("http://bob:other@localhost:8080")).unwrap();
    first.member_id = Some(Uuid::from_u128(1));
    same.member_id = Some(Uuid::from_u128(2));
    assert_eq!(first.concurrency_identity(), same.concurrency_identity());
    assert_ne!(
        first.concurrency_identity(),
        SelectedEgress::capture(Some("http://localhost:8081"))
            .unwrap()
            .concurrency_identity()
    );
    assert!(
        SelectedEgress::capture(None)
            .unwrap()
            .concurrency_identity()
            .is_none()
    );
}
