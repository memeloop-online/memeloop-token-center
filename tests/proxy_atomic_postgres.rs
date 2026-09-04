use memeloop_token_center::{
    archive_staging::{
        ArchiveStagingIntentDigest, ArchiveStagingKey, ArchiveStagingLeaseOwner,
        ArchiveStagingOwner, ArchiveStagingPurpose, ArchiveStagingState, BeginArchiveStagingInput,
        BeginArchiveStagingResult,
    },
    conversation::{ConversationHints, extract_atoms},
    db::{
        ConversationDetailFilter, ConversationListFilter, CreateKeyInput, CreateModelRouteInput,
        CreateUpstreamAccountInput, Database, FinishProxyRequest, FinishProxyRequestResult,
        FinishSynchronousImageRequest, FinishSynchronousImageResult, ProxyConversationInput,
        StartProxyRequest, StartSynchronousImageRequest, StartSynchronousImageResult, StatsFilter,
        unix_millis,
    },
    error::{AppError, LimitReason},
    model::{ArchivedGenerationAsset, EnforcementMode, KeyPolicy, TokenUsage},
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
async fn postgres_metered_unlimited_admits_and_settles_1024_same_key_requests_without_shared_budget_rows()
 {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let _postgres_test_guard = POSTGRES_TEST_SERIAL.lock().await;
    let database = Database::connect_with_max(&database_url, 64).await.unwrap();
    database.migrate().await.unwrap();
    let unique = Uuid::now_v7();
    let model = format!("metered-unlimited-1024-{unique}");
    let pepper = b"postgres metered unlimited 1024 pepper";
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: format!("metered-unlimited-1024-{unique}"),
                principal_external_id: "member".to_owned(),
                alias: "metered-unlimited-1024".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy {
                    requests_per_minute: 1,
                    tokens_per_minute: 1,
                    max_concurrency: 1,
                    enforcement_mode: EnforcementMode::MeteredUnlimited,
                    ..KeyPolicy::default()
                },
                initial_balance: Decimal::ZERO,
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
    assert_eq!(
        key.policy.enforcement_mode,
        EnforcementMode::MeteredUnlimited
    );
    let price = database
        .upsert_model_price(&model, "USD", Decimal::ONE, Decimal::ONE)
        .await
        .unwrap();

    const REQUESTS: usize = 1024;
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(REQUESTS));
    let mut admissions = Vec::with_capacity(REQUESTS);
    for index in 0..REQUESTS {
        let database = database.clone();
        let key = key.clone();
        let price = price.clone();
        let model = model.clone();
        let barrier = barrier.clone();
        admissions.push(tokio::spawn(async move {
            let request_id = Uuid::now_v7();
            let request_object = format!("objects/blake3/metered-unlimited-request-{index}");
            barrier.wait().await;
            let reservation = database
                .start_proxy_request(StartProxyRequest {
                    request_id,
                    key: &key,
                    price: &price,
                    input_token_ceiling: 1,
                    output_token_ceiling: 1,
                    protocol: "openai",
                    model: &model,
                    request_object: &request_object,
                    upstream_account_id: None,
                    model_route_id: None,
                })
                .await
                .unwrap();
            (request_id, reservation)
        }));
    }
    let mut admitted = Vec::with_capacity(REQUESTS);
    for admission in admissions {
        admitted.push(admission.await.unwrap());
    }

    let inspection = PgPool::connect(&database_url).await.unwrap();
    let admission_state: (i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT COUNT(*) FROM usage_reservations WHERE key_id = $1 AND status = 'reserved' AND enforcement_mode = 'metered_unlimited'),
            (SELECT COUNT(*) FROM rate_limit_windows WHERE key_id = $2),
            (SELECT COALESCE(reserved_micros, 0) FROM key_budget_state WHERE key_id = $3),
            (SELECT available_micros FROM credit_accounts WHERE id = $4),
            (SELECT reserved_micros FROM credit_accounts WHERE id = $5)",
    )
    .bind(key.key_id.to_string())
    .bind(key.key_id.to_string())
    .bind(key.key_id.to_string())
    .bind(key.account_id.to_string())
    .bind(key.account_id.to_string())
    .fetch_one(&inspection)
    .await
    .unwrap();
    assert_eq!(admission_state, (REQUESTS as i64, 0, 0, 0, 0));

    let finish_barrier = std::sync::Arc::new(tokio::sync::Barrier::new(REQUESTS));
    let mut finishes = Vec::with_capacity(REQUESTS);
    for (request_id, reservation) in admitted {
        let database = database.clone();
        let finish_barrier = finish_barrier.clone();
        let tenant_id = key.tenant_id;
        finishes.push(tokio::spawn(async move {
            finish_barrier.wait().await;
            database
                .finish_proxy_request(FinishProxyRequest {
                    request_id,
                    tenant_id,
                    reservation: &reservation,
                    input_token_ceiling: 1,
                    output_token_ceiling: 1,
                    requested_service_tier: None,
                    status_code: 200,
                    duration_ms: 1,
                    usage: TokenUsage {
                        input_tokens: 1,
                        output_tokens: 1,
                        ..TokenUsage::default()
                    },
                    charge_contract_ceiling: false,
                    error_code: None,
                    response_object: "objects/blake3/metered-unlimited-response",
                    conversation: None,
                })
                .await
                .unwrap()
        }));
    }
    for finish in finishes {
        assert_eq!(
            finish.await.unwrap(),
            FinishProxyRequestResult::Finished {
                cost_micros: 2,
                usage_invalid: false,
            }
        );
    }

    let terminal_state: (i64, i64, i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT COUNT(*) FROM usage_reservations WHERE key_id = $1 AND status = 'settled' AND actual_micros = 2),
            (SELECT COUNT(*) FROM ledger_entries WHERE key_id = $2 AND kind = 'usage'),
            (SELECT COALESCE(SUM(amount_micros), 0)::BIGINT FROM ledger_entries WHERE key_id = $3 AND kind = 'usage'),
            (SELECT COUNT(*) FROM metered_usage_projection_outbox WHERE key_id = $4 AND actual_micros = 2 AND projected_at IS NULL),
            (SELECT COUNT(*) FROM request_stats_facts WHERE key_id = $5 AND cost_micros = 2),
            (SELECT COUNT(*) FROM rate_limit_windows WHERE key_id = $6),
            (SELECT COALESCE(reserved_micros, 0) FROM key_budget_state WHERE key_id = $7)",
    )
    .bind(key.key_id.to_string())
    .bind(key.key_id.to_string())
    .bind(key.key_id.to_string())
    .bind(key.key_id.to_string())
    .bind(key.key_id.to_string())
    .bind(key.key_id.to_string())
    .bind(key.key_id.to_string())
    .fetch_one(&inspection)
    .await
    .unwrap();
    assert_eq!(
        terminal_state,
        (
            REQUESTS as i64,
            REQUESTS as i64,
            -(REQUESTS as i64 * 2),
            REQUESTS as i64,
            REQUESTS as i64,
            0,
            0,
        )
    );

    let projector = Uuid::now_v7();
    let mut projected = 0_usize;
    loop {
        let tasks = database
            .claim_metered_usage_projection_tasks(projector, 32)
            .await
            .unwrap();
        if tasks.is_empty() {
            break;
        }
        for task in tasks {
            assert!(
                database
                    .project_claimed_metered_usage_projection_task(projector, task.reservation_id)
                    .await
                    .unwrap()
            );
            projected += 1;
        }
    }
    // The projector is intentionally global rather than key-scoped. Other
    // serial PostgreSQL acceptance cases can leave ready work in the same
    // database, so this worker may safely drain more than this test created.
    // The key-scoped assertions below prove that all of this test's rows were
    // projected exactly once.
    assert!(projected >= REQUESTS);
    let projection_state: (i64, i64, i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT COUNT(*) FROM metered_usage_projection_outbox WHERE key_id = $1 AND projected_at IS NOT NULL),
            (SELECT COALESCE(SUM(requests), 0)::BIGINT FROM usage_daily_aggregates WHERE key_id = $2),
            (SELECT COALESCE(SUM(requests), 0)::BIGINT FROM request_daily_aggregates WHERE key_id = $3),
            (SELECT COALESCE(SUM(requests), 0)::BIGINT FROM usage_analysis_hourly WHERE key_id = $4 AND source_kind = 'request'),
            (SELECT COALESCE(SUM(requests), 0)::BIGINT FROM usage_analysis_daily WHERE key_id = $5 AND source_kind = 'request'),
            (SELECT COALESCE(SUM(requests), 0)::BIGINT FROM session_usage_totals WHERE key_id = $6),
            (SELECT COALESCE(settled_lifetime_micros, 0) FROM key_budget_state WHERE key_id = $7)",
    )
    .bind(key.key_id.to_string())
    .bind(key.key_id.to_string())
    .bind(key.key_id.to_string())
    .bind(key.key_id.to_string())
    .bind(key.key_id.to_string())
    .bind(key.key_id.to_string())
    .bind(key.key_id.to_string())
    .fetch_one(&inspection)
    .await
    .unwrap();
    assert_eq!(
        projection_state,
        (
            REQUESTS as i64,
            REQUESTS as i64,
            REQUESTS as i64,
            REQUESTS as i64,
            REQUESTS as i64,
            REQUESTS as i64,
            0,
        )
    );
}

