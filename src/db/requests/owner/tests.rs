use super::*;
use rust_decimal::Decimal;

struct Fixture {
    database: Database,
    replica: Database,
    admin: AnyPool,
    schema: String,
    replica_name: String,
    key: AuthenticatedKey,
    price: ModelPrice,
}

impl Fixture {
    async fn postgres() -> Option<Self> {
        let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
            assert!(
                std::env::var_os("CI").is_none(),
                "U02 PostgreSQL regressions require MTC_TEST_POSTGRES_URL in CI"
            );
            return None;
        };
        sqlx::any::install_default_drivers();
        let admin = AnyPoolOptions::new()
            .max_connections(3)
            .connect(&database_url)
            .await
            .unwrap();
        let schema = format!("proxy_owner_{}", Uuid::new_v4().simple());
        let replica_name = format!("{schema}_replica");
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await
            .unwrap();
        let mut url = url::Url::parse(&database_url).unwrap();
        url.query_pairs_mut()
            .append_pair("options", &format!("-csearch_path={schema}"));
        let database = Database::connect_with_max(url.as_str(), 6).await.unwrap();
        database.migrate().await.unwrap();
        url.query_pairs_mut()
            .append_pair("application_name", &replica_name);
        let replica = Database::connect_with_max(url.as_str(), 6).await.unwrap();
        let pepper = b"u02 ordinary owner regression pepper";
        let issued = database
            .create_key(
                CreateKeyInput {
                    tenant_external_id: schema.clone(),
                    principal_external_id: "owner".into(),
                    alias: "owner".into(),
                    currency: "USD".into(),
                    policy: KeyPolicy::default(),
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
            .upsert_model_price("owner-model", "USD", Decimal::ONE, Decimal::ONE)
            .await
            .unwrap();
        Some(Self {
            database,
            replica,
            admin,
            schema,
            replica_name,
            key,
            price,
        })
    }

    async fn request(&self) -> (ProxyRequestOwner, UsageReservation) {
        let request_id = Uuid::now_v7();
        let reservation = self
            .database
            .start_proxy_request(StartProxyRequest {
                request_id,
                key: &self.key,
                price: &self.price,
                input_token_ceiling: 10,
                output_token_ceiling: 20,
                protocol: "openai",
                model: "owner-model",
                request_object: "inline-json:{}",
                upstream_account_id: None,
                model_route_id: None,
            })
            .await
            .unwrap();
        sqlx::query("UPDATE usage_reservations SET created_at = $1 WHERE id = $2")
            .bind(unix_millis() - 31 * 60 * 1000)
            .bind(reservation.id.to_string())
            .execute(&self.database.pool)
            .await
            .unwrap();
        let owner = ProxyRequestOwner {
            request_id,
            tenant_id: self.key.tenant_id,
            key_id: self.key.key_id,
            reservation_id: reservation.id,
            token: Uuid::now_v7(),
        };
        assert!(
            self.database
                .acquire_proxy_request_owner(&owner)
                .await
                .unwrap()
        );
        (owner, reservation)
    }

    async fn expire(&self, owner: &ProxyRequestOwner) {
        sqlx::query("UPDATE request_records SET proxy_owner_expires_at = 0 WHERE id = $1")
            .bind(owner.request_id.to_string())
            .execute(&self.database.pool)
            .await
            .unwrap();
    }

    async fn assert_once(&self, owner: &ProxyRequestOwner, status: i64, cost: i64) {
        let row = sqlx::query("SELECT status_code, cost_micros, (SELECT COUNT(*) FROM ledger_entries WHERE source = request_records.reservation_id) AS ledgers, (SELECT COUNT(*) FROM request_events WHERE request_id = $1 AND event_kind = 'finished') AS events FROM request_records WHERE id = $1")
            .bind(owner.request_id.to_string()).fetch_one(&self.database.pool).await.unwrap();
        assert_eq!(row.get::<i64, _>("status_code"), status);
        assert_eq!(row.get::<i64, _>("cost_micros"), cost);
        assert_eq!(row.get::<i64, _>("ledgers"), 1);
        assert_eq!(row.get::<i64, _>("events"), 1);
        let row = sqlx::query("SELECT status, actual_micros FROM usage_reservations WHERE id = $1")
            .bind(owner.reservation_id.to_string())
            .fetch_one(&self.database.pool)
            .await
            .unwrap();
        assert_eq!(row.get::<String, _>("status"), "settled");
        assert_eq!(row.get::<i64, _>("actual_micros"), cost);
    }

    async fn wait_for_replica_lock(&self) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let waiting: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pg_stat_activity WHERE application_name = $1 AND wait_event_type = 'Lock'")
                    .bind(&self.replica_name).fetch_one(&self.admin).await.unwrap();
                if waiting != 0 { break; }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.expect("reaper must reach the real PostgreSQL row lock");
    }

