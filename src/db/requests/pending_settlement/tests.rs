use super::*;
use crate::db::test_support::PostgresTestDatabase;
use crate::model::RequestUsageBasis;
use rust_decimal::Decimal;

struct Fixture {
    isolated: PostgresTestDatabase,
    database: Database,
    key: AuthenticatedKey,
    owner: ProxyRequestOwner,
    reservation: UsageReservation,
}

impl Fixture {
    async fn postgres(mode: EnforcementMode) -> Option<Self> {
        let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
            assert!(
                std::env::var_os("CI").is_none(),
                "PostgreSQL required in CI"
            );
            return None;
        };
        let isolated = PostgresTestDatabase::create(&url).await;
        let database = Database::connect_with_max(isolated.url(), 6).await.unwrap();
        database.migrate().await.unwrap();
        let pepper = b"pending settlement regression pepper";
        let issued = database
            .create_key(
                CreateKeyInput {
                    tenant_external_id: "receipt-test".into(),
                    principal_external_id: "member".into(),
                    alias: "receipt-test".into(),
                    currency: "USD".into(),
                    policy: KeyPolicy {
                        enforcement_mode: mode,
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
            .upsert_model_price("receipt-model", "USD", Decimal::ONE, Decimal::ONE)
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
                model: "receipt-model",
                request_object: "gap://receipt/request",
                upstream_account_id: None,
                model_route_id: None,
            })
            .await
            .unwrap();
        let owner = ProxyRequestOwner {
            request_id,
            tenant_id: key.tenant_id,
            key_id: key.key_id,
            reservation_id: reservation.id,
            token: Uuid::now_v7(),
        };
        assert!(database.acquire_proxy_request_owner(&owner).await.unwrap());
        Some(Self {
            isolated,
            database,
            key,
            owner,
            reservation,
        })
    }

    async fn claim(&self) -> PendingProxySettlementClaim {
        self.database
            .claim_pending_proxy_settlement(&self.owner, 10, 20, None)
            .await
            .unwrap()
    }

    fn terminal(&self) -> FinishProxyRequest<'_> {
        FinishProxyRequest {
            terminal_cause: None,
            usage_basis: Some(RequestUsageBasis::ProviderReported),
            first_output_ms: Some(2),
            generation_duration_ms: Some(7),
            request_id: self.owner.request_id,
            tenant_id: self.key.tenant_id,
            reservation: &self.reservation,
            input_token_ceiling: 10,
            output_token_ceiling: 20,
            requested_service_tier: None,
            status_code: 200,
            duration_ms: 10,
            usage: TokenUsage {
                input_tokens: 5,
                output_tokens: 7,
                ..TokenUsage::default()
            },
            error_code: None,
            response_object: "gap://receipt/response",
            routing_session_id: None,
            routing_terminal_observed_at: None,
            conversation: None,
        }
    }

    async fn finish(
        &self,
        claim: &PendingProxySettlementClaim,
        receipt_id: Uuid,
    ) -> Result<PendingProxySettlementAck, AppError> {
        self.database
            .finish_pending_proxy_settlement(
                claim,
                receipt_id,
                PendingProxySettlementEvidence::ProviderReported,
                self.terminal(),
                ProxyRequestUpstreamAttribution::KeepSelected,
            )
            .await
    }

    async fn assert_pending(&self) {
        let row = sqlx::query("SELECT p.state, p.receipt_id, r.status, q.completed_at, (SELECT COUNT(*) FROM ledger_entries WHERE source = r.id) AS ledgers FROM pending_proxy_settlements p JOIN usage_reservations r ON r.id = p.reservation_id JOIN request_records q ON q.id = p.request_id WHERE p.request_id = $1")
            .bind(self.owner.request_id.to_string()).fetch_one(&self.database.pool).await.unwrap();
        assert_eq!(row.get::<String, _>("state"), "pending");
        assert!(row.get::<Option<String>, _>("receipt_id").is_none());
        assert_eq!(row.get::<String, _>("status"), "reserved");
        assert!(row.get::<Option<i64>, _>("completed_at").is_none());
        assert_eq!(row.get::<i64, _>("ledgers"), 0);
    }

    async fn assert_applied_once(&self, cost: i64) {
        let row = sqlx::query("SELECT p.state, r.actual_micros, (SELECT COUNT(*) FROM ledger_entries WHERE source = r.id) AS ledgers, (SELECT COUNT(*) FROM terminal_projection_outbox WHERE request_id = p.request_id) AS outboxes FROM pending_proxy_settlements p JOIN usage_reservations r ON r.id = p.reservation_id WHERE p.request_id = $1")
            .bind(self.owner.request_id.to_string()).fetch_one(&self.database.pool).await.unwrap();
        assert_eq!(row.get::<String, _>("state"), "applied");
        assert_eq!(row.get::<i64, _>("actual_micros"), cost);
        assert_eq!(row.get::<i64, _>("ledgers"), 1);
        assert_eq!(row.get::<i64, _>("outboxes"), 1);
    }

    async fn close(self) {
        self.database.close().await;
        self.isolated.cleanup().await;
    }
}

