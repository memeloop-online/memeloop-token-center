use super::*;
use crate::db::requests::pending_settlement::PendingProxySettlementEvidence;
use crate::db::test_support::PostgresTestDatabase;
use crate::model::RequestUsageBasis;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct LogCapture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogCapture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogCapture {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[test]
fn rejected_usage_diagnostics_keep_scalar_evidence_and_redact_unknown_tiers() {
    let capture = LogCapture::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .without_time()
        .with_writer(capture.clone())
        .finish();
    let usage = TokenUsage {
        input_tokens: 1000,
        cached_input_tokens: 20,
        cache_write_tokens: 3,
        output_tokens: 7,
        ..TokenUsage::default()
    };
    tracing::subscriber::with_default(subscriber, || {
        assert_eq!(
            normalize_proxy_usage_with_output_limit(&usage, 743, None, None).unwrap(),
            usage
        );
        for (reported, requested, limit) in [
            (None, None, Some(6)),
            (Some("priority"), None, None),
            (Some("Bearer private-provider-secret"), None, None),
            (None, Some("private-requested-tier"), None),
        ] {
            let usage = TokenUsage {
                service_tier: reported.map(str::to_owned),
                ..usage.clone()
            };
            assert!(
                normalize_proxy_usage_with_output_limit(&usage, 743, limit, requested).is_err()
            );
        }
    });
    let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert!(!logs.contains("private-provider-secret"));
    assert!(!logs.contains("Bearer"));
    assert!(!logs.contains("private-requested-tier"));
    let events: Vec<Value> = logs
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        events.len(),
        4,
        "accepted provider context emits no rejection"
    );
    for (event, reason, reported, requested) in [
        (
            &events[0],
            "explicit_output_limit_exceeded",
            "absent",
            "absent",
        ),
        (&events[1], "service_tier_mismatch", "priority", "absent"),
        (
            &events[2],
            "invalid_reported_service_tier",
            "invalid",
            "absent",
        ),
        (
            &events[3],
            "invalid_requested_service_tier",
            "absent",
            "invalid",
        ),
    ] {
        let fields = &event["fields"];
        assert_eq!(fields["reason"], reason);
        assert_eq!(fields["reported_input_tokens"], 1000);
        assert_eq!(fields["reported_cached_input_tokens"], 20);
        assert_eq!(fields["reported_cache_write_tokens"], 3);
        assert_eq!(fields["reported_output_tokens"], 7);
        assert_eq!(fields["admitted_input_tokens"], 743);
        assert_eq!(fields["reported_service_tier"], reported);
        assert_eq!(fields["requested_service_tier"], requested);
    }
    assert_eq!(events[0]["fields"]["admitted_output_token_limit"], 6);
    assert_eq!(events[0]["fields"]["has_output_token_limit"], true);
    assert_eq!(events[1]["fields"]["admitted_output_token_limit"], 0);
    assert_eq!(events[1]["fields"]["has_output_token_limit"], false);
}