#[tokio::test]
async fn postgres_metered_unlimited_terminal_projection_keeps_1024_same_session_requests_off_cluster_hotspots()
 {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let _postgres_test_guard = POSTGRES_TEST_SERIAL.lock().await;
    let database = Database::connect_with_max(&database_url, 96).await.unwrap();
    database.migrate().await.unwrap();
    let unique = Uuid::now_v7();
    let tenant = format!("metered-conversation-1024-{unique}");
    let model = format!("metered-conversation-1024-{unique}");
    let pepper = b"postgres metered conversation projection pepper";
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: tenant,
                principal_external_id: "member".to_owned(),
                alias: "metered-conversation-1024".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy {
                    enforcement_mode: EnforcementMode::MeteredUnlimited,
                    ..KeyPolicy::default()
                },
                initial_balance: Decimal::ZERO,
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
    let session_id = format!("metered-terminal-session-{unique}");

    // First project one root turn. Every concurrent child below names it as
    // its parent, so the final query verifies both session membership and the
    // durable parent-child semantics rather than merely a shared label.
    let root_request_id = Uuid::now_v7();
    let root_reservation = database
        .start_proxy_request(StartProxyRequest {
            request_id: root_request_id,
            key: &key,
            price: &price,
            input_token_ceiling: 1,
            output_token_ceiling: 1,
            protocol: "openai",
            model: &model,
            request_object: "objects/blake3/metered-conversation-root-request",
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap();
    let root_request_json = serde_json::json!({"input": [{"role": "user", "content": "root"}]});
    let root_hints = ConversationHints {
        session_id: Some(session_id.clone()),
        turn_id: Some("root-turn".to_owned()),
        ..ConversationHints::default()
    };
    database
        .finish_proxy_request(FinishProxyRequest {
            request_id: root_request_id,
            tenant_id: key.tenant_id,
            reservation: &root_reservation,
            input_token_ceiling: 1,
            output_token_ceiling: 1,
            requested_service_tier: None,
            status_code: 200,
            duration_ms: 1,
            usage: TokenUsage {
                input_tokens: 1,
                output_tokens: 1,
                ..TokenUsage::default()
            },
            charge_contract_ceiling: false,
            error_code: None,
            response_object: "objects/blake3/metered-conversation-root-response",
            conversation: Some(ProxyConversationInput {
                key: &key,
                request_json: &root_request_json,
                hints: &root_hints,
                client_name: Some("codex"),
                upstream_response_id: Some("root-response"),
            }),
        })
        .await
        .unwrap();
    let projector = Uuid::now_v7();
    let root_tasks = database
        .claim_conversation_projection_tasks(projector, 1)
        .await
        .unwrap();
    assert_eq!(root_tasks.len(), 1);
    assert_eq!(root_tasks[0].request_id, root_request_id);
    assert!(
        database
            .project_claimed_conversation_projection_task(projector, root_request_id)
            .await
            .unwrap()
    );

    const REQUESTS: usize = 1024;
    let admission_barrier = std::sync::Arc::new(tokio::sync::Barrier::new(REQUESTS));
    let mut admissions = Vec::with_capacity(REQUESTS);
    for index in 0..REQUESTS {
        let database = database.clone();
        let key = key.clone();
        let price = price.clone();
        let model = model.clone();
        let admission_barrier = admission_barrier.clone();
        admissions.push(tokio::spawn(async move {
            let request_id = Uuid::now_v7();
            let request_object =
                format!("objects/blake3/metered-conversation-child-request-{index}");
            admission_barrier.wait().await;
            let reservation = database
                .start_proxy_request(StartProxyRequest {
                    request_id,
                    key: &key,
                    price: &price,
                    input_token_ceiling: 1,
                    output_token_ceiling: 1,
                    protocol: "openai",
                    model: &model,
                    request_object: &request_object,
                    upstream_account_id: None,
                    model_route_id: None,
                })
                .await
                .unwrap();
            (index, request_id, reservation)
        }));
    }
    let mut admitted = Vec::with_capacity(REQUESTS);
    for admission in admissions {
        admitted.push(admission.await.unwrap());
    }

    let finish_barrier = std::sync::Arc::new(tokio::sync::Barrier::new(REQUESTS));
    let mut finishes = Vec::with_capacity(REQUESTS);
    for (index, request_id, reservation) in admitted {
        let database = database.clone();
        let key = key.clone();
        let finish_barrier = finish_barrier.clone();
        let session_id = session_id.clone();
        finishes.push(tokio::spawn(async move {
            let request_json = serde_json::json!({
                "input": [{"role": "user", "content": format!("child-{index}")}]
            });
            let hints = ConversationHints {
                session_id: Some(session_id),
                turn_id: Some(format!("child-turn-{index}")),
                parent_turn_id: Some("root-turn".to_owned()),
                ..ConversationHints::default()
            };
            finish_barrier.wait().await;
            database
                .finish_proxy_request(FinishProxyRequest {
                    request_id,
                    tenant_id: key.tenant_id,
                    reservation: &reservation,
                    input_token_ceiling: 1,
                    output_token_ceiling: 1,
                    requested_service_tier: None,
                    status_code: 200,
                    duration_ms: 1,
                    usage: TokenUsage {
                        input_tokens: 1,
                        output_tokens: 1,
                        ..TokenUsage::default()
                    },
                    charge_contract_ceiling: false,
                    error_code: None,
                    response_object: "objects/blake3/metered-conversation-child-response",
                    conversation: Some(ProxyConversationInput {
                        key: &key,
                        request_json: &request_json,
                        hints: &hints,
                        client_name: Some("codex"),
                        upstream_response_id: None,
                    }),
                })
                .await
                .unwrap()
        }));
    }
    for finish in finishes {
        assert!(matches!(
            finish.await.unwrap(),
            FinishProxyRequestResult::Finished {
                cost_micros: 2,
                usage_invalid: false,
            }
        ));
    }

    let inspection = PgPool::connect(&database_url).await.unwrap();
    let terminal_only: (i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT COUNT(*) FROM conversation_projection_outbox WHERE key_id = $1 AND projected_at IS NULL),
            (SELECT COUNT(*) FROM conversation_observations WHERE key_id = $2),
            (SELECT COALESCE(SUM(request_count), 0)::BIGINT FROM conversation_key_clusters WHERE key_id = $3)",
    )
    .bind(key.key_id.to_string())
    .bind(key.key_id.to_string())
    .bind(key.key_id.to_string())
    .fetch_one(&inspection)
    .await
    .unwrap();
    assert_eq!(terminal_only, (REQUESTS as i64, 1, 1));

    let mut projected = 0_usize;
    loop {
        let tasks = database
            .claim_conversation_projection_tasks(projector, 32)
            .await
            .unwrap();
        if tasks.is_empty() {
            break;
        }
        for task in tasks {
            assert!(
                database
                    .project_claimed_conversation_projection_task(projector, task.request_id)
                    .await
                    .unwrap()
            );
            projected += 1;
        }
    }
    assert_eq!(projected, REQUESTS);

    let clusters = database
        .conversation_clusters(
            key.key_id,
            ConversationListFilter {
                limit: 10,
                before_updated_at: None,
                before_cluster_id: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(clusters.len(), 1);
    assert_eq!(
        clusters[0].explicit_session_id.as_deref(),
        Some(session_id.as_str())
    );
    assert_eq!(clusters[0].request_count, REQUESTS as i64 + 1);
    let detail = database
        .conversation_cluster_detail(
            key.key_id,
            clusters[0].cluster_id,
            ConversationDetailFilter {
                limit: 200,
                before_created_at: None,
                before_request_id: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(detail.cluster.request_count, REQUESTS as i64 + 1);
    assert_eq!(detail.requests.len(), 200);
    assert!(detail.has_more);

    let materialized: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT COUNT(*) FROM conversation_observations WHERE key_id = $1 AND cluster_id = $2),
            (SELECT COUNT(*) FROM conversation_edges WHERE cluster_id = $3 AND relation_kind = 'continues'),
            (SELECT COUNT(*) FROM request_records WHERE key_id = $4 AND conversation_cluster_id = $5),
            (SELECT COUNT(*) FROM conversation_projection_outbox WHERE key_id = $6 AND projected_at IS NULL)",
    )
    .bind(key.key_id.to_string())
    .bind(clusters[0].cluster_id.to_string())
    .bind(clusters[0].cluster_id.to_string())
    .bind(key.key_id.to_string())
    .bind(clusters[0].cluster_id.to_string())
    .bind(key.key_id.to_string())
    .fetch_one(&inspection)
    .await
    .unwrap();
    assert_eq!(
        materialized,
        (REQUESTS as i64 + 1, REQUESTS as i64, REQUESTS as i64 + 1, 0)
    );
}

#[tokio::test]
async fn postgres_metered_unlimited_terminal_replay_is_exactly_once() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let _postgres_test_guard = POSTGRES_TEST_SERIAL.lock().await;
    let database = Database::connect_with_max(&database_url, 32).await.unwrap();
    database.migrate().await.unwrap();
    let unique = Uuid::now_v7();
    let model = format!("metered-unlimited-replay-{unique}");
    let pepper = b"postgres metered unlimited replay pepper";
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: format!("metered-unlimited-replay-{unique}"),
                principal_external_id: "member".to_owned(),
                alias: "metered-unlimited-replay".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy {
                    enforcement_mode: EnforcementMode::MeteredUnlimited,
                    ..KeyPolicy::default()
                },
                initial_balance: Decimal::ZERO,
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
            request_object: "objects/blake3/metered-unlimited-replay-request",
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap();
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(64));
    let mut finishes = Vec::with_capacity(64);
    for _ in 0..64 {
        let database = database.clone();
        let reservation = reservation.clone();
        let barrier = barrier.clone();
        let tenant_id = key.tenant_id;
        finishes.push(tokio::spawn(async move {
            barrier.wait().await;
            database
                .finish_proxy_request(FinishProxyRequest {
                    request_id,
                    tenant_id,
                    reservation: &reservation,
                    input_token_ceiling: 1,
                    output_token_ceiling: 1,
                    requested_service_tier: None,
                    status_code: 200,
                    duration_ms: 1,
                    usage: TokenUsage {
                        input_tokens: 1,
                        output_tokens: 1,
                        ..TokenUsage::default()
                    },
                    charge_contract_ceiling: false,
                    error_code: None,
                    response_object: "objects/blake3/metered-unlimited-replay-response",
                    conversation: None,
                })
                .await
                .unwrap()
        }));
    }
    let mut winners = 0;
    let mut replays = 0;
    for finish in finishes {
        match finish.await.unwrap() {
            FinishProxyRequestResult::Finished {
                cost_micros: 2,
                usage_invalid: false,
            } => winners += 1,
            FinishProxyRequestResult::AlreadyFinished { cost_micros: 2, .. } => replays += 1,
            result => panic!("unexpected metered replay result: {result:?}"),
        }
    }
    assert_eq!((winners, replays), (1, 63));
    let inspection = PgPool::connect(&database_url).await.unwrap();
    let exact_once: (i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT COUNT(*) FROM ledger_entries WHERE source = $1),
            (SELECT COUNT(*) FROM metered_usage_projection_outbox WHERE reservation_id = $2),
            (SELECT COUNT(*) FROM request_stats_facts WHERE request_id = $3)",
    )
    .bind(reservation.id.to_string())
    .bind(reservation.id.to_string())
    .bind(request_id.to_string())
    .fetch_one(&inspection)
    .await
    .unwrap();
    assert_eq!(exact_once, (1, 1, 1));
}