#[tokio::test]
async fn pending_claim_survives_owner_expiry_and_rejects_zero_orphan_and_split_writers() {
    let Some(f) = Fixture::postgres(EnforcementMode::Prepaid).await else {
        return;
    };
    let claim = f.claim().await;
    assert_eq!(f.claim().await, claim);
    let delivery_marker: Option<String> =
        sqlx::query_scalar("SELECT error_code FROM request_records WHERE id = $1")
            .bind(f.owner.request_id.to_string())
            .fetch_one(&f.database.pool)
            .await
            .unwrap();
    assert!(delivery_marker.is_none(), "claim is not delivery evidence");
    sqlx::query("UPDATE usage_reservations SET created_at = $1 WHERE id = $2")
        .bind(unix_millis() - 31 * 60 * 1000)
        .bind(f.reservation.id.to_string())
        .execute(&f.database.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE request_records SET proxy_owner_expires_at = 0 WHERE id = $1")
        .bind(f.owner.request_id.to_string())
        .execute(&f.database.pool)
        .await
        .unwrap();
    f.database
        .mark_pending_proxy_settlement_unknown(&claim)
        .await
        .unwrap();
    assert_eq!(
        f.database.release_orphaned_reservations(10).await.unwrap(),
        0
    );
    // Models the reaper that selected a row before claim registration and only
    // reached the request CAS afterwards.
    let mut orphan = f.terminal();
    orphan.usage = TokenUsage::default();
    orphan.status_code = 504;
    orphan.usage_basis = Some(RequestUsageBasis::NotObserved);
    orphan.error_code = Some("request_expired");
    assert!(matches!(
        f.database
            .finish_orphaned_proxy_request(orphan.clone())
            .await,
        Err(AppError::Conflict(_))
    ));
    assert!(matches!(
        f.database.finish_proxy_request(orphan).await,
        Err(AppError::Conflict(_))
    ));
    assert!(matches!(
        f.database.settle_usage(&f.reservation, 0, 0).await,
        Err(AppError::Conflict(_))
    ));
    f.assert_pending().await;

    // Durable replay needs the persisted claim, not a still-live process lease.
    let receipt = Uuid::now_v7();
    let ack = f.finish(&claim, receipt).await.unwrap();
    assert_eq!(ack.cost_micros, 12);
    assert_eq!(f.finish(&claim, receipt).await.unwrap(), ack);
    assert!(matches!(
        f.finish(&claim, Uuid::now_v7()).await,
        Err(AppError::Conflict(_))
    ));
    let mut changed = f.terminal();
    changed.duration_ms += 1;
    assert!(matches!(
        f.database
            .finish_pending_proxy_settlement(
                &claim,
                receipt,
                PendingProxySettlementEvidence::ProviderReported,
                changed,
                ProxyRequestUpstreamAttribution::KeepSelected
            )
            .await,
        Err(AppError::Conflict(_))
    ));
    f.assert_applied_once(12).await;
    f.close().await;
}

#[tokio::test]
async fn receipt_ack_failure_rolls_back_money_and_terminal_outbox_before_concurrent_replay() {
    let Some(f) = Fixture::postgres(EnforcementMode::Prepaid).await else {
        return;
    };
    let claim = f.claim().await;
    let receipt = Uuid::now_v7();
    sqlx::raw_sql("CREATE FUNCTION reject_receipt_ack() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected receipt ack failure'; END $$; CREATE TRIGGER reject_receipt_ack BEFORE UPDATE ON pending_proxy_settlements FOR EACH ROW WHEN (NEW.state = 'applied') EXECUTE FUNCTION reject_receipt_ack();")
        .execute(&f.database.pool).await.unwrap();
    assert!(f.finish(&claim, receipt).await.is_err());
    f.assert_pending().await;
    let outboxes: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM terminal_projection_outbox WHERE request_id = $1")
            .bind(f.owner.request_id.to_string())
            .fetch_one(&f.database.pool)
            .await
            .unwrap();
    assert_eq!(outboxes, 0);
    sqlx::query("DROP TRIGGER reject_receipt_ack ON pending_proxy_settlements")
        .execute(&f.database.pool)
        .await
        .unwrap();
    let (first, second) = tokio::join!(f.finish(&claim, receipt), f.finish(&claim, receipt));
    assert_eq!(first.unwrap(), second.unwrap());
    f.assert_applied_once(12).await;
    f.close().await;
}

#[tokio::test]
async fn receipt_authority_rejects_foreign_contract_and_invalid_or_unobserved_usage() {
    let Some(f) = Fixture::postgres(EnforcementMode::MeteredUnlimited).await else {
        return;
    };
    let claim = f.claim().await;
    let mut wrong_owner = f.owner.clone();
    wrong_owner.token = Uuid::now_v7();
    assert!(matches!(
        f.database
            .claim_pending_proxy_settlement(&wrong_owner, 10, 20, None)
            .await,
        Err(AppError::Conflict(_))
    ));
    let mut wrong_claim = claim.clone();
    wrong_claim.fence = Uuid::now_v7();
    assert!(matches!(
        f.finish(&wrong_claim, Uuid::now_v7()).await,
        Err(AppError::Conflict(_))
    ));
    wrong_claim = claim.clone();
    wrong_claim.tenant_id = Uuid::now_v7();
    let mut foreign = f.terminal();
    foreign.tenant_id = wrong_claim.tenant_id;
    assert!(matches!(
        f.database
            .finish_pending_proxy_settlement(
                &wrong_claim,
                Uuid::now_v7(),
                PendingProxySettlementEvidence::ProviderReported,
                foreign,
                ProxyRequestUpstreamAttribution::KeepSelected
            )
            .await,
        Err(AppError::Conflict(_))
    ));
    wrong_claim = claim.clone();
    wrong_claim.input_token_ceiling = 11;
    wrong_claim.output_token_ceiling = 19;
    let mut altered = f.terminal();
    altered.input_token_ceiling = 11;
    altered.output_token_ceiling = 19;
    assert!(matches!(
        f.database
            .finish_pending_proxy_settlement(
                &wrong_claim,
                Uuid::now_v7(),
                PendingProxySettlementEvidence::ProviderReported,
                altered,
                ProxyRequestUpstreamAttribution::KeepSelected
            )
            .await,
        Err(AppError::Conflict(_))
    ));
    let mut invalid = f.terminal();
    invalid.usage.input_tokens = -1;
    assert!(matches!(
        f.database
            .finish_pending_proxy_settlement(
                &claim,
                Uuid::now_v7(),
                PendingProxySettlementEvidence::ProviderReported,
                invalid,
                ProxyRequestUpstreamAttribution::KeepSelected
            )
            .await,
        Err(AppError::Conflict(_))
    ));
    let mut unknown = f.terminal();
    unknown.usage = TokenUsage::default();
    unknown.usage_basis = Some(RequestUsageBasis::NotObserved);
    assert!(matches!(
        f.database
            .finish_pending_proxy_settlement(
                &claim,
                Uuid::now_v7(),
                PendingProxySettlementEvidence::ProviderReported,
                unknown,
                ProxyRequestUpstreamAttribution::KeepSelected
            )
            .await,
        Err(AppError::Conflict(_))
    ));
    f.assert_pending().await;
    let ack = f.finish(&claim, Uuid::now_v7()).await.unwrap();
    assert_eq!(ack.cost_micros, 12);
    f.assert_applied_once(12).await;
    f.close().await;
}

#[tokio::test]
async fn explicit_non_execution_is_required_for_a_zero_usage_receipt() {
    let Some(f) = Fixture::postgres(EnforcementMode::Prepaid).await else {
        return;
    };
    let claim = f.claim().await;
    let mut rejection = f.terminal();
    rejection.status_code = 503;
    rejection.error_code = Some("upstream_rejected_before_execution");
    rejection.usage = TokenUsage::default();
    rejection.usage_basis = Some(RequestUsageBasis::NotObserved);
    let receipt = Uuid::now_v7();
    let ack = f
        .database
        .finish_pending_proxy_settlement(
            &claim,
            receipt,
            PendingProxySettlementEvidence::ConfirmedNotExecuted,
            rejection.clone(),
            ProxyRequestUpstreamAttribution::KeepSelected,
        )
        .await
        .unwrap();
    let replay = f
        .database
        .finish_pending_proxy_settlement(
            &claim,
            receipt,
            PendingProxySettlementEvidence::ConfirmedNotExecuted,
            rejection,
            ProxyRequestUpstreamAttribution::KeepSelected,
        )
        .await
        .unwrap();
    assert_eq!(ack, replay);
    assert_eq!(ack.cost_micros, 0);
    f.assert_applied_once(0).await;
    f.close().await;
}
