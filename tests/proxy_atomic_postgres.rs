use memeloop_token_center::{
    archive_staging::{
        ArchiveStagingIntentDigest, ArchiveStagingKey, ArchiveStagingLeaseOwner,
        ArchiveStagingOwner, ArchiveStagingPurpose, ArchiveStagingState, BeginArchiveStagingInput,
        BeginArchiveStagingResult,
    },
    db::{
        CreateKeyInput, CreateModelRouteInput, CreateUpstreamAccountInput, Database,
        FinishProxyRequest, FinishProxyRequestResult, FinishSynchronousImageRequest,
        FinishSynchronousImageResult, StartProxyRequest, StartSynchronousImageRequest,
        StartSynchronousImageResult, StatsFilter, unix_millis,
    },
    model::{ArchivedGenerationAsset, KeyPolicy, TokenUsage},
    provider::UpstreamCredential,
};
use rust_decimal::Decimal;
use sqlx::PgPool;
use uuid::Uuid;

static POSTGRES_TEST_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn postgres_pending_proxy_failover_assignment_is_tenant_scoped_and_idempotent() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let _postgres_test_guard = POSTGRES_TEST_SERIAL.lock().await;
    let database = Database::connect_with_max(&database_url, 8).await.unwrap();
    database.migrate().await.unwrap();
    let unique = Uuid::now_v7();
    let tenant = format!("proxy-failover-{unique}");
    let model = format!("proxy-failover-model-{unique}");
    let pepper = b"postgres proxy failover pepper value";
    let mut accounts = Vec::new();
    let mut routes = Vec::new();
    for index in 0..2 {
        let account = database
            .create_upstream_account(
                CreateUpstreamAccountInput {
                    tenant_external_id: tenant.clone(),
                    name: format!("proxy-failover-{index}"),
                    driver: "http-json".to_owned(),
                    config: serde_json::json!({"base_url": format!("https://upstream-{index}.example.test")}),
                    credential: UpstreamCredential::None,
                    oauth_session_id: None,
                    oauth_driver: None,
                    oauth_refresh_url: None,
                },
                pepper,
            )
            .await
            .unwrap();
        let route = database
            .create_model_route(CreateModelRouteInput {
                tenant_external_id: tenant.clone(),
                public_model: model.clone(),
                upstream_account_id: account.id,
                upstream_model: model.clone(),
                protocol: "openai".to_owned(),
                priority: index,
            })
            .await
            .unwrap();
        accounts.push(account.id);
        routes.push(route.id);
    }
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: tenant,
                principal_external_id: "member".to_owned(),
                alias: "proxy-failover".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy::default(),
                initial_balance: Decimal::TEN,
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
        .upsert_model_price(&model, "USD", Decimal::ONE, Decimal::ONE)
        .await
        .unwrap();
    let request_id = Uuid::now_v7();
    let reservation = database
        .start_proxy_request(StartProxyRequest {
            request_id,
            key: &key,
            price: &price,
            input_token_ceiling: 1,
            output_token_ceiling: 1,
            protocol: "openai",
            model: &model,
            request_object: "gap://postgres-failover/request",
            upstream_account_id: Some(accounts[0]),
            model_route_id: Some(routes[0]),
        })
        .await
        .unwrap();
    for _ in 0..2 {
        database
            .reassign_pending_proxy_upstream(
                request_id,
                key.tenant_id,
                reservation.id,
                (accounts[0], routes[0]),
                (accounts[1], routes[1]),
            )
            .await
            .unwrap();
    }
    assert!(
        database
            .reassign_pending_proxy_upstream(
                request_id,
                Uuid::now_v7(),
                reservation.id,
                (accounts[1], routes[1]),
                (accounts[0], routes[0]),
            )
            .await
            .is_err()
    );
    let inspection = PgPool::connect(&database_url).await.unwrap();
    let row: (String, String) = sqlx::query_as(
        "SELECT upstream_account_id, model_route_id FROM request_records WHERE id = $1",
    )
    .bind(request_id.to_string())
    .fetch_one(&inspection)
    .await
    .unwrap();
    assert_eq!(row, (accounts[1].to_string(), routes[1].to_string()));
}