#[tokio::test]
async fn postgres_prepaid_boundary_remains_fail_closed_under_parallel_admission() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let _postgres_test_guard = POSTGRES_TEST_SERIAL.lock().await;
    let database = Database::connect_with_max(&database_url, 16).await.unwrap();
    database.migrate().await.unwrap();
    let unique = Uuid::now_v7();
    let model = format!("prepaid-boundary-{unique}");
    let pepper = b"postgres prepaid boundary pepper";
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: format!("prepaid-boundary-{unique}"),
                principal_external_id: "member".to_owned(),
                alias: "prepaid-boundary".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy {
                    requests_per_minute: 100,
                    tokens_per_minute: 100,
                    max_concurrency: 8,
                    enforcement_mode: EnforcementMode::Prepaid,
                    ..KeyPolicy::default()
                },
                initial_balance: Decimal::new(4, 6),
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
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(8));
    let mut admissions = Vec::with_capacity(8);
    for index in 0..8 {
        let database = database.clone();
        let key = key.clone();
        let price = price.clone();
        let model = model.clone();
        let barrier = barrier.clone();
        admissions.push(tokio::spawn(async move {
            let request_id = Uuid::now_v7();
            let request_object = format!("objects/blake3/prepaid-boundary-request-{index}");
            barrier.wait().await;
            (
                request_id,
                database
                    .start_proxy_request(StartProxyRequest {
                        request_id,
                        key: &key,
                        price: &price,
                        input_token_ceiling: 1,
                        output_token_ceiling: 1,
                        protocol: "openai",
                        model: &model,
                        request_object: &request_object,
                        upstream_account_id: None,
                        model_route_id: None,
                    })
                    .await,
            )
        }));
    }
    let mut admitted = Vec::new();
    let mut rejected = 0;
    for admission in admissions {
        let (request_id, result) = admission.await.unwrap();
        match result {
            Ok(reservation) => admitted.push((request_id, reservation)),
            Err(AppError::LimitExceeded {
                reason: LimitReason::BalanceExhausted,
                ..
            }) => rejected += 1,
            Err(error) => panic!("finite prepaid admission failed open or ambiguously: {error:?}"),
        }
    }
    assert_eq!((admitted.len(), rejected), (2, 6));
    for (request_id, reservation) in admitted {
        assert_eq!(
            database
                .finish_proxy_request(FinishProxyRequest {
                    request_id,
                    tenant_id: key.tenant_id,
                    reservation: &reservation,
                    input_token_ceiling: 1,
                    output_token_ceiling: 1,
                    requested_service_tier: None,
                    status_code: 200,
                    duration_ms: 1,
                    usage: TokenUsage {
                        input_tokens: 1,
                        output_tokens: 1,
                        ..TokenUsage::default()
                    },
                    charge_contract_ceiling: false,
                    error_code: None,
                    response_object: "objects/blake3/prepaid-boundary-response",
                    conversation: None,
                })
                .await
                .unwrap(),
            FinishProxyRequestResult::Finished {
                cost_micros: 2,
                usage_invalid: false,
            }
        );
    }
    let inspection = PgPool::connect(&database_url).await.unwrap();
    let terminal_state: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT available_micros FROM credit_accounts WHERE id = $1),
            (SELECT reserved_micros FROM credit_accounts WHERE id = $2),
            (SELECT settled_lifetime_micros FROM key_budget_state WHERE key_id = $3),
            (SELECT COALESCE(-SUM(amount_micros), 0)::BIGINT FROM ledger_entries WHERE key_id = $4 AND kind = 'usage')",
    )
    .bind(key.account_id.to_string())
    .bind(key.account_id.to_string())
    .bind(key.key_id.to_string())
    .bind(key.key_id.to_string())
    .fetch_one(&inspection)
    .await
    .unwrap();
    assert_eq!(terminal_state, (0, 0, 4, 4));
}

