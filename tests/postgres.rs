use memeloop_token_center::{
    db::{
        CreateGenerationJobInput, CreateKeyInput, CreateModelRouteInput, CreateServiceTokenInput,
        CreateUpstreamAccountInput, Database, FinishGenerationJobInput, StatsFilter, unix_millis,
    },
    error::{AppError, LimitReason},
    model::{ArchivedGenerationAsset, GenerationStagedAssets, KeyPolicy},
    provider::UpstreamCredential,
};
use rust_decimal::Decimal;
use serde_json::json;
use uuid::Uuid;

#[tokio::test]
async fn postgres_budget_reservations_and_settlement_replays_are_serialized() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let database = Database::connect_with_max(&database_url, 16).await.unwrap();
    database.migrate().await.unwrap();
    let unique = Uuid::now_v7();
    let pepper = b"postgres budget pepper longer than thirty-two bytes";
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: format!("postgres-budget-{unique}"),
                principal_external_id: "member".to_owned(),
                alias: "budget-key".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy {
                    tokens_per_minute: 10_000,
                    daily_budget: Some("0.001".to_owned()),
                    weekly_budget: Some("0.001".to_owned()),
                    lifetime_budget: Some("0.001".to_owned()),
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
        .upsert_model_price("postgres-budget", "USD", Decimal::ZERO, Decimal::ONE)
        .await
        .unwrap();
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(8));
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let database = database.clone();
        let key = key.clone();
        let price = price.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            database.reserve_usage(&key, &price, 0, 600).await
        }));
    }
    let mut reservation = None;
    let mut rejected = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(value) => reservation = Some(value),
            Err(AppError::LimitExceeded { .. }) => rejected += 1,
            result => panic!("unexpected PostgreSQL budget result: {result:?}"),
        }
    }
    assert_eq!(rejected, 7);
    let reservation = reservation.unwrap();
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(8));
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let database = database.clone();
        let reservation = reservation.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            database.settle_usage(&reservation, 0, 700).await
        }));
    }
    for task in tasks {
        assert_eq!(task.await.unwrap().unwrap(), 700);
    }
    let usage_entries = database
        .list_account_ledger(issued.account_id, 100)
        .await
        .unwrap()
        .into_iter()
        .filter(|entry| entry.kind == "usage")
        .count();
    assert_eq!(usage_entries, 1);
}

#[tokio::test]
async fn postgres_long_running_reservations_enforce_authoritative_concurrency() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let database = Database::connect_with_max(&database_url, 16).await.unwrap();
    database.migrate().await.unwrap();
    let unique = Uuid::now_v7();
    let pepper = b"postgres concurrency pepper longer than thirty-two bytes";
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: format!("postgres-concurrency-{unique}"),
                principal_external_id: "member".to_owned(),
                alias: "long-task".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy {
                    requests_per_minute: 100,
                    tokens_per_minute: 100_000,
                    max_concurrency: 1,
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
        .upsert_model_price(
            &format!("postgres-concurrency-{unique}"),
            "USD",
            Decimal::ZERO,
            Decimal::ONE,
        )
        .await
        .unwrap();
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(8));
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let database = database.clone();
        let key = key.clone();
        let price = price.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            database.reserve_usage(&key, &price, 0, 100).await
        }));
    }
    let mut accepted = Vec::new();
    let mut rejected = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(reservation) => accepted.push(reservation),
            Err(AppError::LimitExceeded {
                reason: LimitReason::ConcurrencyExhausted,
                retry_after_seconds: Some(1),
            }) => rejected += 1,
            result => panic!("unexpected PostgreSQL concurrency result: {result:?}"),
        }
    }
    assert_eq!(accepted.len(), 1);
    assert_eq!(rejected, 7);
    let snapshot = database.key_limit_snapshot(issued.key_id).await.unwrap();
    assert_eq!(snapshot.concurrency.active, 1);
    assert_eq!(snapshot.concurrency.remaining, 0);

    database.settle_usage(&accepted[0], 0, 100).await.unwrap();
    assert_eq!(
        database
            .key_limit_snapshot(issued.key_id)
            .await
            .unwrap()
            .concurrency
            .active,
        0
    );
    let next = database.reserve_usage(&key, &price, 0, 100).await.unwrap();
    database.settle_usage(&next, 0, 100).await.unwrap();
}

