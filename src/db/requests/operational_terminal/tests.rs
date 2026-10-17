use super::*;
use crate::db::test_support::PostgresTestDatabase;
use crate::model::{RequestFinancialState, RequestLifecycleState, RequestUsageBasis};
use rust_decimal::Decimal;

struct Fixture {
    isolated: PostgresTestDatabase,
    database: Database,
    key: AuthenticatedKey,
    claim: PendingProxySettlementClaim,
    reservation: UsageReservation,
}

impl Fixture {
    async fn postgres() -> Option<Self> {
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
        let pepper = b"operational receipt regression";
        let issued = database
            .create_key(
                CreateKeyInput {
                    tenant_external_id: "operational-test".into(),
                    principal_external_id: "member".into(),
                    alias: "operational-test".into(),
                    currency: "USD".into(),
                    policy: KeyPolicy {
                        enforcement_mode: EnforcementMode::Prepaid,
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
            .upsert_model_price("operational-model", "USD", Decimal::ONE, Decimal::ONE)
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
                model: "operational-model",
                request_object: "gap://operational/request",
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
        let claim = database
            .claim_pending_proxy_settlement(&owner, 10, 20, None)
            .await
            .unwrap();
        Some(Self {
            isolated,
            database,
            key,
            claim,
            reservation,
        })
    }

    fn operational(&self, status: i64) -> PendingProxyOperationalTerminal {
        PendingProxyOperationalTerminal {
            status_code: status,
            error_code: Some(
                if status == 499 {
                    "client_cancelled"
                } else {
                    "upstream_read"
                }
                .into(),
            ),
            terminal_cause: Some(
                if status == 499 {
                    "client_disconnected"
                } else {
                    "upstream_stream_read_error"
                }
                .into(),
            ),
            stage: Some("response_delivery".into()),
            completed_at: unix_millis(),
            duration_ms: 10,
            first_output_ms: Some(2),
            generation_duration_ms: Some(7),
            response_object: "gap://operational/response".into(),
        }
    }

    fn financial(&self) -> FinishProxyRequest<'_> {
        FinishProxyRequest {
            request_id: self.claim.request_id,
            tenant_id: self.key.tenant_id,
            reservation: &self.reservation,
            input_token_ceiling: 10,
            output_token_ceiling: 20,
            requested_service_tier: None,
            status_code: 200,
            duration_ms: 10,
            first_output_ms: Some(2),
            generation_duration_ms: Some(7),
            terminal_cause: None,
            usage_basis: Some(RequestUsageBasis::ProviderReported),
            usage: TokenUsage {
                input_tokens: 5,
                output_tokens: 7,
                ..TokenUsage::default()
            },
            error_code: None,
            response_object: "gap://operational/response",
            routing_session_id: None,
            routing_terminal_observed_at: None,
            conversation: None,
        }
    }

    async fn record(
        &self,
        receipt: Uuid,
        terminal: &PendingProxyOperationalTerminal,
    ) -> Result<PendingProxyOperationalTerminalAck, AppError> {
        self.database
            .record_pending_proxy_operational_terminal(
                &self.claim,
                receipt,
                terminal,
                ProxyRequestUpstreamAttribution::KeepSelected,
            )
            .await
    }

    async fn finish(&self, receipt: Uuid) -> Result<PendingProxySettlementAck, AppError> {
        self.database
            .finish_pending_proxy_settlement(
                &self.claim,
                receipt,
                PendingProxySettlementEvidence::ProviderReported,
                self.financial(),
                ProxyRequestUpstreamAttribution::KeepSelected,
            )
            .await
    }

    async fn assert_unsettled(&self) {
        let row = sqlx::query("SELECT r.status, r.reserved_micros, r.actual_micros, q.completed_at, (SELECT COUNT(*) FROM ledger_entries WHERE source = r.id) AS ledgers, (SELECT COUNT(*) FROM terminal_projection_outbox WHERE request_id = q.id) AS outboxes, (SELECT COUNT(*) FROM request_stats_facts WHERE request_id = q.id) AS facts FROM usage_reservations r JOIN request_records q ON q.reservation_id = r.id WHERE r.id = $1")
            .bind(self.reservation.id.to_string()).fetch_one(&self.database.pool).await.unwrap();
        assert_eq!(row.get::<String, _>("status"), "reserved");
        assert_eq!(
            row.get::<i64, _>("reserved_micros"),
            self.reservation.reserved_micros
        );
        assert!(row.get::<Option<i64>, _>("completed_at").is_none());
        assert!(row.get::<Option<i64>, _>("actual_micros").is_none());
        for name in ["ledgers", "outboxes", "facts"] {
            assert_eq!(row.get::<i64, _>(name), 0, "{name}");
        }
    }

    async fn assert_views(
        &self,
        terminal: &PendingProxyOperationalTerminal,
        state: RequestFinancialState,
    ) {
        let filter = || RequestListFilter {
            limit: 1,
            status: Some("error".into()),
            error_code: terminal.error_code.clone(),
            ..RequestListFilter::default()
        };
        let lists = [
            self.database
                .list_requests_filtered(self.key.key_id, filter())
                .await
                .unwrap(),
            self.database
                .list_all_requests_filtered("operational-test", filter())
                .await
                .unwrap(),
            self.database
                .list_global_requests_filtered(filter())
                .await
                .unwrap(),
        ];
        for list in lists {
            assert_eq!(list.len(), 1);
            let view = &list[0];
            assert_eq!(view.request_id, self.claim.request_id);
            assert_eq!(view.status_code, Some(terminal.status_code));
            assert_eq!(view.error_code, terminal.error_code);
            assert_eq!(
                view.lifecycle_state,
                if terminal.status_code == 499 {
                    RequestLifecycleState::Cancelled
                } else {
                    RequestLifecycleState::Failed
                }
            );
            let operational = view.operational_terminal.as_ref().unwrap();
            assert_eq!(operational.completed_at, terminal.completed_at);
            assert_eq!(operational.stage, terminal.stage);
            assert_eq!(operational.financial_state, state);
            if state != RequestFinancialState::Applied {
                assert!(view.completed_at.is_none());
                assert!(view.usage.tokens.is_none());
                assert!(view.billing.cost.is_none());
                let json = serde_json::to_value(view).unwrap();
                assert!(json["input_tokens"].is_null());
                assert!(json["cost"].is_null());
            }
        }
        let details = [
            self.database
                .request_archive_refs(self.key.key_id, self.claim.request_id)
                .await
                .unwrap(),
            self.database
                .request_archive_refs_for_tenant("operational-test", self.claim.request_id)
                .await
                .unwrap(),
            self.database
                .request_archive_refs_global(self.claim.request_id)
                .await
                .unwrap(),
        ];
        for detail in details {
            assert_eq!(detail.view.status_code, Some(terminal.status_code));
            assert_eq!(
                detail.response_object.as_deref(),
                Some(terminal.response_object.as_str())
            );
            assert_eq!(
                detail.view.operational_terminal.unwrap().financial_state,
                state
            );
        }
        assert!(
            self.database
                .list_requests_filtered(
                    self.key.key_id,
                    RequestListFilter {
                        status: Some("pending".into()),
                        limit: 10,
                        ..RequestListFilter::default()
                    }
                )
                .await
                .unwrap()
                .is_empty()
        );
    }

    async fn close(self) {
        self.database.close().await;
        self.isolated.cleanup().await;
    }
}

#[tokio::test]
async fn operational_failure_is_visible_without_settling_unknown_usage_or_releasing_hold() {
    for status in [499, 502] {
        let Some(f) = Fixture::postgres().await else {
            return;
        };
        let terminal = f.operational(status);
        let receipt = Uuid::now_v7();
        f.database
            .mark_pending_proxy_settlement_unknown(&f.claim)
            .await
            .unwrap();
        let (first, replay) =
            tokio::join!(f.record(receipt, &terminal), f.record(receipt, &terminal));
        assert_eq!(first.unwrap(), replay.unwrap());
        f.assert_unsettled().await;
        f.assert_views(&terminal, RequestFinancialState::Unknown)
            .await;
        assert!(matches!(
            f.database.finish_proxy_request(f.financial()).await,
            Err(AppError::Conflict(_))
        ));
        assert!(matches!(
            f.database.settle_usage(&f.reservation, 0, 0).await,
            Err(AppError::Conflict(_))
        ));
        assert_eq!(
            f.database.release_orphaned_reservations(10).await.unwrap(),
            0
        );
        let ack = f.finish(receipt).await.unwrap();
        assert_eq!(ack.cost_micros, 12);
        assert_eq!(f.finish(receipt).await.unwrap(), ack);
        f.record(receipt, &terminal).await.unwrap();
        f.assert_views(&terminal, RequestFinancialState::Applied)
            .await;
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM ledger_entries WHERE source = $1")
                .bind(f.reservation.id.to_string())
                .fetch_one(&f.database.pool)
                .await
                .unwrap();
        assert_eq!(count, 1);
        f.close().await;
    }
}

#[tokio::test]
async fn operational_ack_binds_all_fields_and_full_authority_even_after_financial_ack() {
    let Some(f) = Fixture::postgres().await else {
        return;
    };
    let terminal = f.operational(499);
    let receipt = Uuid::now_v7();
    // The financial writer is allowed to commit first; runtime receipt still
    // records the client's actual cancellation without changing the charge.
    f.finish(receipt).await.unwrap();
    let original = f.record(receipt, &terminal).await.unwrap();
    for field in [
        "status_code",
        "error_code",
        "terminal_cause",
        "stage",
        "completed_at",
        "duration_ms",
        "first_output_ms",
        "generation_duration_ms",
        "response_object",
    ] {
        let mut value = serde_json::to_value(&terminal).unwrap();
        value[field] = match field {
            "status_code" => serde_json::json!(502),
            "error_code" => serde_json::json!("other_error"),
            "terminal_cause" => serde_json::json!("upstream_stream_read_error"),
            "stage" => serde_json::json!("upstream_response_body"),
            "completed_at" => serde_json::json!(terminal.completed_at + 1),
            "response_object" => serde_json::json!("gap://changed/response"),
            _ => serde_json::json!(99),
        };
        let changed: PendingProxyOperationalTerminal = serde_json::from_value(value).unwrap();
        assert!(
            matches!(
                f.record(receipt, &changed).await,
                Err(AppError::Conflict(_))
            ),
            "{field}"
        );
    }
    for field in [
        "request_id",
        "tenant_id",
        "key_id",
        "account_id",
        "reservation_id",
        "owner_token",
        "fence",
        "contract_digest",
        "input_token_ceiling",
        "output_token_ceiling",
        "requested_service_tier",
    ] {
        let mut value = serde_json::to_value(&f.claim).unwrap();
        value[field] = match field {
            "input_token_ceiling" | "output_token_ceiling" => serde_json::json!(99),
            "contract_digest" | "requested_service_tier" => serde_json::json!("changed"),
            _ => serde_json::json!(Uuid::now_v7()),
        };
        let claim: PendingProxySettlementClaim = serde_json::from_value(value).unwrap();
        assert!(
            matches!(
                f.database
                    .record_pending_proxy_operational_terminal(
                        &claim,
                        receipt,
                        &terminal,
                        ProxyRequestUpstreamAttribution::KeepSelected
                    )
                    .await,
                Err(AppError::Conflict(_))
            ),
            "{field}"
        );
    }
    assert!(matches!(
        f.database
            .record_pending_proxy_operational_terminal(
                &f.claim,
                receipt,
                &terminal,
                ProxyRequestUpstreamAttribution::LastDispatched(None)
            )
            .await,
        Err(AppError::Conflict(_))
    ));
    assert!(matches!(
        f.record(Uuid::now_v7(), &terminal).await,
        Err(AppError::Conflict(_))
    ));
    assert_eq!(f.record(receipt, &terminal).await.unwrap(), original);
    f.assert_views(&terminal, RequestFinancialState::Applied)
        .await;
    f.close().await;
}

#[tokio::test]
async fn operational_storage_failure_rolls_back_and_concurrent_financial_recovery_is_once() {
    let Some(f) = Fixture::postgres().await else {
        return;
    };
    let terminal = f.operational(502);
    let receipt = Uuid::now_v7();
    sqlx::raw_sql("CREATE FUNCTION reject_operational_terminal() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected operational persistence failure'; END $$; CREATE TRIGGER reject_operational_terminal BEFORE INSERT ON proxy_operational_terminals FOR EACH ROW EXECUTE FUNCTION reject_operational_terminal();")
        .execute(&f.database.pool).await.unwrap();
    assert!(f.record(receipt, &terminal).await.is_err());
    f.assert_unsettled().await;
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM proxy_operational_terminals")
        .fetch_one(&f.database.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    sqlx::query("DROP TRIGGER reject_operational_terminal ON proxy_operational_terminals")
        .execute(&f.database.pool)
        .await
        .unwrap();
    let (operational, financial) = tokio::join!(f.record(receipt, &terminal), f.finish(receipt));
    operational.unwrap();
    assert_eq!(financial.unwrap().cost_micros, 12);
    f.assert_views(&terminal, RequestFinancialState::Applied)
        .await;
    f.close().await;
}

#[tokio::test]
async fn operational_filters_precede_pagination_and_null_error_does_not_revive_delivery_marker() {
    use crate::filter_ast::{
        TypedFilterAst, TypedFilterCondition, TypedFilterField, TypedFilterOperator,
        TypedFilterValue,
    };
    let Some(f) = Fixture::postgres().await else {
        return;
    };
    let terminal = f.operational(502);
    f.record(Uuid::now_v7(), &terminal).await.unwrap();
    // A newer pending row must not consume the page before the error predicate.
    let price = f
        .database
        .upsert_model_price("operational-model", "USD", Decimal::ONE, Decimal::ONE)
        .await
        .unwrap();
    let newer = Uuid::now_v7();
    f.database
        .start_proxy_request(StartProxyRequest {
            request_id: newer,
            key: &f.key,
            price: &price,
            input_token_ceiling: 10,
            output_token_ceiling: 20,
            protocol: "openai",
            model: "operational-model",
            request_object: "gap://newer/request",
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap();
    let list = f
        .database
        .list_requests_filtered(
            f.key.key_id,
            RequestListFilter {
                limit: 1,
                typed_ast: Some(TypedFilterAst {
                    conditions: vec![
                        TypedFilterCondition {
                            field: TypedFilterField::Status,
                            operator: TypedFilterOperator::Equals,
                            value: TypedFilterValue::Status("error".into()),
                            upper: None,
                        },
                        TypedFilterCondition {
                            field: TypedFilterField::ErrorCode,
                            operator: TypedFilterOperator::Equals,
                            value: TypedFilterValue::Text("upstream_read".into()),
                            upper: None,
                        },
                        TypedFilterCondition {
                            field: TypedFilterField::DurationMs,
                            operator: TypedFilterOperator::GreaterThan,
                            value: TypedFilterValue::Integer(1),
                            upper: None,
                        },
                    ],
                    ..TypedFilterAst::default()
                }),
                ..RequestListFilter::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].request_id, f.claim.request_id);
    let pending = f
        .database
        .list_requests_filtered(
            f.key.key_id,
            RequestListFilter {
                limit: 1,
                status: Some("pending".into()),
                ..RequestListFilter::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(pending[0].request_id, newer);
    assert!(
        f.database
            .request_archive_refs(Uuid::now_v7(), f.claim.request_id)
            .await
            .is_err()
    );
    assert!(
        f.database
            .request_archive_refs_for_tenant("other-tenant", f.claim.request_id)
            .await
            .is_err()
    );
    f.close().await;

    let Some(f) = Fixture::postgres().await else {
        return;
    };
    sqlx::query("UPDATE request_records SET error_code = 'delivery_started' WHERE id = $1")
        .bind(f.claim.request_id.to_string())
        .execute(&f.database.pool)
        .await
        .unwrap();
    let mut complete = f.operational(200);
    complete.error_code = None;
    complete.terminal_cause = None;
    complete.stage = Some("buffered_response_ready".into());
    f.record(Uuid::now_v7(), &complete).await.unwrap();
    let views = f
        .database
        .list_requests_filtered(
            f.key.key_id,
            RequestListFilter {
                limit: 1,
                status: Some("success".into()),
                ..RequestListFilter::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(views.len(), 1);
    assert_eq!(views[0].lifecycle_state, RequestLifecycleState::Succeeded);
    assert!(views[0].error_code.is_none());
    assert!(views[0].billing.cost.is_none());
    assert!(views[0].usage.tokens.is_none());
    f.assert_unsettled().await;
    f.close().await;
}
