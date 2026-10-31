use super::*;
use crate::db::test_support::PostgresTestDatabase;
use crate::model::{RequestFinancialState, RequestLifecycleState, RequestUsageBasis};
use rust_decimal::Decimal;

mod conversation_projection;
mod unknown_bridge;

struct Fixture {
    isolated: Option<PostgresTestDatabase>,
    directory: Option<tempfile::TempDir>,
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
        Some(Self::create(database, Some(isolated), "operational-test").await)
    }

    async fn sqlite() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let database = Database::connect(&format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("operational.db").display()
        ))
        .await
        .unwrap();
        let mut fixture = Self::create(database, None, "operational-test").await;
        fixture.directory = Some(directory);
        fixture
    }

    async fn create(
        database: Database,
        isolated: Option<PostgresTestDatabase>,
        tenant: &str,
    ) -> Self {
        Self::create_with_mode(database, isolated, tenant, EnforcementMode::Prepaid).await
    }

    async fn create_with_mode(
        database: Database,
        isolated: Option<PostgresTestDatabase>,
        tenant: &str,
        enforcement_mode: EnforcementMode,
    ) -> Self {
        Self::create_with_currency(database, isolated, tenant, enforcement_mode, "USD").await
    }

    async fn create_with_currency(
        database: Database,
        isolated: Option<PostgresTestDatabase>,
        tenant: &str,
        enforcement_mode: EnforcementMode,
        currency: &str,
    ) -> Self {
        database.migrate().await.unwrap();
        let pepper = b"operational receipt regression";
        let issued = database
            .create_key(
                CreateKeyInput {
                    tenant_external_id: tenant.into(),
                    principal_external_id: "member".into(),
                    alias: "operational-test".into(),
                    currency: currency.into(),
                    policy: KeyPolicy {
                        enforcement_mode,
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
            .upsert_model_price("operational-model", currency, Decimal::ONE, Decimal::ONE)
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
        Self {
            isolated,
            directory: None,
            database,
            key,
            claim,
            reservation,
        }
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
        for name in ["ledgers", "facts"] {
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
        if let Some(isolated) = self.isolated {
            isolated.cleanup().await;
        }
        drop(self.directory);
    }
}

#[tokio::test]
async fn sqlite_operational_statistics_preserve_unknown_finance_and_filters() {
    for status in [200, 429, 499, 502] {
        operational_statistics_contract(Fixture::sqlite().await, status).await;
    }
}

#[tokio::test]
async fn postgres_operational_statistics_preserve_unknown_finance_and_filters() {
    for status in [200, 429, 499, 502] {
        let Some(fixture) = Fixture::postgres().await else {
            return;
        };
        operational_statistics_contract(fixture, status).await;
    }
}

#[tokio::test]
async fn sqlite_unsettled_requests_count_as_failures_in_success_rates() {
    unsettled_requests_count_as_failures_in_success_rates(Fixture::sqlite().await).await;
}

#[tokio::test]
async fn postgres_unsettled_requests_count_as_failures_in_success_rates() {
    let Some(fixture) = Fixture::postgres().await else {
        return;
    };
    unsettled_requests_count_as_failures_in_success_rates(fixture).await;
}

async fn unsettled_requests_count_as_failures_in_success_rates(fixture: Fixture) {
    let created_at: i64 =
        sqlx::query_scalar("SELECT created_at FROM request_record_locators WHERE id = $1")
            .bind(fixture.claim.request_id.to_string())
            .fetch_one(&fixture.database.pool)
            .await
            .unwrap();
    let filter = StatsFilter {
        from_created_at: Some(created_at),
        to_created_at: Some(created_at),
        ..StatsFilter::default()
    };
    let unsettled = fixture
        .database
        .stats_filtered(fixture.key.key_id, filter.clone())
        .await
        .unwrap();
    assert_eq!(unsettled.summary.total_requests, 1);
    assert_eq!(unsettled.summary.successful_requests, 0);
    assert_eq!(unsettled.summary.failed_requests, 0);
    assert_eq!(unsettled.summary.pending_requests, 1);
    assert_eq!(unsettled.summary.total_cost, None);
    assert!(unsettled.summary.costs.is_empty());
    assert_eq!(unsettled.by_model.len(), 1);
    assert_eq!(unsettled.by_model[0].requests, 1);
    assert_eq!(unsettled.by_day.len(), 1);
    assert_eq!(unsettled.by_day[0].requests, 1);
    let errored = fixture
        .database
        .stats_filtered(
            fixture.key.key_id,
            StatsFilter {
                status: Some("error".into()),
                ..filter.clone()
            },
        )
        .await
        .unwrap();
    assert_eq!(errored.summary.total_requests, 0);
    assert_eq!(errored.summary.failed_requests, 0);
    let succeeded = fixture
        .database
        .stats_filtered(
            fixture.key.key_id,
            StatsFilter {
                status: Some("success".into()),
                ..filter.clone()
            },
        )
        .await
        .unwrap();
    assert_eq!(succeeded.summary.total_requests, 0);
    // The dedicated pending operational view keeps its own classification.
    let pending = fixture
        .database
        .stats_filtered(
            fixture.key.key_id,
            StatsFilter {
                status: Some("pending".into()),
                ..filter.clone()
            },
        )
        .await
        .unwrap();
    assert_eq!(pending.summary.total_requests, 1);
    assert_eq!(pending.summary.failed_requests, 0);

    // Once a 499 operational terminal is recorded it stays exactly one failure.
    let terminal = fixture.operational(499);
    fixture.record(Uuid::now_v7(), &terminal).await.unwrap();
    let cancelled = fixture
        .database
        .stats_filtered(fixture.key.key_id, filter.clone())
        .await
        .unwrap();
    assert_eq!(cancelled.summary.total_requests, 1);
    assert_eq!(cancelled.summary.successful_requests, 0);
    assert_eq!(cancelled.summary.failed_requests, 1);
    assert_eq!(cancelled.errors.len(), 1);
    assert_eq!(cancelled.errors[0].name, "client_cancelled");
    fixture.close().await;
}

async fn operational_statistics_contract(fixture: Fixture, status: i64) {
    let mut terminal = fixture.operational(status);
    if status == 200 {
        terminal.error_code = None;
        terminal.terminal_cause = None;
    }
    let upstream = Uuid::now_v7();
    let route = Uuid::now_v7();
    let receipt = Uuid::now_v7();
    fixture
        .database
        .record_pending_proxy_operational_terminal(
            &fixture.claim,
            receipt,
            &terminal,
            ProxyRequestUpstreamAttribution::LastDispatched(Some((upstream, route))),
        )
        .await
        .unwrap();
    fixture
        .database
        .mark_pending_proxy_settlement_unknown(&fixture.claim)
        .await
        .unwrap();
    let created_at: i64 =
        sqlx::query_scalar("SELECT created_at FROM request_record_locators WHERE id = $1")
            .bind(fixture.claim.request_id.to_string())
            .fetch_one(&fixture.database.pool)
            .await
            .unwrap();
    let filter = StatsFilter {
        from_created_at: Some(created_at),
        to_created_at: Some(created_at),
        ..StatsFilter::default()
    };
    let stats = fixture
        .database
        .stats_filtered(fixture.key.key_id, filter.clone())
        .await
        .unwrap();
    assert_eq!(stats.summary.total_requests, 1);
    assert_eq!(stats.summary.successful_requests, i64::from(status == 200));
    assert_eq!(stats.summary.failed_requests, i64::from(status != 200));
    assert_eq!(stats.summary.total_cost, None);
    assert!(stats.summary.costs.is_empty());
    assert_eq!(stats.summary.input_tokens, 0);
    assert_eq!(stats.summary.output_tokens, 0);
    assert_eq!(stats.summary.cache_usage.unknown_requests, 1);
    assert_eq!(stats.by_model.len(), 1);
    assert_eq!(stats.by_day.len(), 1);
    for bucket in stats
        .by_model
        .iter()
        .chain(&stats.by_day)
        .chain(&stats.errors)
    {
        assert_eq!(bucket.requests, 1);
        assert_eq!(bucket.cost, None);
        assert!(bucket.costs.is_empty());
    }
    assert_eq!(stats.errors.len(), usize::from(status != 200));
    fixture.assert_unsettled().await;
    let day_start = created_at / 86_400_000 * 86_400_000;
    let complete_day = StatsFilter {
        from_created_at: Some(day_start),
        to_created_at: Some(day_start + 86_400_000 - 1),
        ..filter.clone()
    };
    let complete_day_stats = fixture
        .database
        .stats_filtered(fixture.key.key_id, complete_day)
        .await
        .unwrap();
    assert_eq!(complete_day_stats.summary.total_requests, 1);
    assert_eq!(complete_day_stats.summary.total_cost, None);

    let other_key = Fixture::create(fixture.database.clone(), None, "operational-test").await;
    other_key
        .record(Uuid::now_v7(), &other_key.operational(status))
        .await
        .unwrap();
    let foreign = Fixture::create(fixture.database.clone(), None, "foreign-tenant").await;
    foreign
        .record(Uuid::now_v7(), &foreign.operational(status))
        .await
        .unwrap();
    let scope_filter = StatsFilter {
        from_created_at: Some(created_at),
        to_created_at: Some(unix_millis()),
        ..StatsFilter::default()
    };
    assert_eq!(
        fixture
            .database
            .global_operator_stats_filtered(scope_filter.clone())
            .await
            .unwrap()
            .summary
            .total_requests,
        3
    );
    assert_eq!(
        fixture
            .database
            .operator_stats_filtered("operational-test", scope_filter.clone())
            .await
            .unwrap()
            .summary
            .total_requests,
        2
    );
    assert_eq!(
        fixture
            .database
            .operator_stats_filtered("foreign-tenant", scope_filter)
            .await
            .unwrap()
            .summary
            .total_requests,
        1
    );

    let matching = StatsFilter {
        key_id: Some(fixture.key.key_id),
        model: Some("operational-model".into()),
        protocol: Some("openai".into()),
        status: Some(if status == 200 { "success" } else { "error" }.into()),
        error_code: terminal.error_code.clone(),
        upstream_account_id: Some(upstream),
        route_id: Some(route),
        min_duration_ms: Some(10),
        max_duration_ms: Some(10),
        key_alias: Some("OPERATIONAL".into()),
        principal: Some("MEM".into()),
        ..filter.clone()
    };
    assert_eq!(
        fixture
            .database
            .operator_stats_filtered("operational-test", matching.clone())
            .await
            .unwrap()
            .summary
            .total_requests,
        1
    );
    assert_eq!(
        fixture
            .database
            .global_operator_stats_filtered(matching.clone())
            .await
            .unwrap()
            .summary
            .total_requests,
        1
    );
    assert_eq!(
        fixture
            .database
            .operator_stats_filtered("foreign-tenant", matching.clone())
            .await
            .unwrap()
            .summary
            .total_requests,
        0
    );
    let mut forged = matching.clone();
    forged.key_id = Some(Uuid::now_v7());
    assert_eq!(
        fixture
            .database
            .stats_filtered(fixture.key.key_id, forged.clone())
            .await
            .unwrap()
            .summary
            .total_requests,
        1
    );
    assert_eq!(
        fixture
            .database
            .global_operator_stats_filtered(forged)
            .await
            .unwrap()
            .summary
            .total_requests,
        0
    );
    let mut excluded = Vec::new();
    let mut changed = matching.clone();
    changed.model = Some("foreign-model".into());
    excluded.push(changed);
    let mut changed = matching.clone();
    changed.protocol = Some("anthropic".into());
    excluded.push(changed);
    let mut changed = matching.clone();
    changed.status = Some(if status == 200 { "error" } else { "success" }.into());
    excluded.push(changed);
    let mut changed = matching.clone();
    changed.status = Some("pending".into());
    excluded.push(changed);
    let mut changed = matching.clone();
    changed.error_code = Some("foreign-error".into());
    excluded.push(changed);
    let mut changed = matching.clone();
    changed.upstream_account_id = Some(Uuid::now_v7());
    excluded.push(changed);
    let mut changed = matching.clone();
    changed.route_id = Some(Uuid::now_v7());
    excluded.push(changed);
    let mut changed = matching.clone();
    changed.min_duration_ms = Some(11);
    changed.max_duration_ms = None;
    excluded.push(changed);
    let mut changed = matching.clone();
    changed.min_duration_ms = None;
    changed.max_duration_ms = Some(9);
    excluded.push(changed);
    let mut changed = matching.clone();
    changed.min_cost_micros = Some(0);
    excluded.push(changed);
    let mut changed = matching.clone();
    changed.max_cost_micros = Some(1000);
    excluded.push(changed);
    let mut changed = matching.clone();
    changed.key_alias = Some("foreign".into());
    excluded.push(changed);
    let mut changed = matching.clone();
    changed.principal = Some("foreign".into());
    excluded.push(changed);
    let mut changed = matching.clone();
    changed.from_created_at = Some(created_at + 1);
    changed.to_created_at = Some(created_at + 1);
    excluded.push(changed);
    let mut changed = matching.clone();
    changed.from_created_at = Some(created_at - 1);
    changed.to_created_at = Some(created_at - 1);
    excluded.push(changed);
    for excluded_filter in excluded {
        assert_eq!(
            fixture
                .database
                .global_operator_stats_filtered(excluded_filter.clone())
                .await
                .unwrap()
                .summary
                .total_requests,
            0,
            "{excluded_filter:?}"
        );
    }

    let mut financial = fixture.financial();
    financial.status_code = terminal.status_code;
    financial.error_code = terminal.error_code.as_deref();
    financial.terminal_cause = if status == 200 {
        None
    } else {
        Some(crate::model::RequestTerminalCause::StreamReadError)
    };
    let ack = fixture
        .database
        .finish_pending_proxy_settlement(
            &fixture.claim,
            receipt,
            PendingProxySettlementEvidence::ProviderReported,
            financial,
            ProxyRequestUpstreamAttribution::LastDispatched(Some((upstream, route))),
        )
        .await
        .unwrap();
    assert_eq!(ack.cost_micros, 12);
    let awaiting_projection = fixture
        .database
        .stats_filtered(fixture.key.key_id, matching.clone())
        .await
        .unwrap();
    assert_eq!(awaiting_projection.summary.total_requests, 1);
    assert_eq!(
        awaiting_projection.summary.total_cost.as_deref(),
        Some("0.000012")
    );
    let owner = Uuid::now_v7();
    assert_eq!(
        fixture
            .database
            .claim_terminal_projection_tasks(owner, 10)
            .await
            .unwrap(),
        vec![fixture.claim.request_id]
    );
    assert!(
        fixture
            .database
            .project_claimed_terminal_projection_task(owner, fixture.claim.request_id)
            .await
            .unwrap()
    );
    assert!(
        !fixture
            .database
            .project_claimed_terminal_projection_task(owner, fixture.claim.request_id)
            .await
            .unwrap()
    );
    for window in [
        filter.clone(),
        StatsFilter {
            from_created_at: Some(day_start),
            to_created_at: Some(day_start + 86_400_000 - 1),
            ..filter
        },
    ] {
        for use_facts in [false, true] {
            let selected = StatsFilter {
                min_duration_ms: use_facts.then_some(0),
                ..window.clone()
            };
            let stats = fixture
                .database
                .stats_filtered(fixture.key.key_id, selected)
                .await
                .unwrap();
            assert_eq!(stats.summary.total_requests, 1);
            assert_eq!(stats.summary.failed_requests, i64::from(status != 200));
            assert_eq!(stats.summary.successful_requests, i64::from(status == 200));
            assert_eq!(stats.summary.total_cost.as_deref(), Some("0.000012"));
            assert_eq!(stats.summary.input_tokens, 5);
            assert_eq!(stats.summary.output_tokens, 7);
        }
    }
    fixture.close().await;
}

#[tokio::test]
async fn sqlite_operational_cached_usage_is_unchanged_by_financial_fact_projection() {
    for denominator in [Some(8), None] {
        operational_cached_usage_projection_contract(Fixture::sqlite().await, denominator).await;
    }
}

#[tokio::test]
async fn postgres_operational_cached_usage_is_unchanged_by_financial_fact_projection() {
    for denominator in [Some(8), None] {
        let Some(fixture) = Fixture::postgres().await else {
            return;
        };
        operational_cached_usage_projection_contract(fixture, denominator).await;
    }
}

async fn operational_cached_usage_projection_contract(fixture: Fixture, denominator: Option<i64>) {
    let mut terminal = fixture.operational(200);
    terminal.error_code = None;
    terminal.terminal_cause = None;
    let receipt = Uuid::now_v7();
    fixture.record(receipt, &terminal).await.unwrap();
    let created_at: i64 =
        sqlx::query_scalar("SELECT created_at FROM request_record_locators WHERE id = $1")
            .bind(fixture.claim.request_id.to_string())
            .fetch_one(&fixture.database.pool)
            .await
            .unwrap();
    let filter = StatsFilter {
        from_created_at: Some(created_at),
        to_created_at: Some(created_at),
        ..StatsFilter::default()
    };
    let unknown = fixture
        .database
        .stats_filtered(fixture.key.key_id, filter.clone())
        .await
        .unwrap();
    assert_eq!(unknown.summary.total_cost, None);
    assert_eq!(unknown.summary.cache_usage.unknown_requests, 1);
    assert_eq!(unknown.summary.cache_usage.reported_requests, 0);
    let mut financial = fixture.financial();
    financial.usage.cached_input_tokens = 3;
    financial.usage.cache_coverage = Some(crate::model::CacheUsageCoverage {
        read_tokens: 3,
        inclusive_input_tokens: denominator,
    });
    fixture
        .database
        .finish_pending_proxy_settlement(
            &fixture.claim,
            receipt,
            PendingProxySettlementEvidence::ProviderReported,
            financial,
            ProxyRequestUpstreamAttribution::KeepSelected,
        )
        .await
        .unwrap();
    let before = fixture
        .database
        .stats_filtered(fixture.key.key_id, filter.clone())
        .await
        .unwrap();
    let cache = &before.summary.cache_usage;
    assert_eq!(
        cache.known_read_tokens,
        if denominator.is_some() { 3 } else { 0 }
    );
    assert_eq!(cache.known_input_tokens, denominator.unwrap_or(0));
    assert_eq!(cache.eligible_requests, i64::from(denominator.is_some()));
    assert_eq!(cache.unknown_requests, i64::from(denominator.is_none()));
    assert_eq!(cache.reported_read_tokens, 3);
    assert_eq!(cache.reported_requests, 1);
    assert_eq!(cache.hit_rate, denominator.map(|input| 3.0 / input as f64));
    assert_eq!(before.summary.total_requests, 1);
    assert!(before.summary.total_cost.is_some());
    let fact_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM request_stats_facts WHERE request_id = $1")
            .bind(fixture.claim.request_id.to_string())
            .fetch_one(&fixture.database.pool)
            .await
            .unwrap();
    assert_eq!(fact_count, 0);
    let owner = Uuid::now_v7();
    assert_eq!(
        fixture
            .database
            .claim_terminal_projection_tasks(owner, 10)
            .await
            .unwrap(),
        vec![fixture.claim.request_id]
    );
    assert!(
        fixture
            .database
            .project_claimed_terminal_projection_task(owner, fixture.claim.request_id)
            .await
            .unwrap()
    );
    let after = fixture
        .database
        .stats_filtered(fixture.key.key_id, filter)
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(&before).unwrap(),
        serde_json::to_value(&after).unwrap()
    );
    let day_start = created_at / 86_400_000 * 86_400_000;
    let daily = fixture
        .database
        .stats_filtered(
            fixture.key.key_id,
            StatsFilter {
                from_created_at: Some(day_start),
                to_created_at: Some(day_start + 86_400_000 - 1),
                ..StatsFilter::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(&after).unwrap(),
        serde_json::to_value(&daily).unwrap()
    );
    fixture.close().await;
}

#[tokio::test]
async fn sqlite_mixed_operational_statistics_do_not_report_partial_cost_as_total() {
    mixed_operational_cost_contract(Fixture::sqlite().await).await;
}

#[tokio::test]
async fn postgres_mixed_operational_statistics_do_not_report_partial_cost_as_total() {
    let Some(fixture) = Fixture::postgres().await else {
        return;
    };
    mixed_operational_cost_contract(fixture).await;
}

async fn mixed_operational_cost_contract(fixture: Fixture) {
    let terminal = fixture.operational(429);
    fixture.record(Uuid::now_v7(), &terminal).await.unwrap();
    for _request in 0..50 {
        sqlx::query("INSERT INTO request_stats_facts (request_id, tenant_id, key_id, created_at, model, protocol, status_class, error_code, upstream_account_id, model_route_id, duration_ms, input_tokens, output_tokens, currency, cost_micros) SELECT $1, tenant_id, key_id, created_at, model, protocol, 'success', '', '', '', 10, 5, 7, currency, 12 FROM request_records WHERE id = $2")
            .bind(Uuid::now_v7().to_string()).bind(fixture.claim.request_id.to_string()).execute(&fixture.database.pool).await.unwrap();
    }
    let created_at: i64 =
        sqlx::query_scalar("SELECT created_at FROM request_record_locators WHERE id = $1")
            .bind(fixture.claim.request_id.to_string())
            .fetch_one(&fixture.database.pool)
            .await
            .unwrap();
    let filter = StatsFilter {
        from_created_at: Some(created_at),
        to_created_at: Some(created_at),
        ..StatsFilter::default()
    };
    let stats = fixture
        .database
        .stats_filtered(fixture.key.key_id, filter.clone())
        .await
        .unwrap();
    assert_eq!(
        [
            stats.summary.total_requests,
            stats.summary.successful_requests,
            stats.summary.failed_requests
        ],
        [51, 50, 1]
    );
    assert_eq!(stats.summary.total_cost, None);
    assert!(stats.summary.costs.is_empty());
    for bucket in stats.by_model.iter().chain(&stats.by_day) {
        assert_eq!(bucket.requests, 51);
        assert_eq!(bucket.cost, None);
        assert!(bucket.costs.is_empty());
    }
    let known_only = fixture
        .database
        .stats_filtered(
            fixture.key.key_id,
            StatsFilter {
                min_cost_micros: Some(0),
                ..filter.clone()
            },
        )
        .await
        .unwrap();
    assert_eq!(known_only.summary.total_requests, 50);
    assert_eq!(known_only.summary.total_cost.as_deref(), Some("0.0006"));
    sqlx::query("UPDATE request_stats_facts SET currency = 'EUR' WHERE key_id = $1")
        .bind(fixture.key.key_id.to_string())
        .execute(&fixture.database.pool)
        .await
        .unwrap();
    let multiple_currencies = fixture
        .database
        .stats_filtered(fixture.key.key_id, filter)
        .await
        .unwrap();
    assert_eq!(multiple_currencies.summary.total_requests, 51);
    assert_eq!(multiple_currencies.summary.total_cost, None);
    assert_eq!(multiple_currencies.summary.costs.len(), 1);
    assert_eq!(multiple_currencies.summary.costs[0].currency, "EUR");
    assert_eq!(multiple_currencies.summary.costs[0].cost, "0.0006");
    fixture.close().await;
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