#[tokio::test]
async fn postgres_credential_rotations_are_locked_and_idempotent() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let database = Database::connect_with_max(&database_url, 16).await.unwrap();
    database.migrate().await.unwrap();
    let unique = Uuid::now_v7();
    let pepper: &'static [u8] = b"postgres rotation pepper longer than thirty-two bytes";
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: format!("postgres-rotation-{unique}"),
                principal_external_id: "member".to_owned(),
                alias: "stable-key".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy::default(),
                initial_balance: Decimal::ONE,
                idempotency_key: None,
            },
            pepper,
        )
        .await
        .unwrap();
    let idempotency_key = format!("postgres:{unique}:same-key-rotation");
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(8));
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let database = database.clone();
        let barrier = barrier.clone();
        let idempotency_key = idempotency_key.clone();
        let key_id = issued.key_id;
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            database.rotate_key(key_id, &idempotency_key, pepper).await
        }));
    }
    let mut replays = Vec::new();
    for task in tasks {
        replays.push(task.await.unwrap().unwrap());
    }
    assert!(replays.iter().all(|value| {
        value.key_id == issued.key_id
            && value.account_id == issued.account_id
            && value.credential_generation == 2
            && value.key == replays[0].key
    }));

    let distinct_barrier = std::sync::Arc::new(tokio::sync::Barrier::new(4));
    let mut distinct_tasks = Vec::new();
    for index in 0..4 {
        let database = database.clone();
        let barrier = distinct_barrier.clone();
        let key_id = issued.key_id;
        let idempotency_key = format!("postgres:{unique}:distinct-key-rotation:{index}");
        distinct_tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            database.rotate_key(key_id, &idempotency_key, pepper).await
        }));
    }
    let mut generations = Vec::new();
    for task in distinct_tasks {
        generations.push(task.await.unwrap().unwrap().credential_generation);
    }
    generations.sort_unstable();
    assert_eq!(generations, vec![3, 4, 5, 6]);
    let replay_after_later_rotations = database
        .rotate_key(issued.key_id, &idempotency_key, pepper)
        .await
        .unwrap();
    assert_eq!(replay_after_later_rotations.credential_generation, 2);
    assert_eq!(replay_after_later_rotations.key, replays[0].key);

    let service = database
        .create_service_token(
            CreateServiceTokenInput {
                name: format!("postgres-rotation-service-{unique}"),
                scopes: vec!["keys:write".to_owned()],
                tenant_external_id: None,
            },
            pepper,
        )
        .await
        .unwrap();
    let service_idempotency_key = format!("postgres:{unique}:same-service-rotation");
    let service_barrier = std::sync::Arc::new(tokio::sync::Barrier::new(8));
    let mut service_tasks = Vec::new();
    for _ in 0..8 {
        let database = database.clone();
        let barrier = service_barrier.clone();
        let idempotency_key = service_idempotency_key.clone();
        let service_id = service.service_id;
        service_tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            database
                .rotate_service_token(service_id, &idempotency_key, pepper)
                .await
        }));
    }
    let mut service_replays = Vec::new();
    for task in service_tasks {
        service_replays.push(task.await.unwrap().unwrap());
    }
    assert!(service_replays.iter().all(|value| {
        value.service_id == service.service_id
            && value.credential_generation == 2
            && value.token == service_replays[0].token
    }));
    assert!(
        database
            .rotate_key(issued.key_id, &service_idempotency_key, pepper)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn postgres_oauth_refresh_has_one_account_generation_lease() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let database = Database::connect_with_max(&database_url, 16).await.unwrap();
    database.migrate().await.unwrap();
    let unique = Uuid::now_v7();
    let pepper: &'static [u8] = b"postgres OAuth lease pepper longer than thirty-two bytes";
    let account = database
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: format!("postgres-oauth-lease-{unique}"),
                name: "managed-oauth".into(),
                driver: "http-json".into(),
                config: json!({"base_url": "https://api.example.test"}),
                credential: UpstreamCredential::OAuth {
                    access_token: "postgres-access-v1".into(),
                    refresh_token: Some("postgres-refresh-v1".into()),
                    expires_at: Some(memeloop_token_center::db::unix_millis() + 60_000),
                    header: "authorization".into(),
                    prefix: "Bearer ".into(),
                    adapter_state: None,
                    proxy_url: None,
                    proxy_network_scope: None,
                },
                oauth_session_id: Some(Uuid::now_v7()),
                oauth_driver: Some("cursor".into()),
                oauth_refresh_url: Some("https://oauth.example.test/refresh".into()),
            },
            pepper,
        )
        .await
        .unwrap();
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(8));
    let mut tasks = Vec::new();
    for index in 0..8 {
        let database = database.clone();
        let barrier = barrier.clone();
        let key = format!("postgres-oauth-{unique}-{index}");
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            let result = database
                .begin_upstream_oauth_refresh(account.id, &key, pepper)
                .await;
            (key, result)
        }));
    }
    let mut winner = None;
    let mut conflicts = 0;
    for task in tasks {
        let (key, result) = task.await.unwrap();
        match result {
            Ok(None) => winner = Some(key),
            Err(AppError::Conflict(_)) => conflicts += 1,
            other => panic!("unexpected PostgreSQL OAuth lease result: {other:?}"),
        }
    }
    assert_eq!(conflicts, 7);
    let winner = winner.expect("one refresh lease winner");
    let refreshed = database
        .finish_upstream_oauth_refresh(
            account.id,
            UpstreamCredential::OAuth {
                access_token: "postgres-access-v2".into(),
                refresh_token: Some("postgres-refresh-v2".into()),
                expires_at: Some(memeloop_token_center::db::unix_millis() + 3_600_000),
                header: "authorization".into(),
                prefix: "Bearer ".into(),
                adapter_state: None,
                proxy_url: None,
                proxy_network_scope: None,
            },
            &winner,
            pepper,
        )
        .await
        .unwrap();
    assert_eq!(refreshed.id, account.id);
    assert_eq!(refreshed.credential_generation, 2);
    let replay = database
        .begin_upstream_oauth_refresh(account.id, &winner, pepper)
        .await
        .unwrap()
        .expect("PostgreSQL exact committed replay");
    assert_eq!(replay.id, account.id);
    assert_eq!(replay.credential_generation, 2);
}

