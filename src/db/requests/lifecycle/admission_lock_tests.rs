use super::*;
use rust_decimal::Decimal;

struct Fixture {
    database: Database,
    contender: Database,
    admin: AnyPool,
    schema: String,
    contender_name: String,
    key: AuthenticatedKey,
    price: ModelPrice,
}

impl Fixture {
    async fn postgres() -> Option<Self> {
        let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
            assert!(
                std::env::var_os("CI").is_none(),
                "admission lock regressions require PostgreSQL in CI"
            );
            return None;
        };
        sqlx::any::install_default_drivers();
        let admin = AnyPoolOptions::new()
            .max_connections(3)
            .connect(&database_url)
            .await
            .unwrap();
        let schema = format!("admission_lock_{}", Uuid::new_v4().simple());
        let contender_name = format!("{schema}_contender");
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await
            .unwrap();
        let mut url = url::Url::parse(&database_url).unwrap();
        url.query_pairs_mut()
            .append_pair("options", &format!("-csearch_path={schema}"));
        let database = Database::connect_with_max(url.as_str(), 4).await.unwrap();
        database.migrate().await.unwrap();
        url.query_pairs_mut()
            .append_pair("application_name", &contender_name);
        let contender = Database::connect_with_max(url.as_str(), 4).await.unwrap();
        let pepper = b"admission lock financial regression pepper";
        let issued = database
            .create_key(
                CreateKeyInput {
                    tenant_external_id: schema.clone(),
                    principal_external_id: "member".into(),
                    alias: "admission".into(),
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
            .upsert_model_price("admission-model", "USD", Decimal::ONE, Decimal::ONE)
            .await
            .unwrap();
        Some(Self {
            database,
            contender,
            admin: admin.into(),
            schema,
            contender_name,
            key,
            price,
        })
    }

    async fn wait_until_blocked_by(&self, blocker: i32) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let count: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM pg_stat_activity WHERE application_name = $1 AND wait_event_type = 'Lock' AND $2 = ANY(pg_blocking_pids(pid))",
                )
                .bind(&self.contender_name)
                .bind(blocker)
                .fetch_one(&self.admin)
                .await
                .unwrap();
                if count == 1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("contender must reach the expected PostgreSQL blocking edge");
    }

    async fn amounts(&self) -> (i64, i64, i64, i64) {
        sqlx::query_as(
            "SELECT a.available_micros, a.reserved_micros, k.settled_lifetime_micros, k.reserved_micros FROM credit_accounts a JOIN key_budget_state k ON k.key_id = $1 WHERE a.id = $2",
        )
        .bind(self.key.key_id.to_string())
        .bind(self.key.account_id.to_string())
        .fetch_one(&self.database.pool)
        .await
        .unwrap()
    }

    async fn close(self) {
        self.database.close().await;
        self.contender.close().await;
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

async fn admit(
    database: &Database,
    key: &AuthenticatedKey,
    price: &ModelPrice,
    request_id: Uuid,
) -> Result<UsageReservation, AppError> {
    database
        .start_proxy_forwarding_request_with_output_limit(
            StartProxyRequest {
                request_id,
                key,
                price,
                input_token_ceiling: 10,
                output_token_ceiling: 20,
                protocol: "openai",
                model: "admission-model",
                request_object: &format!("gap://{request_id}/request"),
                upstream_account_id: None,
                model_route_id: None,
            },
            Some("admission-upstream-model"),
            None,
        )
        .await
}

#[tokio::test]
async fn postgres_blocked_request_insert_does_not_hold_shared_key_budget() {
    let Some(fixture) = Fixture::postgres().await else {
        return;
    };
    // The selected upstream model must be in the INSERT, not a second UPDATE.
    for statement in [
        "CREATE FUNCTION reject_model_rewrite() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'unexpected admission model rewrite'; END $$",
        "CREATE TRIGGER reject_model_rewrite BEFORE UPDATE OF upstream_model ON request_records FOR EACH ROW EXECUTE FUNCTION reject_model_rewrite()",
    ] {
        sqlx::query(statement)
            .execute(&fixture.database.pool)
            .await
            .unwrap();
    }
    let mut blocker = fixture.admin.begin().await.unwrap();
    let blocker_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *blocker)
        .await
        .unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "LOCK TABLE {}.request_records IN SHARE MODE",
        fixture.schema
    )))
    .execute(&mut *blocker)
    .await
    .unwrap();
    let request_id = Uuid::now_v7();
    let contender = fixture.contender.clone();
    let key = fixture.key.clone();
    let price = fixture.price.clone();
    let pending = tokio::spawn(async move { admit(&contender, &key, &price, request_id).await });
    fixture.wait_until_blocked_by(blocker_pid).await;

    // The old ordering holds key_budget_state while stalled on the INSERT,
    // so this independent connection cannot reserve on the same key.
    let other = tokio::time::timeout(
        Duration::from_secs(5),
        fixture
            .database
            .reserve_usage(&fixture.key, &fixture.price, 10, 20),
    )
    .await
    .expect("request-record contention must not monopolize the key budget")
    .unwrap();
    assert_eq!(fixture.amounts().await, (999_970, 30, 0, 30));
    let visible: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM request_record_locators WHERE id = $1")
            .bind(request_id.to_string())
            .fetch_one(&fixture.database.pool)
            .await
            .unwrap();
    assert_eq!(visible, 0, "uncommitted request identity must stay private");

    blocker.rollback().await.unwrap();
    let reservation = pending.await.unwrap().unwrap();
    assert_ne!(reservation.id, other.id);
    assert_eq!(fixture.amounts().await, (999_940, 60, 0, 60));
    let stored: (String, String, String) = sqlx::query_as(
        "SELECT q.currency, q.upstream_model, r.price_snapshot_json FROM request_records q JOIN usage_reservations r ON r.id = q.reservation_id WHERE q.id = $1",
    )
    .bind(request_id.to_string())
    .fetch_one(&fixture.database.pool)
    .await
    .unwrap();
    assert_eq!(stored.0, "USD");
    assert_eq!(stored.1, "admission-upstream-model");
    assert_eq!(
        serde_json::from_str::<Value>(&stored.2)
            .unwrap()
            .get("output_token_limit"),
        Some(&Value::Null)
    );

    assert!(
        admit(&fixture.database, &fixture.key, &fixture.price, request_id)
            .await
            .is_err()
    );
    assert_eq!(fixture.amounts().await, (999_940, 60, 0, 60));
    let (first, replay) = tokio::join!(
        fixture.database.settle_usage(&reservation, 3, 5),
        fixture.contender.settle_usage(&reservation, 3, 5)
    );
    assert_eq!((first.unwrap(), replay.unwrap()), (8, 8));
    assert_eq!(fixture.amounts().await, (999_962, 30, 8, 30));
    let ledgers: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM ledger_entries WHERE source = $1 AND kind = 'usage'",
    )
    .bind(reservation.id.to_string())
    .fetch_one(&fixture.database.pool)
    .await
    .unwrap();
    assert_eq!(ledgers, 1);
    fixture.close().await;
}