async fn actual_input_above_estimate_settles_once(database: &Database) {
    for (mode, capped) in [
        (EnforcementMode::Prepaid, false),
        (EnforcementMode::Prepaid, true),
        (EnforcementMode::MeteredUnlimited, false),
    ] {
        let pepper = b"provider context settlement contract pepper";
        let unique = Uuid::now_v7().to_string();
        let issued = database
            .create_key(
                CreateKeyInput {
                    tenant_external_id: unique.clone(),
                    principal_external_id: "member".into(),
                    alias: unique,
                    currency: "USD".into(),
                    policy: KeyPolicy {
                        enforcement_mode: mode,
                        lifetime_budget: capped.then(|| "0.0001".to_owned()),
                        ..KeyPolicy::default()
                    },
                    initial_balance: Decimal::ONE,
                    idempotency_key: None,
                },
                pepper,
            )
            .await
            .unwrap();
        let key = database
            .authenticate_key(&issued.key, pepper)
            .await
            .unwrap();
        let price = database
            .upsert_model_price("provider-context", "USD", Decimal::ONE, Decimal::ONE)
            .await
            .unwrap();
        let request_id = Uuid::now_v7();
        let reservation = database
            .start_proxy_request(StartProxyRequest {
                request_id,
                key: &key,
                price: &price,
                input_token_ceiling: 10,
                output_token_ceiling: 20,
                protocol: "openai",
                model: "provider-context",
                request_object: "gap://provider-context/request",
                upstream_account_id: None,
                model_route_id: None,
            })
            .await
            .unwrap();
        assert_eq!(reservation.reserved_tokens, 30);
        let owner = ProxyRequestOwner {
            request_id,
            tenant_id: key.tenant_id,
            key_id: key.key_id,
            reservation_id: reservation.id,
            token: Uuid::now_v7(),
        };
        assert!(database.acquire_proxy_request_owner(&owner).await.unwrap());
        let claim = database
            .claim_pending_proxy_settlement(&owner, 10, 20, None)
            .await
            .unwrap();
        let receipt = Uuid::now_v7();
        let terminal = || FinishProxyRequest {
            terminal_cause: None,
            usage_basis: Some(RequestUsageBasis::ProviderReported),
            first_output_ms: None,
            generation_duration_ms: None,
            request_id,
            tenant_id: key.tenant_id,
            reservation: &reservation,
            input_token_ceiling: 10,
            output_token_ceiling: 20,
            requested_service_tier: None,
            status_code: 200,
            duration_ms: 10,
            usage: TokenUsage {
                input_tokens: 500,
                output_tokens: 7,
                ..TokenUsage::default()
            },
            error_code: None,
            response_object: "gap://provider-context/response",
            routing_session_id: None,
            routing_terminal_observed_at: None,
            conversation: None,
        };
        let finish = || {
            database.finish_pending_proxy_settlement(
                &claim,
                receipt,
                PendingProxySettlementEvidence::ProviderReported,
                terminal(),
                ProxyRequestUpstreamAttribution::KeepSelected,
            )
        };
        let (first, second) = tokio::join!(finish(), finish());
        let ack = first.unwrap();
        assert_eq!(second.unwrap(), ack);
        let cost = if capped { 100 } else { 507 };
        assert_eq!(ack.cost_micros, cost);
        let mut altered = terminal();
        altered.usage.input_tokens += 1;
        assert!(matches!(
            database
                .finish_pending_proxy_settlement(
                    &claim,
                    receipt,
                    PendingProxySettlementEvidence::ProviderReported,
                    altered,
                    ProxyRequestUpstreamAttribution::KeepSelected
                )
                .await,
            Err(AppError::Conflict(_))
        ));
        let mut forged = claim.clone();
        forged.input_token_ceiling += 1;
        let mut altered = terminal();
        altered.input_token_ceiling += 1;
        assert!(matches!(
            database
                .finish_pending_proxy_settlement(
                    &forged,
                    receipt,
                    PendingProxySettlementEvidence::ProviderReported,
                    altered,
                    ProxyRequestUpstreamAttribution::KeepSelected
                )
                .await,
            Err(AppError::Conflict(_))
        ));
        assert_eq!(finish().await.unwrap(), ack);
        let projection_owner = Uuid::now_v7();
        let tasks = database
            .claim_terminal_projection_tasks(projection_owner, 32)
            .await
            .unwrap();
        assert_eq!(tasks, vec![request_id]);
        assert!(
            database
                .project_claimed_terminal_projection_task(projection_owner, request_id)
                .await
                .unwrap()
        );
        assert!(
            !database
                .project_claimed_terminal_projection_task(projection_owner, request_id)
                .await
                .unwrap()
        );
        let row = sqlx::query("SELECT p.state, p.contract_digest, r.status, r.reserved_tokens, r.actual_micros, q.status_code, q.input_tokens, q.output_tokens, q.usage_basis, (SELECT COUNT(*) FROM ledger_entries l WHERE l.source = r.id AND l.kind = 'usage') AS ledgers, (SELECT CAST(SUM(amount_micros) AS BIGINT) FROM ledger_entries l WHERE l.source = r.id AND l.kind = 'usage') AS charged, (SELECT COUNT(*) FROM request_stats_facts f WHERE f.request_id = q.id) AS facts FROM pending_proxy_settlements p JOIN usage_reservations r ON r.id = p.reservation_id JOIN request_records q ON q.id = p.request_id WHERE p.request_id = $1")
            .bind(request_id.to_string()).fetch_one(&database.pool).await.unwrap();
        assert_eq!(row.get::<String, _>("state"), "applied");
        assert_eq!(
            row.get::<String, _>("contract_digest"),
            claim.contract_digest
        );
        assert_eq!(row.get::<String, _>("status"), "settled");
        assert_eq!(row.get::<i64, _>("reserved_tokens"), 30);
        assert_eq!(row.get::<i64, _>("actual_micros"), cost);
        assert_eq!(row.get::<i64, _>("status_code"), 200);
        assert_eq!(row.get::<i64, _>("input_tokens"), 500);
        assert_eq!(row.get::<i64, _>("output_tokens"), 7);
        assert_eq!(row.get::<String, _>("usage_basis"), "provider_reported");
        assert_eq!(row.get::<i64, _>("ledgers"), 1);
        assert_eq!(row.get::<i64, _>("charged"), -cost);
        assert_eq!(row.get::<i64, _>("facts"), 1);
        let snapshot = database.key_limit_snapshot(key.key_id).await.unwrap();
        assert_eq!(snapshot.reserved_balance, "0");
        assert_eq!(snapshot.concurrency.active, 0);
        if mode == EnforcementMode::Prepaid {
            assert_eq!(
                snapshot.available_balance,
                if capped { "0.9999" } else { "0.999493" }
            );
        }
    }
}

#[tokio::test]
async fn sqlite_provider_context_above_input_estimate_preserves_receipt_and_actual_charge() {
    let directory = tempfile::tempdir().unwrap();
    let database = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("provider-context.db").display()
    ))
    .await
    .unwrap();
    database.migrate().await.unwrap();
    actual_input_above_estimate_settles_once(&database).await;
    database.close().await;
}

#[tokio::test]
async fn postgres_provider_context_above_input_estimate_preserves_receipt_and_actual_charge() {
    let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        assert!(
            std::env::var_os("CI").is_none(),
            "PostgreSQL required in CI"
        );
        return;
    };
    let isolated = PostgresTestDatabase::create(&url).await;
    let database = Database::connect_with_max(isolated.url(), 6).await.unwrap();
    database.migrate().await.unwrap();
    actual_input_above_estimate_settles_once(&database).await;
    database.close().await;
    isolated.cleanup().await;
}