#[tokio::test]
async fn postgres_proxy_conversation_finish_lock_does_not_block_same_key_admission() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let _postgres_test_guard = POSTGRES_TEST_SERIAL.lock().await;
    let database = Database::connect_with_max(&database_url, 8).await.unwrap();
    database.migrate().await.unwrap();
    let inspection = PgPool::connect(&database_url).await.unwrap();
    let unique = Uuid::now_v7();
    let model = format!("proxy-admission-lock-{unique}");
    let pepper = b"postgres proxy admission lock pepper";
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: format!("proxy-admission-lock-{unique}"),
                principal_external_id: "member".to_owned(),
                alias: "proxy-admission-lock".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy {
                    requests_per_minute: 100_000,
                    tokens_per_minute: 100_000,
                    max_concurrency: 8,
                    ..KeyPolicy::default()
                },
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

    let request_a = Uuid::now_v7();
    let reservation_a = database
        .start_proxy_request(StartProxyRequest {
            request_id: request_a,
            key: &key,
            price: &price,
            input_token_ceiling: 100,
            output_token_ceiling: 100,
            protocol: "openai",
            model: &model,
            request_object: "objects/blake3/postgres-admission-lock-request-a",
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap();
    // Cross both PostgreSQL/SQLite-safe insert batch boundaries while keeping
    // every semantic atom unique to this test tenant.
    let messages = (0..151)
        .map(|index| {
            serde_json::json!({
                "role": "user",
                "content": format!("admission-lock-{unique}-{index}"),
            })
        })
        .collect::<Vec<_>>();
    let request_json = serde_json::json!({"model": model, "messages": messages});
    let target_atom = extract_atoms(&request_json)
        .into_iter()
        .next()
        .expect("the request must materialize one semantic atom");
    sqlx::query(
        "INSERT INTO semantic_atoms (tenant_id, content_hash, instance_hash, role, kind, content_json, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(key.tenant_id.to_string())
    .bind(&target_atom.content_hash)
    .bind(&target_atom.instance_hash)
    .bind(&target_atom.role)
    .bind(&target_atom.kind)
    .bind(serde_json::to_string(&target_atom.content).unwrap())
    .bind(unix_millis())
    .execute(&inspection)
    .await
    .unwrap();

    // Deleting the conflicting unique-key row without committing makes PostgreSQL's
    // `INSERT .. ON CONFLICT DO NOTHING` wait for this transaction's outcome.
    let mut blocker = inspection.begin().await.unwrap();
    let blocker_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *blocker)
        .await
        .unwrap();
    let deleted =
        sqlx::query("DELETE FROM semantic_atoms WHERE tenant_id = $1 AND content_hash = $2")
            .bind(key.tenant_id.to_string())
            .bind(&target_atom.content_hash)
            .execute(&mut *blocker)
            .await
            .unwrap();
    assert_eq!(deleted.rows_affected(), 1);

    let finish_a_database = database.clone();
    let finish_a_key = key.clone();
    let finish_a_reservation = reservation_a.clone();
    let finish_a_request_json = request_json.clone();
    let mut finish_a = tokio::spawn(async move {
        let hints = ConversationHints {
            session_id: Some(format!("admission-lock-{unique}")),
            ..ConversationHints::default()
        };
        finish_a_database
            .finish_proxy_request(FinishProxyRequest {
                request_id: request_a,
                tenant_id: finish_a_key.tenant_id,
                reservation: &finish_a_reservation,
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
                response_object: "objects/blake3/postgres-admission-lock-response-a",
                conversation: Some(ProxyConversationInput {
                    key: &finish_a_key,
                    request_json: &finish_a_request_json,
                    hints: &hints,
                    client_name: Some("codex"),
                    upstream_response_id: Some("resp-postgres-admission-lock-a"),
                }),
            })
            .await
    });

    let blocked_pid = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if let Some(pid) = sqlx::query_scalar::<_, i32>(
                "SELECT pid FROM pg_stat_activity WHERE datname = current_database() AND backend_type = 'client backend' AND state = 'active' AND wait_event_type = 'Lock' AND query LIKE 'INSERT INTO semantic_atoms%' AND $1 = ANY(pg_blocking_pids(pid)) ORDER BY query_start DESC LIMIT 1",
            )
            .bind(blocker_pid)
            .fetch_optional(&inspection)
            .await
            .unwrap()
            {
                break pid;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    let blocked_pid = match blocked_pid {
        Ok(pid) => pid,
        Err(_) => {
            blocker.rollback().await.unwrap();
            let finish_result =
                tokio::time::timeout(std::time::Duration::from_secs(30), &mut finish_a).await;
            if finish_result.is_err() {
                finish_a.abort();
                let _ = finish_a.await;
            }
            panic!("finish A never reached the semantic INSERT lock wait: {finish_result:?}");
        }
    };
    assert!(blocked_pid > 0);

    let request_b = Uuid::now_v7();
    // Keep the semantic-row blocker open for this entire admission gate. The two-second
    // wait-detection budget plus this five-second admission gate remains below the
    // production 10-second PostgreSQL lock_timeout: the old lock order cannot pass by
    // merely timing out its lock.
    let started_b = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        database.start_proxy_request(StartProxyRequest {
            request_id: request_b,
            key: &key,
            price: &price,
            input_token_ceiling: 100,
            output_token_ceiling: 100,
            protocol: "openai",
            model: &model,
            request_object: "objects/blake3/postgres-admission-lock-request-b",
            upstream_account_id: None,
            model_route_id: None,
        }),
    )
    .await;

    // Always release the blocker before interpreting B's result so a regression fails
    // promptly instead of leaving the spawned finish waiting on test teardown.
    blocker.rollback().await.unwrap();
    let finish_a_result =
        match tokio::time::timeout(std::time::Duration::from_secs(30), &mut finish_a).await {
            Ok(result) => result.unwrap().unwrap(),
            Err(_) => {
                finish_a.abort();
                let _ = finish_a.await;
                panic!("finish A must resume after the semantic row blocker is released");
            }
        };
    assert_eq!(
        finish_a_result,
        FinishProxyRequestResult::Finished {
            cost_micros: 18,
            usage_invalid: false,
        }
    );
    let materialized: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM semantic_atoms WHERE tenant_id = $1), (SELECT COUNT(*) FROM context_nodes WHERE tenant_id = $2)",
    )
    .bind(key.tenant_id.to_string())
    .bind(key.tenant_id.to_string())
    .fetch_one(&inspection)
    .await
    .unwrap();
    assert_eq!(materialized, (151, 151));
    let reservation_b = started_b
        .expect("same-key request B admission must complete within five seconds")
        .unwrap();

    let finish_b_result = database
        .finish_proxy_request(FinishProxyRequest {
            request_id: request_b,
            tenant_id: key.tenant_id,
            reservation: &reservation_b,
            input_token_ceiling: 100,
            output_token_ceiling: 100,
            requested_service_tier: None,
            status_code: 200,
            duration_ms: 1,
            usage: TokenUsage {
                input_tokens: 5,
                output_tokens: 3,
                ..TokenUsage::default()
            },
            charge_contract_ceiling: false,
            error_code: None,
            response_object: "objects/blake3/postgres-admission-lock-response-b",
            conversation: None,
        })
        .await
        .unwrap();
    assert_eq!(
        finish_b_result,
        FinishProxyRequestResult::Finished {
            cost_micros: 8,
            usage_invalid: false,
        }
    );

    for (reservation_id, expected_cost) in [(reservation_a.id, 18_i64), (reservation_b.id, 8)] {
        let reservation_row: (String, Option<i64>) =
            sqlx::query_as("SELECT status, actual_micros FROM usage_reservations WHERE id = $1")
                .bind(reservation_id.to_string())
                .fetch_one(&inspection)
                .await
                .unwrap();
        assert_eq!(reservation_row, ("settled".to_owned(), Some(expected_cost)));
        let ledger: (i64, Option<i64>) = sqlx::query_as(
            "SELECT COUNT(*), SUM(amount_micros)::BIGINT FROM ledger_entries WHERE source = $1",
        )
        .bind(reservation_id.to_string())
        .fetch_one(&inspection)
        .await
        .unwrap();
        assert_eq!(ledger, (1, Some(-expected_cost)));
    }
    let account: (i64, i64) = sqlx::query_as(
        "SELECT available_micros, reserved_micros FROM credit_accounts WHERE id = $1",
    )
    .bind(issued.account_id.to_string())
    .fetch_one(&inspection)
    .await
    .unwrap();
    assert_eq!(account, (10_000_000 - 26, 0));
    let budget: (i64, i64) = sqlx::query_as(
        "SELECT settled_lifetime_micros, reserved_micros FROM key_budget_state WHERE key_id = $1",
    )
    .bind(key.key_id.to_string())
    .fetch_one(&inspection)
    .await
    .unwrap();
    assert_eq!(budget, (26, 0));
    let active_reservations: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM usage_reservations WHERE key_id = $1 AND status = 'reserved'",
    )
    .bind(key.key_id.to_string())
    .fetch_one(&inspection)
    .await
    .unwrap();
    assert_eq!(active_reservations, 0);
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