#[tokio::test]
async fn postgres_rejected_admission_rolls_back_private_request_and_all_budget_changes() {
    let Some(fixture) = Fixture::postgres().await else {
        return;
    };
    let mut key = fixture.key.clone();
    key.policy.lifetime_budget = Some("0.000029".into());
    let request_id = Uuid::now_v7();
    let error = admit(&fixture.database, &key, &fixture.price, request_id)
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        AppError::LimitExceeded {
            reason: LimitReason::LifetimeBudgetExhausted,
            ..
        }
    ));
    assert_eq!(fixture.amounts().await, (1_000_000, 0, 0, 0));
    for statement in [
        "SELECT COUNT(*) FROM request_record_locators WHERE id = $1",
        "SELECT COUNT(*) FROM request_records WHERE id = $1",
        "SELECT COUNT(*) FROM request_events WHERE request_id = $1",
    ] {
        let rows: i64 = sqlx::query_scalar(statement)
            .bind(request_id.to_string())
            .fetch_one(&fixture.database.pool)
            .await
            .unwrap();
        assert_eq!(rows, 0);
    }
    let reservations: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM usage_reservations WHERE key_id = $1")
            .bind(key.key_id.to_string())
            .fetch_one(&fixture.database.pool)
            .await
            .unwrap();
    assert_eq!(reservations, 0);
    // A rejected private claim must not poison reuse of that server identity.
    admit(&fixture.database, &fixture.key, &fixture.price, request_id)
        .await
        .unwrap();
    assert_eq!(fixture.amounts().await, (999_970, 30, 0, 30));
    fixture.close().await;
}