#[derive(Clone, Copy)]
struct PostgresNativeCodexFixture {
    label: &'static str,
    schema: &'static str,
    proxy_url: &'static str,
    expected_proxy_url: &'static str,
}

async fn assert_postgres_native_codex_upgrade_fixture(
    database: &Database,
    fixture: PostgresNativeCodexFixture,
) {
    let unique = Uuid::now_v7();
    let key_material = b"postgres native Codex upgrade key material longer than thirty-two bytes";
    let tenant = format!("postgres-native-codex-{}-{unique}", fixture.label);
    let expires_at = unix_millis() + 3_600_000;
    let account = database
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: tenant.clone(),
                name: format!("Imported Codex {}", fixture.label),
                driver: "cpa-codex-oauth".to_owned(),
                config: json!({
                    "base_url": "https://chatgpt.com/backend-api/codex",
                    "network_scope": "public",
                    "reservation_token_bounds": {"gpt-5.6-sol": 128000}
                }),
                credential: UpstreamCredential::OAuth {
                    access_token: "postgres-native-access-secret".to_owned(),
                    refresh_token: Some("postgres-native-refresh-secret".to_owned()),
                    expires_at: Some(expires_at),
                    header: "authorization".to_owned(),
                    prefix: "Bearer ".to_owned(),
                    adapter_state: Some(json!({
                        "schema": fixture.schema,
                        "account_id": "postgres-native-account-123"
                    })),
                    proxy_url: Some(fixture.proxy_url.to_owned()),
                    proxy_network_scope: Some(
                        memeloop_token_center::network::OutboundScope::Private,
                    ),
                },
                oauth_session_id: Some(Uuid::now_v7()),
                oauth_driver: Some("damaged-driver".to_owned()),
                oauth_refresh_url: Some("https://damaged.invalid".to_owned()),
            },
            key_material,
        )
        .await
        .unwrap();
    let route = database
        .create_model_route(CreateModelRouteInput {
            tenant_external_id: tenant.clone(),
            public_model: format!("postgres-native-upgrade-model-{}", fixture.label),
            upstream_account_id: account.id,
            upstream_model: "gpt-5.6-sol".to_owned(),
            protocol: "openai".to_owned(),
            priority: 0,
        })
        .await
        .unwrap();
    let issued_key = database
        .create_key_with_routing(
            CreateKeyInput {
                tenant_external_id: tenant.clone(),
                principal_external_id: format!("postgres-native-principal-{}", fixture.label),
                alias: format!("postgres-native-key-{}", fixture.label),
                currency: "USD".to_owned(),
                policy: KeyPolicy::default(),
                initial_balance: Decimal::ONE,
                idempotency_key: None,
            },
            &[route.id],
            &[],
            key_material,
        )
        .await
        .unwrap();
    let ciphertext: String = sqlx::query_scalar(
        "SELECT credential_ciphertext FROM upstream_credentials WHERE upstream_account_id = $1 AND generation = 1",
    )
    .bind(account.id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    let history_created_at = unix_millis();
    sqlx::query(
        "UPDATE upstream_credentials SET generation = 5 WHERE upstream_account_id = $1 AND generation = 1",
    )
    .bind(account.id.to_string())
    .execute(&database.pool)
    .await
    .unwrap();
    for generation in 1_i64..5 {
        sqlx::query(
            "INSERT INTO upstream_credentials (id, upstream_account_id, generation, credential_ciphertext, expires_at, created_at, revoked_at) VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(account.id.to_string())
        .bind(generation)
        .bind(&ciphertext)
        .bind(expires_at)
        .bind(history_created_at.saturating_sub(generation))
        .bind(history_created_at.saturating_sub(generation))
        .execute(&database.pool)
        .await
        .unwrap();
    }
    sqlx::query(
        "UPDATE upstream_accounts SET credential_generation = 5, status = 'disabled' WHERE id = $1",
    )
    .bind(account.id.to_string())
    .execute(&database.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO upstream_account_imports (tenant_id, import_kind, source_key, contract_version, payload_digest, upstream_account_id, created_at) VALUES ($1, 'cpa_managed_oauth', $2, 1, $3, $4, $5)",
    )
    .bind(account.tenant_id.to_string())
    .bind("a".repeat(64))
    .bind("b".repeat(64))
    .bind(account.id.to_string())
    .bind(unix_millis())
    .execute(&database.pool)
    .await
    .unwrap();

    let plan = database
        .prepare_native_codex_upgrade(&[account.id], key_material)
        .await
        .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0].account_id, account.id);
    assert_eq!(plan[0].expected_credential_generation, 5);
    let report = database
        .apply_native_codex_upgrade(&plan, key_material)
        .await
        .unwrap();
    assert_eq!(report.upgraded_account_ids, vec![account.id]);

    let (upgraded, credential) = database
        .upstream_account_with_credential(account.id, key_material)
        .await
        .unwrap();
    assert_eq!(upgraded.id, account.id);
    assert_eq!(upgraded.driver, "openai-codex");
    assert_eq!(upgraded.auth_kind, "oauth");
    assert_eq!(upgraded.status, "disabled");
    assert_eq!(upgraded.credential_generation, 5);
    assert_eq!(upgraded.credential_expires_at, Some(expires_at));
    assert_eq!(upgraded.route_count, 1);
    assert_eq!(
        credential.adapter_state(),
        Some(&json!({
            "schema": "openai-codex-oauth-v1",
            "account_id": "postgres-native-account-123"
        }))
    );
    assert_eq!(
        credential.proxy(),
        Some((
            fixture.expected_proxy_url,
            memeloop_token_center::network::OutboundScope::Private
        ))
    );
    let UpstreamCredential::OAuth {
        access_token,
        refresh_token,
        expires_at: actual_expires_at,
        ..
    } = credential
    else {
        panic!("native upgrade must retain an OAuth credential");
    };
    assert_eq!(access_token, "postgres-native-access-secret");
    assert_eq!(
        refresh_token.as_deref(),
        Some("postgres-native-refresh-secret")
    );
    assert_eq!(actual_expires_at, Some(expires_at));
    assert_eq!(
        database
            .credential_routing(issued_key.key_id, &tenant)
            .await
            .unwrap()
            .route_ids,
        vec![route.id]
    );
    let grant_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM routing_grants WHERE tenant_id = $1 AND key_id = $2 AND model_route_id = $3",
    )
    .bind(account.tenant_id.to_string())
    .bind(issued_key.key_id.to_string())
    .bind(route.id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(grant_count, 1);
    let history_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM upstream_credentials WHERE upstream_account_id = $1",
    )
    .bind(account.id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    let revoked_history_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM upstream_credentials WHERE upstream_account_id = $1 AND revoked_at IS NOT NULL",
    )
    .bind(account.id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(history_count, 5);
    assert_eq!(revoked_history_count, 4);
    let preserved_history_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM upstream_credentials WHERE upstream_account_id = $1 AND generation < 5 AND credential_ciphertext = $2 AND revoked_at IS NOT NULL",
    )
    .bind(account.id.to_string())
    .bind(&ciphertext)
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(preserved_history_count, 4);
}