    async fn close(self) {
        self.database.pool.close().await;
        self.replica.pool.close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP SCHEMA {} CASCADE",
            self.schema
        )))
        .execute(&self.admin)
        .await
        .unwrap();
        self.admin.close().await;
    }
}

fn terminal<'a>(
    owner: &ProxyRequestOwner,
    reservation: &'a UsageReservation,
    expired: bool,
) -> FinishProxyRequest<'a> {
    FinishProxyRequest {
        request_id: owner.request_id,
        tenant_id: owner.tenant_id,
        reservation,
        input_token_ceiling: 10,
        output_token_ceiling: 20,
        requested_service_tier: None,
        status_code: if expired { 504 } else { 200 },
        duration_ms: 31 * 60 * 1000,
        usage: if expired {
            TokenUsage::default()
        } else {
            TokenUsage {
                input_tokens: 5,
                output_tokens: 7,
                ..TokenUsage::default()
            }
        },
        usage_basis: Some(if expired {
            crate::model::RequestUsageBasis::NotObserved
        } else {
            crate::model::RequestUsageBasis::ProviderReported
        }),
        error_code: expired.then_some("request_expired"),
        response_object: "inline-json:{}",
        routing_session_id: None,
        routing_terminal_observed_at: None,
        conversation: None,
        terminal_cause: None,
        first_output_ms: None,
        generation_duration_ms: None,
    }
}