#[tokio::test]
async fn postgres_resize_waits_for_key_without_blocking_once_only_settlement() {
    let Some(fixture) = Fixture::postgres().await else {
        return;
    };
    let reservation = fixture
        .database
        .reserve_usage(&fixture.key, &fixture.price, 10, 20)
        .await
        .unwrap();
    let mut settlement = fixture.database.begin_write_transaction().await.unwrap();
    let settlement_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *settlement)
        .await
        .unwrap();
    sqlx::query("UPDATE key_budget_state SET updated_at = updated_at WHERE key_id = $1")
        .bind(fixture.key.key_id.to_string())
        .execute(&mut *settlement)
        .await
        .unwrap();

    let contender = fixture.contender.clone();
    let key = fixture.key.clone();
    let price = fixture.price.clone();
    let resizing = reservation.clone();
    let pending = tokio::spawn(async move {
        let mut transaction = contender.begin_write_transaction().await.unwrap();
        let result = resize_usage_reservation_in_transaction(
            &mut transaction,
            &key,
            &price,
            &resizing,
            10,
            40,
            unix_millis(),
        )
        .await;
        transaction.rollback().await.unwrap();
        result
    });
    fixture.wait_until_blocked_by(settlement_pid).await;

    // Previously resize held the reservation before waiting for this key,
    // completing a reservation -> key -> reservation deadlock.
    let charged = tokio::time::timeout(
        Duration::from_secs(5),
        settle_token_usage_in_transaction(
            &mut settlement,
            &reservation,
            &TokenUsage {
                input_tokens: 3,
                output_tokens: 5,
                ..TokenUsage::default()
            },
            unix_millis(),
        ),
    )
    .await
    .expect("a resize waiting for the key must not own the reservation")
    .unwrap();
    assert_eq!(charged, 8);
    settlement.commit().await.unwrap();
    assert!(matches!(pending.await.unwrap(), Err(AppError::Conflict(_))));
    assert_eq!(fixture.amounts().await, (999_992, 0, 8, 0));
    assert_eq!(
        fixture
            .database
            .settle_usage(&reservation, 3, 5)
            .await
            .unwrap(),
        8
    );
    assert_eq!(fixture.amounts().await, (999_992, 0, 8, 0));
    fixture.close().await;
}