#[tokio::test]
async fn postgres_native_codex_upgrade_recovers_generation_five_old_and_native_envelopes() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let database = Database::connect_with_max(&database_url, 16).await.unwrap();
    database.migrate().await.unwrap();
    for fixture in [
        PostgresNativeCodexFixture {
            label: "old-envelope",
            schema: "cpa-codex-oauth-v1",
            proxy_url: "socks5://operator:proxy-secret@100.64.0.16:1080",
            expected_proxy_url: "socks5h://operator:proxy-secret@100.64.0.16:1080",
        },
        PostgresNativeCodexFixture {
            label: "native-envelope",
            schema: "openai-codex-oauth-v1",
            proxy_url: "socks5h://operator:proxy-secret@100.64.0.16:1080",
            expected_proxy_url: "socks5h://operator:proxy-secret@100.64.0.16:1080",
        },
    ] {
        assert_postgres_native_codex_upgrade_fixture(&database, fixture).await;
    }
}

#[tokio::test]
async fn postgres_migrations_queue_aggregates_and_events_work_together() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let database = Database::connect(&database_url).await.unwrap();
    database.migrate().await.unwrap();
    database.maintain_partitions().await.unwrap();

    let index_pool = sqlx::PgPool::connect(&database_url).await.unwrap();
    let default_history_indexes_ready: bool = sqlx::query_scalar(
        "SELECT to_regclass(current_schema() || '.request_records_recent_idx') IS NOT NULL AND to_regclass(current_schema() || '.request_events_global_cursor_idx') IS NOT NULL AND to_regclass(current_schema() || '.request_records_global_model_time_idx') IS NOT NULL AND to_regclass(current_schema() || '.generation_jobs_global_model_time_idx') IS NOT NULL",
    )
    .fetch_one(&index_pool)
    .await
    .unwrap();
    assert!(
        default_history_indexes_ready,
        "fresh PostgreSQL migrations must install global request, model-filtered and SSE cursor indexes"
    );
    let unattached_model_index_leaves: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_inherits table_inheritance WHERE table_inheritance.inhparent = to_regclass(current_schema() || '.request_records') AND NOT EXISTS (SELECT 1 FROM pg_inherits index_inheritance JOIN pg_index child_index ON child_index.indexrelid = index_inheritance.inhrelid WHERE index_inheritance.inhparent = to_regclass(current_schema() || '.request_records_global_model_time_idx') AND child_index.indrelid = table_inheritance.inhrelid AND child_index.indisvalid AND child_index.indisready)",
    )
    .fetch_one(&index_pool)
    .await
    .unwrap();
    assert_eq!(
        unattached_model_index_leaves, 0,
        "the v59 parent model index must cover every request partition"
    );
    let model_index_definitions: Vec<String> = sqlx::query_scalar(
        "SELECT pg_get_indexdef(indexrelid) FROM pg_index WHERE indexrelid IN (to_regclass(current_schema() || '.request_records_global_model_time_idx'), to_regclass(current_schema() || '.generation_jobs_global_model_time_idx')) ORDER BY indexrelid::regclass::TEXT",
    )
    .fetch_all(&index_pool)
    .await
    .unwrap();
    assert_eq!(model_index_definitions.len(), 2);
    assert!(model_index_definitions.iter().any(|definition| {
        definition.contains("request_records")
            && definition.contains("(model, created_at DESC, id DESC)")
    }));
    assert!(model_index_definitions.iter().any(|definition| {
        definition.contains("generation_jobs")
            && definition.contains("(public_model, created_at DESC, id DESC)")
    }));
    let mut explain = index_pool.begin().await.unwrap();
    sqlx::query("SET LOCAL enable_seqscan = off")
        .execute(&mut *explain)
        .await
        .unwrap();
    let request_model_plan: Vec<String> = sqlx::query_scalar(
        "EXPLAIN (FORMAT TEXT, COSTS OFF) SELECT id, created_at FROM request_records WHERE created_at >= 0 AND created_at <= 9223372036854775807 AND model = 'needle' ORDER BY created_at DESC, id DESC LIMIT 5",
    )
    .fetch_all(&mut *explain)
    .await
    .unwrap();
    let generation_model_plan: Vec<String> = sqlx::query_scalar(
        "EXPLAIN (FORMAT TEXT, COSTS OFF) SELECT id, created_at FROM generation_jobs WHERE created_at >= 0 AND created_at <= 9223372036854775807 AND public_model = 'needle' ORDER BY created_at DESC, id DESC LIMIT 5",
    )
    .fetch_all(&mut *explain)
    .await
    .unwrap();
    explain.rollback().await.unwrap();
    for (source, plan, model_column) in [
        ("request_records", request_model_plan, "model"),
        ("generation_jobs", generation_model_plan, "public_model"),
    ] {
        let rendered = plan.join("\n");
        assert!(
            !rendered.contains("Seq Scan"),
            "global {source} model Top-N must not fall back to a sequential scan: {rendered}"
        );
        assert!(
            rendered.contains("Index") && rendered.contains(model_column),
            "global {source} model Top-N must expose an ordered model-index plan: {rendered}"
        );
    }
    index_pool.close().await;

    let unique = Uuid::now_v7();
    let tenant = format!("postgres-test-{unique}");
    let pepper = b"postgres integration pepper longer than thirty-two bytes";
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: tenant.clone(),
                principal_external_id: "member".to_owned(),
                alias: "postgres-integration".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy {
                    allowed_models: vec!["video-test".to_owned()],
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
    let account = database
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: tenant.clone(),
                name: "postgres-comfy".to_owned(),
                driver: "comfyui".to_owned(),
                config: json!({"base_url": "http://comfy.example.test", "api_prefix": ""}),
                credential: UpstreamCredential::None,
                oauth_session_id: None,
                oauth_driver: None,
                oauth_refresh_url: None,
            },
            pepper,
        )
        .await
        .unwrap();
    database
        .create_model_route(CreateModelRouteInput {
            tenant_external_id: tenant.clone(),
            public_model: "video-test".to_owned(),
            upstream_account_id: account.id,
            upstream_model: "workflow-test".to_owned(),
            protocol: "generation".to_owned(),
            priority: 0,
        })
        .await
        .unwrap();
    let price = database
        .upsert_generation_price("video-test", "USD", "job", Decimal::new(25, 2))
        .await
        .unwrap();
    let reservation = database
        .reserve_usage(&key, &price.reservation_price().unwrap(), 0, 1)
        .await
        .unwrap();
    let job_id = Uuid::now_v7();
    database
        .create_generation_job(CreateGenerationJobInput {
            job_id,
            key: key.clone(),
            upstream_account_id: account.id,
            reservation: reservation.clone(),
            public_model: "video-test".to_owned(),
            upstream_model: "workflow-test".to_owned(),
            driver: "comfyui".to_owned(),
            request_object: "objects/blake3/test".to_owned(),
            estimated_units: 1,
            billing_unit: price.billing_unit.clone(),
            micros_per_unit: price.micros_per_unit,
        })
        .await
        .unwrap();
    let claimed = database
        .claim_generation_job("postgres-integration-worker")
        .await
        .unwrap()
        .expect("queued generation job");
    assert_eq!(claimed.job_id, job_id);
    let submission_nonce = Uuid::now_v7();
    database
        .mark_generation_submitting(job_id, "postgres-integration-worker", submission_nonce)
        .await
        .unwrap();
    database
        .mark_generation_submitted(
            job_id,
            "postgres-integration-worker",
            submission_nonce,
            "postgres-upstream-generation-job",
        )
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(2_050)).await;
    let claimed = database
        .claim_generation_job("postgres-integration-worker")
        .await
        .unwrap()
        .expect("running generation job");
    assert_eq!(claimed.job_id, job_id);
    let attempt_nonce = Uuid::now_v7();
    let asset = ArchivedGenerationAsset {
        asset_id: Uuid::now_v7(),
        index: 0,
        object_locator: format!("staging/generation/{job_id}/{attempt_nonce}/asset-0"),
        mime_type: "image/png".to_owned(),
        size_bytes: 17,
        filename: "test-result.png".to_owned(),
    };
    let manifest = GenerationStagedAssets {
        attempt_nonce,
        billed_units: 1,
        assets: vec![asset.clone()],
    };
    database
        .save_generation_staged_assets(job_id, "postgres-integration-worker", &manifest)
        .await
        .unwrap();
    let cost = database
        .finish_generation_job(FinishGenerationJobInput {
            job_id,
            worker_id: "postgres-integration-worker",
            status: "succeeded",
            billed_units: 1,
            error_code: None,
            assets: std::slice::from_ref(&asset),
            staged_assets: Some(&manifest),
        })
        .await
        .unwrap();
    assert_eq!(cost, 250_000);
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(8));
    let mut terminal_replays = Vec::new();
    for _ in 0..8 {
        let database = database.clone();
        let barrier = barrier.clone();
        let replay_manifest = manifest.clone();
        terminal_replays.push(tokio::spawn(async move {
            barrier.wait().await;
            database
                .finish_generation_job(FinishGenerationJobInput {
                    job_id,
                    worker_id: "postgres-integration-worker",
                    status: "succeeded",
                    billed_units: 1,
                    error_code: None,
                    assets: &replay_manifest.assets,
                    staged_assets: Some(&replay_manifest),
                })
                .await
        }));
    }
    for replay in terminal_replays {
        assert_eq!(replay.await.unwrap().unwrap(), 250_000);
    }
    assert_eq!(
        database
            .list_account_ledger(issued.account_id, 100)
            .await
            .unwrap()
            .into_iter()
            .filter(|entry| entry.kind == "usage" && entry.source == reservation.id.to_string())
            .count(),
        1
    );

    let range = StatsFilter {
        from_created_at: Some(unix_millis().saturating_sub(60_000)),
        to_created_at: Some(unix_millis().saturating_add(1)),
        ..Default::default()
    };
    let stats = database
        .stats_filtered(key.key_id, range.clone())
        .await
        .unwrap();
    assert_eq!(stats.summary.total_requests, 1);
    assert_eq!(stats.summary.successful_requests, 1);
    assert_eq!(stats.summary.total_cost.as_deref(), Some("0.25"));
    assert_eq!(stats.by_model[0].name, "video-test");
    let operator_stats = database
        .operator_stats_filtered(&tenant, range)
        .await
        .unwrap();
    assert_eq!(operator_stats.summary.total_requests, 1);
    assert_eq!(operator_stats.summary.successful_requests, 1);
    assert_eq!(operator_stats.summary.total_cost.as_deref(), Some("0.25"));
    assert_eq!(operator_stats.by_model[0].name, "video-test");
    let requests = database.list_all_requests(&tenant, 10).await.unwrap();
    assert_eq!(requests[0].protocol, "generation");
    assert_eq!(requests[0].status_code, Some(200));
    let key_detail = database
        .request_archive_refs(key.key_id, job_id)
        .await
        .unwrap();
    assert_eq!(key_detail.view.protocol, "generation");
    let result = json!({
        "provider": {"status": "success"},
        "assets": [{
            "asset_id": asset.asset_id,
            "index": 0,
            "mime_type": "image/png",
            "size_bytes": 17,
            "filename": "test-result.png"
        }]
    });
    assert_eq!(key_detail.response_json, Some(result.clone()));
    let operator_detail = database
        .request_archive_refs_for_tenant(&tenant, job_id)
        .await
        .unwrap();
    assert_eq!(operator_detail.view.cost, "0.25");
    assert_eq!(operator_detail.response_json, Some(result));
    let events = database
        .request_events_after(&tenant, 0, None, 10)
        .await
        .unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].event_kind, "started");
    assert_eq!(events[1].event_kind, "finished");

    assert_eq!(
        database
            .grant(
                issued.account_id,
                Decimal::ONE,
                "postgres-subscription",
                &format!("postgres:{unique}:grant"),
            )
            .await
            .unwrap(),
        "1"
    );
    let reversal_idempotency = format!("postgres:{unique}:reversal");
    for _ in 0..2 {
        assert_eq!(
            database
                .reverse_grant(
                    issued.account_id,
                    &format!("postgres:{unique}:grant"),
                    "postgres-subscription-cancelled",
                    &reversal_idempotency,
                )
                .await
                .unwrap(),
            "1"
        );
    }
    database
        .plugin_kv_put("postgres-plugin", &format!("state/{unique}"), b"durable")
        .await
        .unwrap();
    assert_eq!(
        database
            .plugin_kv_get("postgres-plugin", &format!("state/{unique}"))
            .await
            .unwrap(),
        Some(b"durable".to_vec())
    );
}