#[tokio::test]
async fn postgres_live_owner_survives_thirty_minutes_and_dead_owner_is_reaped_once() {
    let Some(fixture) = Fixture::postgres().await else {
        return;
    };
    let (owner, reservation) = fixture.request().await;
    let identity = crate::db::ArchiveSpoolIdentity {
        request_id: owner.request_id,
        tenant_id: owner.tenant_id,
        reservation_id: owner.reservation_id,
    };
    assert!(
        fixture
            .database
            .begin_response_archive_spool(identity)
            .await
            .unwrap()
    );
    sqlx::query("UPDATE response_archive_spools SET expires_at = $1 WHERE request_id = $2")
        .bind(unix_millis() + 10_000)
        .bind(owner.request_id.to_string())
        .execute(&fixture.database.pool)
        .await
        .unwrap();
    assert!(
        fixture
            .database
            .renew_proxy_request_owner(&owner)
            .await
            .unwrap()
    );
    let capture_expires: i64 =
        sqlx::query_scalar("SELECT expires_at FROM response_archive_spools WHERE request_id = $1")
            .bind(owner.request_id.to_string())
            .fetch_one(&fixture.database.pool)
            .await
            .unwrap();
    assert!(capture_expires > unix_millis() + 20 * 60 * 1000);
    let mut wrong_owner = owner.clone();
    wrong_owner.token = Uuid::now_v7();
    assert!(
        !fixture
            .replica
            .renew_proxy_request_owner(&wrong_owner)
            .await
            .unwrap()
    );
    assert!(
        !fixture
            .replica
            .acquire_proxy_request_owner(&wrong_owner)
            .await
            .unwrap()
    );
    assert_eq!(
        fixture
            .replica
            .release_orphaned_reservations(100)
            .await
            .unwrap(),
        0
    );
    assert!(matches!(
        fixture
            .replica
            .finish_orphaned_proxy_request(terminal(&owner, &reservation, true))
            .await,
        Err(AppError::Conflict(_))
    ));
    assert!(matches!(
        fixture
            .database
            .finish_proxy_request(terminal(&owner, &reservation, false))
            .await
            .unwrap(),
        FinishProxyRequestResult::Finished { .. }
    ));
    fixture.assert_once(&owner, 200, 12).await;
    assert!(
        fixture
            .database
            .proxy_request_owner_finished(&owner)
            .await
            .unwrap()
    );
    assert!(
        !fixture
            .replica
            .renew_proxy_request_owner(&owner)
            .await
            .unwrap()
    );

    let (dead, reservation) = fixture.request().await;
    fixture.expire(&dead).await;
    assert_eq!(
        fixture
            .replica
            .release_orphaned_reservations(100)
            .await
            .unwrap(),
        1
    );
    assert!(matches!(
        fixture
            .database
            .finish_proxy_request(terminal(&dead, &reservation, false))
            .await
            .unwrap(),
        FinishProxyRequestResult::AlreadyFinished {
            status_code: 504,
            ..
        }
    ));
    fixture.assert_once(&dead, 504, 0).await;
    assert!(
        !fixture
            .database
            .proxy_request_owner_finished(&dead)
            .await
            .unwrap()
    );
    assert_eq!(
        fixture
            .replica
            .release_orphaned_reservations(100)
            .await
            .unwrap(),
        0
    );
    fixture.close().await;
}

#[tokio::test]
async fn postgres_reaper_rechecks_renewal_under_lock_and_races_late_finish_once() {
    let Some(fixture) = Fixture::postgres().await else {
        return;
    };
    let (owner, reservation) = fixture.request().await;
    fixture.expire(&owner).await;
    let mut renewal = fixture.database.begin_write_transaction().await.unwrap();
    sqlx::query("UPDATE request_records SET proxy_owner_expires_at = $1 WHERE id = $2")
        .bind(unix_millis() + REQUEST_OWNER_LEASE_MILLIS)
        .bind(owner.request_id.to_string())
        .execute(&mut *renewal)
        .await
        .unwrap();
    let replica = fixture.replica.clone();
    let reaper = tokio::spawn(async move { replica.release_orphaned_reservations(100).await });
    fixture.wait_for_replica_lock().await;
    renewal.commit().await.unwrap();
    assert_eq!(reaper.await.unwrap().unwrap(), 0);
    fixture
        .database
        .finish_proxy_request(terminal(&owner, &reservation, false))
        .await
        .unwrap();
    fixture.assert_once(&owner, 200, 12).await;

    for _ in 0..4 {
        let (owner, reservation) = fixture.request().await;
        fixture.expire(&owner).await;
        let normal_input = terminal(&owner, &reservation, false);
        let expired_input = terminal(&owner, &reservation, true);
        let (normal, expired) = tokio::join!(
            fixture.database.finish_proxy_request(normal_input),
            fixture.replica.finish_orphaned_proxy_request(expired_input),
        );
        let normal = normal.unwrap();
        let expired = expired.unwrap();
        assert_eq!(
            usize::from(matches!(normal, FinishProxyRequestResult::Finished { .. }))
                + usize::from(matches!(expired, FinishProxyRequestResult::Finished { .. })),
            1
        );
        let (status, cost) = if matches!(normal, FinishProxyRequestResult::Finished { .. }) {
            (200, 12)
        } else {
            (504, 0)
        };
        fixture.assert_once(&owner, status, cost).await;
    }
    fixture.close().await;
}