#[tokio::test]
async fn postgres_started_draft_known_partition_limits_relations_while_cursor_waits() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        assert!(std::env::var_os("CI").is_none());
        return;
    };
    let pool = sqlx::PgPool::connect(&database_url).await.unwrap();
    let mut blocker = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock($1,$2)")
        .bind(REQUEST_EVENT_CURSOR_LOCK_NAMESPACE)
        .bind(REQUEST_EVENT_CURSOR_LOCK_KEY)
        .execute(&mut *blocker)
        .await
        .unwrap();
    let mut counts = Vec::new();
    for bounded in [false, true] {
        let mut tx = pool.begin().await.unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        for statement in [
            "CREATE TEMP TABLE request_records(id TEXT, tenant_id TEXT, key_id TEXT, created_at BIGINT) PARTITION BY RANGE(created_at)",
            "CREATE TEMP TABLE draft_old PARTITION OF request_records FOR VALUES FROM(0) TO(1000)",
            "CREATE TEMP TABLE draft_current PARTITION OF request_records FOR VALUES FROM(1000) TO(2000)",
            "CREATE TEMP TABLE draft_new PARTITION OF request_records FOR VALUES FROM(2000) TO(3000)",
            "CREATE INDEX ON request_records(id)",
            "INSERT INTO request_records VALUES('request','tenant','key',1500)",
        ] {
            sqlx::query(statement).execute(&mut *tx).await.unwrap();
        }
        let sql = if bounded {
            STARTED_EVENT_DRAFT_SQL.to_owned()
        } else {
            STARTED_EVENT_DRAFT_SQL.replace(" AND created_at = $5", "")
        };
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind("event")
            .bind("tenant")
            .bind("key")
            .bind("request")
            .bind(1500_i64)
            .bind("openai")
            .bind("model")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert_eq!(row.try_get::<String, _>("request_id").unwrap(), "request");
        let pending = tokio::spawn(async move {
            sqlx::query("SELECT pg_advisory_xact_lock($1,$2)")
                .bind(REQUEST_EVENT_CURSOR_LOCK_NAMESPACE)
                .bind(REQUEST_EVENT_CURSOR_LOCK_KEY)
                .execute(&mut *tx)
                .await
                .unwrap();
            tx.rollback().await.unwrap();
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE pid=$1 AND wait_event='advisory')")
                    .bind(pid).fetch_one(&pool).await.unwrap();
                if waiting { break; }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.unwrap();
        let relations: Vec<String> = sqlx::query_scalar("SELECT DISTINCT c.relname FROM pg_locks l JOIN pg_class c ON c.oid=l.relation WHERE l.pid=$1 AND l.granted AND l.mode='AccessShareLock' AND c.relname LIKE 'draft_%' ORDER BY c.relname")
            .bind(pid).fetch_all(&pool).await.unwrap();
        assert!(relations.iter().any(|name| name == "draft_current"));
        if bounded {
            assert!(
                !relations
                    .iter()
                    .any(|name| name.starts_with("draft_old") || name.starts_with("draft_new")),
                "{relations:?}"
            );
        } else {
            assert!(relations.iter().any(|name| name == "draft_old"));
            assert!(relations.iter().any(|name| name == "draft_new"));
        }
        counts.push(relations.len());
        pending.abort();
        let _ = pending.await;
        // The dropped contender rolls back; the global cursor blocker remains held.
    }
    assert!(counts[1] < counts[0], "{counts:?}");
    blocker.rollback().await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn postgres_admission_model_insert_matches_previous_final_record() {
    let Some(fixture) = Fixture::postgres().await else {
        return;
    };
    let request = NewRequest {
        request_id: Uuid::now_v7(),
        tenant_id: fixture.key.tenant_id,
        key_id: fixture.key.key_id,
        reservation_id: Uuid::now_v7(),
        protocol: "openai".into(),
        model: "admission-model".into(),
        request_object: "gap://fixture/request".into(),
        upstream_account_id: None,
        model_route_id: None,
    };
    let now = unix_millis();
    let mut values = Vec::new();
    for combined in [false, true] {
        let mut tx = fixture.database.begin_write_transaction().await.unwrap();
        if combined {
            insert_request_started_record_with_model_in_transaction(
                &mut tx,
                &request,
                now,
                Some("selected-model"),
            )
            .await
            .unwrap();
        } else {
            insert_request_started_record_in_transaction(&mut tx, &request, now)
                .await
                .unwrap();
            sqlx::query(ADMISSION_UPSTREAM_MODEL_UPDATE)
                .bind("selected-model")
                .bind(request.request_id.to_string())
                .bind(request.reservation_id.to_string())
                .bind(now)
                .execute(&mut *tx)
                .await
                .unwrap();
        }
        let record: String = sqlx::query_scalar(
            "SELECT CAST(to_jsonb(r) AS TEXT) FROM request_records r WHERE id=$1 AND created_at=$2",
        )
        .bind(request.request_id.to_string())
        .bind(now)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        values.push(serde_json::from_str::<Value>(&record).unwrap());
        tx.rollback().await.unwrap();
    }
    assert_eq!(values[0], values[1]);
    fixture.close().await;
}