#[tokio::test]
async fn postgres_proxy_terminal_owner_is_exactly_once() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let _postgres_test_guard = POSTGRES_TEST_SERIAL.lock().await;
    let database = Database::connect_with_max(&database_url, 16).await.unwrap();
    database.migrate().await.unwrap();
    let unique = Uuid::now_v7();
    let model = format!("proxy-atomic-{unique}");
    let pepper = b"postgres proxy atomic pepper value";
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: format!("proxy-atomic-{unique}"),
                principal_external_id: "member".to_owned(),
                alias: "proxy-atomic".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy {
                    tokens_per_minute: 100_000,
                    max_concurrency: 16,
                    ..KeyPolicy::default()
                },
                initial_balance: Decimal::from(10),
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
        .upsert_model_price(&model, "USD", Decimal::ONE, Decimal::ONE)
        .await
        .unwrap();
    let request_id = Uuid::now_v7();
    let reservation = database
        .start_proxy_request(StartProxyRequest {
            request_id,
            key: &key,
            price: &price,
            input_token_ceiling: 100,
            output_token_ceiling: 100,
            protocol: "openai",
            model: &model,
            request_object: "objects/blake3/postgres-proxy-atomic",
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap();
    let tenant_id = key.tenant_id;
    let key_id = key.key_id;

    let fault_attempt_id = Uuid::now_v7();
    let fault_lease = match database
        .begin_archive_staging_attempt(BeginArchiveStagingInput {
            key: ArchiveStagingKey::new(
                ArchiveStagingOwner::ProxyRequest(request_id),
                ArchiveStagingPurpose::Response,
                fault_attempt_id,
            )
            .unwrap(),
            intent_digest: ArchiveStagingIntentDigest::new(format!(
                "{:064x}",
                fault_attempt_id.as_u128()
            ))
            .unwrap(),
            lease_token: Uuid::now_v7(),
            lease_owner: ArchiveStagingLeaseOwner::new("postgres-proxy-fault").unwrap(),
        })
        .await
        .unwrap()
    {
        BeginArchiveStagingResult::Created(lease) => lease,
        result => panic!("unexpected staging begin: {result:?}"),
    };
    let fault_locator = format!("{}/body", fault_lease.key.canonical_prefix());
    let inspection = PgPool::connect(&database_url).await.unwrap();
    let suffix = request_id.simple();
    let function_name = format!("proxy_bind_fault_{suffix}");
    let trigger_name = format!("proxy_bind_fault_trigger_{suffix}");
    // Test-only SQL safety boundary for the DDL below: every identifier and predicate value is
    // derived from the typed `request_id` UUID rendered as hexadecimal; no external input enters.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE FUNCTION {function_name}() RETURNS trigger LANGUAGE plpgsql AS $function$ BEGIN RAISE EXCEPTION 'proxy bind fault'; END; $function$"
    )))
    .execute(&inspection)
    .await
    .unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE TRIGGER {trigger_name} BEFORE UPDATE OF response_object ON request_records FOR EACH ROW WHEN (NEW.id = '{request_id}') EXECUTE FUNCTION {function_name}()"
    )))
    .execute(&inspection)
    .await
    .unwrap();
    assert!(
        database
            .finish_proxy_request_with_archive_staging(
                FinishProxyRequest {
                    request_id,
                    tenant_id,
                    reservation: &reservation,
                    input_token_ceiling: 100,
                    output_token_ceiling: 100,
                    requested_service_tier: None,
                    status_code: 200,
                    duration_ms: 1,
                    usage: TokenUsage {
                        input_tokens: 11,
                        output_tokens: 7,
                        ..TokenUsage::default()
                    },
                    charge_contract_ceiling: false,
                    error_code: None,
                    response_object: &fault_locator,
                    conversation: None,
                },
                Some(&fault_lease),
            )
            .await
            .is_err()
    );
    assert_eq!(
        database
            .archive_staging_attempt(fault_attempt_id)
            .await
            .unwrap()
            .unwrap()
            .state,
        ArchiveStagingState::Writing,
        "PostgreSQL must roll the staging bind back with the terminal locator"
    );
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP TRIGGER {trigger_name} ON request_records"
    )))
    .execute(&inspection)
    .await
    .unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP FUNCTION {function_name}()"
    )))
    .execute(&inspection)
    .await
    .unwrap();
    assert!(
        database
            .abandon_archive_staging_attempt(&fault_lease)
            .await
            .unwrap()
    );

    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(8));
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let database = database.clone();
        let reservation = reservation.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            let attempt_id = Uuid::now_v7();
            let lease = match database
                .begin_archive_staging_attempt(BeginArchiveStagingInput {
                    key: ArchiveStagingKey::new(
                        ArchiveStagingOwner::ProxyRequest(request_id),
                        ArchiveStagingPurpose::Response,
                        attempt_id,
                    )
                    .unwrap(),
                    intent_digest: ArchiveStagingIntentDigest::new(format!(
                        "{:064x}",
                        attempt_id.as_u128()
                    ))
                    .unwrap(),
                    lease_token: Uuid::now_v7(),
                    lease_owner: ArchiveStagingLeaseOwner::new("postgres-proxy-writer").unwrap(),
                })
                .await
                .unwrap()
            {
                BeginArchiveStagingResult::Created(lease) => lease,
                result => panic!("unexpected staging begin: {result:?}"),
            };
            let response_object = format!("{}/body", lease.key.canonical_prefix());
            barrier.wait().await;
            let result = database
                .finish_proxy_request_with_archive_staging(
                    FinishProxyRequest {
                        request_id,
                        tenant_id,
                        reservation: &reservation,
                        input_token_ceiling: 100,
                        output_token_ceiling: 100,
                        requested_service_tier: None,
                        status_code: 200,
                        duration_ms: 1,
                        usage: TokenUsage {
                            input_tokens: 11,
                            output_tokens: 7,
                            ..TokenUsage::default()
                        },
                        charge_contract_ceiling: false,
                        error_code: None,
                        response_object: &response_object,
                        conversation: None,
                    },
                    Some(&lease),
                )
                .await;
            if matches!(
                &result,
                Ok(FinishProxyRequestResult::AlreadyFinished {
                    response_object: winner,
                    ..
                }) if winner != &response_object
            ) {
                assert!(
                    database
                        .abandon_archive_staging_attempt(&lease)
                        .await
                        .unwrap()
                );
            }
            (result, attempt_id)
        }));
    }
    let mut winners = 0;
    let mut replays = 0;
    let mut bound_attempts = 0;
    let mut cleanup_attempts = 0;
    for task in tasks {
        let (result, attempt_id) = task.await.unwrap();
        match result.unwrap() {
            FinishProxyRequestResult::Finished { .. } => winners += 1,
            FinishProxyRequestResult::AlreadyFinished { .. } => replays += 1,
        }
        let state = database
            .archive_staging_attempt(attempt_id)
            .await
            .unwrap()
            .unwrap()
            .state;
        match state {
            ArchiveStagingState::Bound => bound_attempts += 1,
            ArchiveStagingState::CleanupPending => cleanup_attempts += 1,
            state => panic!("unexpected terminal staging state: {state:?}"),
        }
    }
    assert_eq!(winners, 1);
    assert_eq!(replays, 7);
    assert_eq!(bound_attempts, 1);
    assert_eq!(cleanup_attempts, 7);
    assert_eq!(
        database
            .list_account_ledger(issued.account_id, 100)
            .await
            .unwrap()
            .into_iter()
            .filter(|entry| entry.source == reservation.id.to_string())
            .count(),
        1
    );
    let stats = database
        .stats_filtered(
            key_id,
            StatsFilter {
                from_created_at: Some(unix_millis().saturating_sub(60_000)),
                to_created_at: Some(unix_millis().saturating_add(1)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(stats.summary.total_requests, 1);
    assert_eq!(stats.summary.successful_requests, 1);
    assert_eq!(stats.summary.input_tokens, 11);
    assert_eq!(stats.summary.output_tokens, 7);
}

#[tokio::test]
async fn postgres_synchronous_image_terminal_ack_recovery_is_exactly_once_without_idempotency() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let _postgres_test_guard = POSTGRES_TEST_SERIAL.lock().await;
    let database = Database::connect_with_max(&database_url, 16).await.unwrap();
    database.migrate().await.unwrap();
    let unique = Uuid::now_v7();
    let model = format!("image-atomic-{unique}");
    let pepper = b"postgres synchronous image atomic pepper";
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: format!("image-atomic-{unique}"),
                principal_external_id: "member".to_owned(),
                alias: "image-atomic".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy {
                    tokens_per_minute: 100_000,
                    max_concurrency: 16,
                    ..KeyPolicy::default()
                },
                initial_balance: Decimal::from(10),
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
        .upsert_model_price(&model, "USD", Decimal::ZERO, Decimal::ONE)
        .await
        .unwrap();
    let request_id = Uuid::now_v7();
    let reservation = match database
        .start_synchronous_image_request(StartSynchronousImageRequest {
            request_id,
            key: &key,
            price: &price,
            input_token_ceiling: 0,
            output_token_ceiling: 1,
            idempotency: None,
            protocol: "openai-image",
            model: &model,
            request_object: "staging://blake3/postgres-image-request",
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap()
    {
        StartSynchronousImageResult::Started(reservation) => reservation,
        replay => panic!("non-idempotent image request cannot replay at start: {replay:?}"),
    };
    let asset = ArchivedGenerationAsset {
        asset_id: Uuid::now_v7(),
        index: 0,
        object_locator: format!("staging/synchronous/{request_id}/asset-0"),
        mime_type: "image/png".to_owned(),
        size_bytes: 3,
        filename: "asset-0.png".to_owned(),
    };
    let response_object = format!("staging/synchronous/{request_id}/response.json");
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(8));
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let database = database.clone();
        let reservation = reservation.clone();
        let asset = asset.clone();
        let response_object = response_object.clone();
        let barrier = barrier.clone();
        let key_id = key.key_id;
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            database
                .finish_synchronous_image_request(FinishSynchronousImageRequest {
                    key_id,
                    idempotency_key: None,
                    request_id,
                    reservation: &reservation,
                    status_code: 200,
                    duration_ms: 1,
                    input_tokens: 0,
                    output_tokens: 1,
                    error_code: None,
                    response_object: &response_object,
                    assets: std::slice::from_ref(&asset),
                })
                .await
        }));
    }
    let mut winners = 0;
    let mut recovered_acks = 0;
    for task in tasks {
        match task.await.unwrap().unwrap() {
            FinishSynchronousImageResult::Finished { cost_micros: 1 } => winners += 1,
            FinishSynchronousImageResult::Replay(
                memeloop_token_center::db::SynchronousImageIdempotencyClaim::Completed {
                    request_id: replay_request_id,
                    response_status: 200,
                    response_object: replay_object,
                },
            ) if replay_request_id == request_id && replay_object == response_object => {
                recovered_acks += 1;
            }
            result => panic!("unexpected synchronous image terminal result: {result:?}"),
        }
    }
    assert_eq!(winners, 1);
    assert_eq!(recovered_acks, 7);
    assert_eq!(
        database
            .list_account_ledger(issued.account_id, 100)
            .await
            .unwrap()
            .into_iter()
            .filter(|entry| entry.source == reservation.id.to_string())
            .count(),
        1
    );
    assert_eq!(
        database
            .synchronous_generation_assets(request_id)
            .await
            .unwrap()
            .len(),
        1
    );
    let stats = database
        .stats_filtered(
            key.key_id,
            StatsFilter {
                from_created_at: Some(unix_millis().saturating_sub(60_000)),
                to_created_at: Some(unix_millis().saturating_add(1)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(stats.summary.total_requests, 1);
    assert_eq!(stats.summary.successful_requests, 1);
    assert_eq!(stats.summary.output_tokens, 1);
}
