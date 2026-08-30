use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use memeloop_token_center::{
    AppState, api,
    config::Config,
    db::{
        CreateKeyInput, CreateServiceTokenInput, CreateUpstreamAccountInput, FinishRequest,
        NewRequest,
    },
    model::{AuthenticatedKey, KeyPolicy},
    provider::UpstreamCredential,
};
use rust_decimal::Decimal;
use serde_json::Value;
use sqlx::AnyPool;
use tower::ServiceExt;
use uuid::Uuid;

const PEPPER: &[u8] = b"usage analysis integration pepper is sufficiently long";

async fn get_json(state: &AppState, path: &str, token: &str) -> (StatusCode, Value) {
    let response = api::router(state.clone())
        .oneshot(
            Request::builder()
                .uri(path)
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();
    let value = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&body).into_owned()))
    };
    (status, value)
}

async fn issue(
    state: &AppState,
    tenant: &str,
    principal: &str,
    alias: &str,
    currency: &str,
) -> AuthenticatedKey {
    let issued = state
        .db
        .create_key(
            CreateKeyInput {
                tenant_external_id: tenant.to_owned(),
                principal_external_id: principal.to_owned(),
                alias: alias.to_owned(),
                currency: currency.to_owned(),
                policy: KeyPolicy {
                    allowed_models: vec!["*".to_owned()],
                    ..KeyPolicy::default()
                },
                initial_balance: Decimal::TEN,
                idempotency_key: None,
            },
            PEPPER,
        )
        .await
        .unwrap();
    state
        .db
        .authenticate_key(&issued.key, PEPPER)
        .await
        .unwrap()
}

async fn issue_with_credential(
    state: &AppState,
    tenant: &str,
    principal: &str,
    alias: &str,
    currency: &str,
) -> (AuthenticatedKey, String) {
    let issued = state
        .db
        .create_key(
            CreateKeyInput {
                tenant_external_id: tenant.to_owned(),
                principal_external_id: principal.to_owned(),
                alias: alias.to_owned(),
                currency: currency.to_owned(),
                policy: KeyPolicy {
                    allowed_models: vec!["*".to_owned()],
                    ..KeyPolicy::default()
                },
                initial_balance: Decimal::TEN,
                idempotency_key: None,
            },
            PEPPER,
        )
        .await
        .unwrap();
    let authenticated = state
        .db
        .authenticate_key(&issued.key, PEPPER)
        .await
        .unwrap();
    (authenticated, issued.key)
}

struct UsageSample<'a> {
    model: &'a str,
    status_code: i64,
    duration_ms: i64,
    input_tokens: i64,
    cached_input_tokens: i64,
    cache_write_tokens: i64,
    output_tokens: i64,
    cost_micros: i64,
    error_code: Option<&'a str>,
}

async fn finish(state: &AppState, key: &AuthenticatedKey, sample: UsageSample<'_>) -> Uuid {
    let request_id = Uuid::now_v7();
    state
        .db
        .record_request_started(NewRequest {
            request_id,
            key_id: key.key_id,
            tenant_id: key.tenant_id,
            protocol: "openai-responses".to_owned(),
            model: sample.model.to_owned(),
            request_object: format!("memory://usage-analysis/{request_id}/request"),
            reservation_id: Uuid::now_v7(),
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap();
    state
        .db
        .record_request_finished(FinishRequest {
            request_id,
            status_code: sample.status_code,
            duration_ms: sample.duration_ms,
            input_tokens: sample.input_tokens,
            cached_input_tokens: sample.cached_input_tokens,
            cache_write_tokens: sample.cache_write_tokens,
            output_tokens: sample.output_tokens,
            service_tier: Some("priority".to_owned()),
            cost_micros: sample.cost_micros,
            error_code: sample.error_code.map(str::to_owned),
            response_object: format!("memory://usage-analysis/{request_id}/response"),
        })
        .await
        .unwrap();
    request_id
}

async fn move_request_fact(database_url: &str, request_id: Uuid, created_at: i64) {
    let pool = AnyPool::connect(database_url).await.unwrap();
    let result =
        sqlx::query("UPDATE request_stats_facts SET created_at = $1 WHERE request_id = $2")
            .bind(created_at)
            .bind(request_id.to_string())
            .execute(&pool)
            .await
            .unwrap();
    assert_eq!(result.rows_affected(), 1);
    pool.close().await;
}

fn assert_exact_boundary_response(body: &Value, bucket_start: i64) {
    assert_eq!(body["summary"]["requests"], 2, "{body}");
    assert_eq!(body["summary"]["input_tokens"], 110, "{body}");
    assert_eq!(body["summary"]["costs"][0]["cost"], "110", "{body}");
    assert_eq!(body["time_series"][0]["bucket_start"], bucket_start);
    assert_eq!(body["time_series"][0]["requests"], 2, "{body}");
    assert_eq!(
        body["heatmap"]
            .as_array()
            .unwrap()
            .iter()
            .map(|bucket| bucket["requests"].as_i64().unwrap())
            .sum::<i64>(),
        2,
        "{body}"
    );
}

enum SeedUsageKind<'a> {
    Request {
        input_tokens: i64,
    },
    Generation {
        units: i64,
        modality: &'a str,
        billing_unit: &'a str,
    },
}

struct SeedUsageActivity<'a> {
    key: &'a AuthenticatedKey,
    currency: &'a str,
    model: &'a str,
    created_at: i64,
    status_class: &'a str,
    error_code: &'a str,
    upstream_account_id: Option<Uuid>,
    cost_micros: i64,
    kind: SeedUsageKind<'a>,
}

async fn seed_usage_activity(pool: &AnyPool, activity: SeedUsageActivity<'_>) {
    let tenant_id = activity.key.tenant_id.to_string();
    let key_id = activity.key.key_id.to_string();
    let upstream_account_id = activity
        .upstream_account_id
        .map(|id| id.to_string())
        .unwrap_or_default();
    let (source_kind, protocol, input_tokens, generation_units, modality, billing_unit) =
        match activity.kind {
            SeedUsageKind::Request { input_tokens } => {
                let request_id = Uuid::now_v7().to_string();
                sqlx::query(
                    r#"INSERT INTO request_records (
                           id, tenant_id, key_id, created_at, completed_at, protocol, model,
                           status_code, duration_ms, input_tokens, output_tokens, cost_micros,
                           error_code, request_object, response_object, reservation_id,
                           upstream_account_id, model_route_id, currency
                       ) VALUES ($1, $2, $3, $4, $4, 'openai-responses', $5, $6, 20, $7,
                                 0, $8, NULLIF($9, ''), $10, $11, $12, NULLIF($13, ''),
                                 NULL, $14)"#,
                )
                .bind(&request_id)
                .bind(&tenant_id)
                .bind(&key_id)
                .bind(activity.created_at)
                .bind(activity.model)
                .bind(if activity.status_class == "success" {
                    200_i64
                } else {
                    500_i64
                })
                .bind(input_tokens)
                .bind(activity.cost_micros)
                .bind(activity.error_code)
                .bind(format!("memory://usage-analysis/{request_id}/request"))
                .bind(format!("memory://usage-analysis/{request_id}/response"))
                .bind(Uuid::now_v7().to_string())
                .bind(&upstream_account_id)
                .bind(activity.currency)
                .execute(pool)
                .await
                .unwrap();
                sqlx::query(
                    r#"INSERT INTO request_stats_facts (
                       request_id, tenant_id, key_id, created_at, model, protocol,
                       status_class, error_code, upstream_account_id, model_route_id,
                       duration_ms, input_tokens, output_tokens, cached_input_tokens,
                       cache_write_tokens, service_tier, currency, cost_micros, session_id
                   ) VALUES ($1, $2, $3, $4, $5, 'openai-responses', $6, $7, $8, '',
                             20, $9, 0, 0, 0, 'default', $10, $11, $12)"#,
                )
                .bind(&request_id)
                .bind(&tenant_id)
                .bind(&key_id)
                .bind(activity.created_at)
                .bind(activity.model)
                .bind(activity.status_class)
                .bind(activity.error_code)
                .bind(&upstream_account_id)
                .bind(input_tokens)
                .bind(activity.currency)
                .bind(activity.cost_micros)
                .bind(format!("unlinked:{key_id}"))
                .execute(pool)
                .await
                .unwrap();
                ("request", "openai", input_tokens, 0, "", "")
            }
            SeedUsageKind::Generation {
                units,
                modality,
                billing_unit,
            } => {
                sqlx::query(
                    r#"INSERT INTO generation_stats_facts (
                       job_id, tenant_id, key_id, created_at, model, status_class,
                       error_code, upstream_account_id, duration_ms, cost_micros, billed_units,
                       currency, modality, billing_unit, model_route_id
                   ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 20, $9, $10, $11,
                             $12, $13, '')"#,
                )
                .bind(Uuid::now_v7().to_string())
                .bind(&tenant_id)
                .bind(&key_id)
                .bind(activity.created_at)
                .bind(activity.model)
                .bind(activity.status_class)
                .bind(activity.error_code)
                .bind(&upstream_account_id)
                .bind(activity.cost_micros)
                .bind(units)
                .bind(activity.currency)
                .bind(modality)
                .bind(billing_unit)
                .execute(pool)
                .await
                .unwrap();
                ("generation", "generation", 0, units, modality, billing_unit)
            }
        };

    for (table, bucket_column, bucket) in [
        (
            "usage_analysis_hourly",
            "hour_bucket",
            activity.created_at.div_euclid(3_600_000),
        ),
        (
            "usage_analysis_daily",
            "day_bucket",
            activity.created_at.div_euclid(86_400_000),
        ),
    ] {
        let sql = format!(
            r#"INSERT INTO {table} (
                   tenant_id, key_id, {bucket_column}, source_kind, model, protocol,
                   status_class, error_code, upstream_account_id, model_route_id,
                   service_tier, currency, requests, input_tokens, output_tokens,
                   cached_input_tokens, cache_write_tokens, generation_units,
                   duration_count, duration_sum_ms, duration_bucket_0, duration_bucket_1,
                   duration_bucket_2, duration_bucket_3, duration_bucket_4, duration_bucket_5,
                   duration_bucket_6, duration_bucket_7, duration_bucket_8, duration_bucket_9,
                   duration_bucket_10, duration_bucket_11, cost_micros
               ) VALUES (
                   $1, $2, $3, $4, $5, $6, $7, $8, $9, '', 'default', $10,
                   1, $11, 0, 0, 0, $12, 1, 20, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, $13
               )"#
        );
        // Test-only SQL safety boundary: `table` and `bucket_column` come from the two literal
        // tuples in this loop. All fixture values remain bind parameters below.
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(&tenant_id)
            .bind(&key_id)
            .bind(bucket)
            .bind(source_kind)
            .bind(activity.model)
            .bind(protocol)
            .bind(activity.status_class)
            .bind(activity.error_code)
            .bind(&upstream_account_id)
            .bind(activity.currency)
            .bind(input_tokens)
            .bind(generation_units)
            .bind(activity.cost_micros)
            .execute(pool)
            .await
            .unwrap();
    }
    if source_kind == "request" {
        for (table, bucket_column, bucket) in [
            (
                "session_usage_hourly",
                "hour_bucket",
                activity.created_at.div_euclid(3_600_000),
            ),
            (
                "session_usage_daily",
                "day_bucket",
                activity.created_at.div_euclid(86_400_000),
            ),
        ] {
            let sql = format!(
                r#"INSERT INTO {table} (
                       tenant_id, key_id, session_id, {bucket_column}, model, protocol,
                       status_class, error_code, upstream_account_id, model_route_id,
                       currency, requests, input_tokens, output_tokens, duration_count,
                       duration_sum_ms, cost_micros
                   ) VALUES ($1, $2, $3, $4, $5, 'openai', $6, $7, $8, '', $9,
                             1, $10, 0, 1, 20, $11)"#
            );
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(&tenant_id)
                .bind(&key_id)
                .bind(format!("unlinked:{key_id}"))
                .bind(bucket)
                .bind(activity.model)
                .bind(activity.status_class)
                .bind(activity.error_code)
                .bind(&upstream_account_id)
                .bind(activity.currency)
                .bind(input_tokens)
                .bind(activity.cost_micros)
                .execute(pool)
                .await
                .unwrap();
        }
    }
    if source_kind == "generation" {
        for (table, bucket_column, bucket) in [
            (
                "generation_usage_dimensions_hourly",
                "hour_bucket",
                activity.created_at.div_euclid(3_600_000),
            ),
            (
                "generation_usage_dimensions_daily",
                "day_bucket",
                activity.created_at.div_euclid(86_400_000),
            ),
        ] {
            let sql = format!(
                r#"INSERT INTO {table} (
                       tenant_id, key_id, {bucket_column}, model, status_class,
                       error_code, upstream_account_id, model_route_id, modality,
                       billing_unit, currency, units
                   ) VALUES ($1, $2, $3, $4, $5, $6, $7, '', $8, $9, $10, $11)"#
            );
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(&tenant_id)
                .bind(&key_id)
                .bind(bucket)
                .bind(activity.model)
                .bind(activity.status_class)
                .bind(activity.error_code)
                .bind(&upstream_account_id)
                .bind(modality)
                .bind(billing_unit)
                .bind(activity.currency)
                .bind(generation_units)
                .execute(pool)
                .await
                .unwrap();
        }
    }
}

fn assert_multibucket_metrics(body: &Value) {
    assert_eq!(body["upstream_grouping"], "stable_account", "{body}");
    assert_eq!(body["summary"]["requests"], 7, "{body}");
    assert_eq!(body["summary"]["success"], 4, "{body}");
    assert_eq!(body["summary"]["failed"], 3, "{body}");
    assert_eq!(body["summary"]["input_tokens"], 90, "{body}");
    assert_eq!(body["summary"]["generation_units"], 12, "{body}");
    assert_eq!(
        body["generation_units_by_modality"],
        serde_json::json!([
            {"modality": "image", "currency": "CNY", "units": 8},
            {"modality": "video", "currency": "USD", "units": 4}
        ]),
        "{body}"
    );
    assert_eq!(
        body["generation_units_by_billing_unit"],
        serde_json::json!([
            {"billing_unit": "job", "currency": "CNY", "units": 8},
            {"billing_unit": "second", "currency": "USD", "units": 4}
        ]),
        "{body}"
    );
    assert_eq!(body["summary"]["costs"][0]["currency"], "CNY", "{body}");
    assert_eq!(body["summary"]["costs"][0]["cost"], "11", "{body}");
    assert_eq!(body["summary"]["costs"][1]["currency"], "USD", "{body}");
    assert_eq!(body["summary"]["costs"][1]["cost"], "10", "{body}");
    assert_eq!(
        body["time_series"]
            .as_array()
            .unwrap()
            .iter()
            .map(|bucket| bucket["requests"].as_i64().unwrap())
            .sum::<i64>(),
        7,
        "{body}"
    );
    assert_eq!(
        body["heatmap"]
            .as_array()
            .unwrap()
            .iter()
            .map(|bucket| bucket["requests"].as_i64().unwrap())
            .sum::<i64>(),
        7,
        "{body}"
    );
    assert!(
        body["by_status"]
            .as_array()
            .unwrap()
            .iter()
            .any(|bucket| bucket["id"] == "error" && bucket["requests"] == 3),
        "{body}"
    );
    let by_session = body["by_session"].as_array().unwrap();
    assert_eq!(by_session.len(), 2, "{body}");
    assert!(
        by_session
            .iter()
            .any(|bucket| bucket["requests"] == 3 && bucket["costs"][0]["currency"] == "USD"),
        "{body}"
    );
    assert!(
        by_session.iter().any(|bucket| bucket["requests"] == 3
            && bucket["generation_units"] == 8
            && bucket["costs"][0]["currency"] == "CNY"),
        "{body}"
    );
}

async fn assert_multibucket_usage_analysis(database_url: String, tenant: String) {
    let mut config = Config::for_test(database_url.clone());
    config.key_pepper = String::from_utf8(PEPPER.to_vec()).unwrap();
    let state = AppState::initialize(config).await.unwrap();
    let usd = issue(&state, &tenant, "Multi-USD", "Multi-USD", "USD").await;
    let cny = issue(&state, &tenant, "Multi-CNY", "Multi-CNY", "CNY").await;
    let upstream = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: tenant.clone(),
                name: "usage-analysis-assigned".to_owned(),
                driver: "http-json".to_owned(),
                config: serde_json::json!({"base_url": "https://usage.example.test"}),
                credential: UpstreamCredential::None,
                oauth_session_id: None,
                oauth_driver: None,
                oauth_refresh_url: None,
            },
            PEPPER,
        )
        .await
        .unwrap();
    let pool = AnyPool::connect(&database_url).await.unwrap();
    let now = memeloop_token_center::db::unix_millis();
    let day_start = now.div_euclid(86_400_000) * 86_400_000 - 2 * 86_400_000;
    let from = day_start + 6 * 3_600_000 + 15 * 60_000;
    let to = day_start + 2 * 86_400_000 + 18 * 3_600_000 + 45 * 60_000;
    seed_usage_activity(
        &pool,
        SeedUsageActivity {
            key: &usd,
            currency: "USD",
            model: "multi-before-upstream-rotation",
            created_at: from + 60_000,
            status_class: "success",
            error_code: "",
            upstream_account_id: Some(upstream.id),
            cost_micros: 0,
            kind: SeedUsageKind::Request { input_tokens: 0 },
        },
    )
    .await;
    let rotated = state
        .db
        .rotate_upstream_credential(
            upstream.id,
            UpstreamCredential::ApiKey {
                value: "usage-analysis-rotated-not-a-secret".to_owned(),
                header: "authorization".to_owned(),
                prefix: "Bearer ".to_owned(),
            },
            "usage-analysis-stable-account-rotation",
            PEPPER,
        )
        .await
        .unwrap();
    assert_eq!(rotated.id, upstream.id);
    assert_eq!(rotated.credential_generation, 2);
    let activities = [
        SeedUsageActivity {
            key: &usd,
            currency: "USD",
            model: "multi-outside-left",
            created_at: from - 1,
            status_class: "success",
            error_code: "",
            upstream_account_id: None,
            cost_micros: 70_000_000,
            kind: SeedUsageKind::Request { input_tokens: 700 },
        },
        SeedUsageActivity {
            key: &usd,
            currency: "USD",
            model: "multi-left-request",
            created_at: from,
            status_class: "success",
            error_code: "",
            upstream_account_id: None,
            cost_micros: 1_000_000,
            kind: SeedUsageKind::Request { input_tokens: 10 },
        },
        SeedUsageActivity {
            key: &cny,
            currency: "CNY",
            model: "multi-left-generation",
            created_at: from + 5 * 60_000,
            status_class: "failure",
            error_code: "edge_generation_error",
            upstream_account_id: None,
            cost_micros: 2_000_000,
            kind: SeedUsageKind::Generation {
                units: 2,
                modality: "image",
                billing_unit: "job",
            },
        },
        SeedUsageActivity {
            key: &cny,
            currency: "CNY",
            model: "multi-interior-request",
            created_at: day_start + 86_400_000 + 12 * 3_600_000,
            status_class: "failure",
            error_code: "interior_request_error",
            upstream_account_id: None,
            cost_micros: 3_000_000,
            kind: SeedUsageKind::Request { input_tokens: 30 },
        },
        SeedUsageActivity {
            key: &usd,
            currency: "USD",
            model: "multi-interior-generation",
            created_at: day_start + 86_400_000 + 13 * 3_600_000,
            status_class: "success",
            error_code: "",
            upstream_account_id: Some(upstream.id),
            cost_micros: 4_000_000,
            kind: SeedUsageKind::Generation {
                units: 4,
                modality: "video",
                billing_unit: "second",
            },
        },
        SeedUsageActivity {
            key: &usd,
            currency: "USD",
            model: "multi-right-request",
            created_at: to - 5 * 60_000,
            status_class: "failure",
            error_code: "right_request_error",
            upstream_account_id: None,
            cost_micros: 5_000_000,
            kind: SeedUsageKind::Request { input_tokens: 50 },
        },
        SeedUsageActivity {
            key: &cny,
            currency: "CNY",
            model: "multi-right-generation",
            created_at: to,
            status_class: "success",
            error_code: "",
            upstream_account_id: None,
            cost_micros: 6_000_000,
            kind: SeedUsageKind::Generation {
                units: 6,
                modality: "image",
                billing_unit: "job",
            },
        },
        SeedUsageActivity {
            key: &cny,
            currency: "CNY",
            model: "multi-outside-right",
            created_at: to + 1,
            status_class: "success",
            error_code: "",
            upstream_account_id: None,
            cost_micros: 80_000_000,
            kind: SeedUsageKind::Generation {
                units: 80,
                modality: "image",
                billing_unit: "job",
            },
        },
    ];
    for activity in activities {
        seed_usage_activity(&pool, activity).await;
    }
    pool.close().await;

    let service = state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: format!("usage-multibucket-{tenant}"),
                scopes: vec!["requests:read".to_owned()],
                tenant_external_id: Some(tenant),
            },
            PEPPER,
        )
        .await
        .unwrap();
    for granularity in ["hour", "day"] {
        let (status, body) = get_json(
            &state,
            &format!(
                "/internal/v1/usage-analysis?from_created_at={from}&to_created_at={to}&granularity={granularity}"
            ),
            &service.token,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_multibucket_metrics(&body);
        if granularity == "day" {
            assert_eq!(body["time_series"].as_array().unwrap().len(), 3, "{body}");
        }
    }

    let (status, error_only) = get_json(
        &state,
        &format!(
            "/internal/v1/usage-analysis?from_created_at={from}&to_created_at={to}&status=error"
        ),
        &service.token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{error_only}");
    assert_eq!(error_only["summary"]["requests"], 3, "{error_only}");
    assert!(
        error_only["by_status"]
            .as_array()
            .unwrap()
            .iter()
            .all(|bucket| bucket["id"] == "error"),
        "{error_only}"
    );

    let (status, unassigned) = get_json(
        &state,
        &format!(
            "/internal/v1/usage-analysis?from_created_at={from}&to_created_at={to}&upstream_account_id=unassigned"
        ),
        &service.token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{unassigned}");
    assert_eq!(unassigned["summary"]["requests"], 5, "{unassigned}");
    let (status, assigned) = get_json(
        &state,
        &format!(
            "/internal/v1/usage-analysis?from_created_at={from}&to_created_at={to}&upstream_account_id={}",
            upstream.id
        ),
        &service.token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{assigned}");
    assert_eq!(assigned["summary"]["requests"], 2, "{assigned}");
    assert_eq!(assigned["summary"]["generation_units"], 4, "{assigned}");
    assert_eq!(assigned["by_upstream"].as_array().unwrap().len(), 1);
    assert_eq!(assigned["by_upstream"][0]["id"], upstream.id.to_string());

    let (status, generation_only) = get_json(
        &state,
        &format!(
            "/internal/v1/usage-analysis?from_created_at={from}&to_created_at={to}&protocol=generation"
        ),
        &service.token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{generation_only}");
    assert_eq!(
        generation_only["summary"]["requests"], 3,
        "{generation_only}"
    );
    assert_eq!(
        generation_only["summary"]["generation_units"], 12,
        "{generation_only}"
    );
    assert_eq!(generation_only["by_protocol"][0]["id"], "generation");

    let (status, error_code_only) = get_json(
        &state,
        &format!(
            "/internal/v1/usage-analysis?from_created_at={from}&to_created_at={to}&error_code=interior_request_error"
        ),
        &service.token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{error_code_only}");
    assert_eq!(
        error_code_only["summary"]["requests"], 1,
        "{error_code_only}"
    );
    assert_eq!(error_code_only["errors"][0]["id"], "interior_request_error");

    let (status, model_only) = get_json(
        &state,
        &format!(
            "/internal/v1/usage-analysis?from_created_at={from}&to_created_at={to}&model=multi-interior-request"
        ),
        &service.token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{model_only}");
    assert_eq!(model_only["summary"]["requests"], 1, "{model_only}");
    assert_eq!(model_only["by_model"][0]["id"], "multi-interior-request");

    let (status, key_only) = get_json(
        &state,
        &format!(
            "/internal/v1/usage-analysis?from_created_at={from}&to_created_at={to}&key_id={}",
            cny.key_id
        ),
        &service.token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{key_only}");
    assert_eq!(key_only["summary"]["requests"], 3, "{key_only}");
    assert_eq!(key_only["by_key"][0]["id"], cny.key_id.to_string());
    assert_eq!(key_only["by_key"][0]["label"], "Multi-CNY");

    let (status, _) = get_json(
        &state,
        &format!(
            "/internal/v1/usage-analysis?from_created_at={from}&to_created_at={to}&upstream_account_id=not-a-uuid"
        ),
        &service.token,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

async fn assert_self_usage_analysis_is_stable_key_scoped(
    database_url: String,
    tenant_prefix: &str,
) {
    let mut config = Config::for_test(database_url.clone());
    config.key_pepper = String::from_utf8(PEPPER.to_vec()).unwrap();
    let state = AppState::initialize(config).await.unwrap();
    let unique = Uuid::now_v7();
    let tenant = format!("{tenant_prefix}-{unique}");
    let foreign_tenant = format!("{tenant_prefix}-foreign-{unique}");
    let (viewer, viewer_credential) =
        issue_with_credential(&state, &tenant, "Self Viewer", "Self Viewer", "USD").await;
    let (same_tenant_foreign, foreign_credential) =
        issue_with_credential(&state, &tenant, "Foreign Key", "Foreign Key", "CNY").await;
    let other_tenant = issue(
        &state,
        &foreign_tenant,
        "Other Tenant",
        "Other Tenant",
        "USD",
    )
    .await;

    finish(
        &state,
        &viewer,
        UsageSample {
            model: "self-text-model",
            status_code: 200,
            duration_ms: 20,
            input_tokens: 100,
            cached_input_tokens: 30,
            cache_write_tokens: 20,
            output_tokens: 7,
            cost_micros: 1_000_000,
            error_code: None,
        },
    )
    .await;
    finish(
        &state,
        &same_tenant_foreign,
        UsageSample {
            model: "foreign-key-model",
            status_code: 503,
            duration_ms: 1_200,
            input_tokens: 900,
            cached_input_tokens: 100,
            cache_write_tokens: 50,
            output_tokens: 90,
            cost_micros: 9_000_000,
            error_code: Some("foreign_key_error"),
        },
    )
    .await;
    finish(
        &state,
        &other_tenant,
        UsageSample {
            model: "foreign-tenant-model",
            status_code: 500,
            duration_ms: 3_000,
            input_tokens: 800,
            cached_input_tokens: 80,
            cache_write_tokens: 40,
            output_tokens: 80,
            cost_micros: 8_000_000,
            error_code: Some("foreign_tenant_error"),
        },
    )
    .await;

    let now = memeloop_token_center::db::unix_millis();
    let pool = AnyPool::connect(&database_url).await.unwrap();
    seed_usage_activity(
        &pool,
        SeedUsageActivity {
            key: &viewer,
            currency: "USD",
            model: "self-image-model",
            created_at: now,
            status_class: "success",
            error_code: "",
            upstream_account_id: None,
            cost_micros: 3_000_000,
            kind: SeedUsageKind::Generation {
                units: 4,
                modality: "image",
                billing_unit: "job",
            },
        },
    )
    .await;
    pool.close().await;

    let from = now.saturating_sub(86_400_000);
    let to = memeloop_token_center::db::unix_millis().saturating_add(1_000);
    let path = format!(
        "/self/v1/usage-analysis?from_created_at={from}&to_created_at={to}&granularity=hour"
    );
    let (status, body) = get_json(&state, &path, &viewer_credential).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["summary"]["requests"], 2, "{body}");
    assert_eq!(body["summary"]["success"], 2, "{body}");
    assert_eq!(body["summary"]["failed"], 0, "{body}");
    assert_eq!(body["summary"]["input_tokens"], 50, "{body}");
    assert_eq!(body["summary"]["cached_input_tokens"], 30, "{body}");
    assert_eq!(body["summary"]["cache_write_tokens"], 20, "{body}");
    assert_eq!(body["summary"]["output_tokens"], 7, "{body}");
    assert_eq!(body["summary"]["generation_units"], 4, "{body}");
    assert_eq!(body["summary"]["avg_duration_ms"], 20.0, "{body}");
    assert_eq!(body["summary"]["p95_duration_ms"], 50, "{body}");
    assert_eq!(body["p95_is_approximate"], true, "{body}");
    assert_eq!(body["summary"]["costs"].as_array().unwrap().len(), 1);
    assert_eq!(body["summary"]["costs"][0]["currency"], "USD");
    assert_eq!(body["summary"]["costs"][0]["cost"], "4");
    assert_eq!(body["generation_units_by_modality"][0]["modality"], "image");
    assert_eq!(body["generation_units_by_modality"][0]["units"], 4);
    assert_eq!(
        body["generation_units_by_billing_unit"][0]["billing_unit"],
        "job"
    );
    assert_eq!(body["generation_units_by_billing_unit"][0]["units"], 4);
    assert!(
        body["by_model"]
            .as_array()
            .unwrap()
            .iter()
            .any(|bucket| bucket["id"] == "self-text-model")
    );
    assert!(
        body["by_protocol"]
            .as_array()
            .unwrap()
            .iter()
            .any(|bucket| bucket["id"] == "generation")
    );
    assert!(
        body["heatmap"]
            .as_array()
            .is_some_and(|rows| !rows.is_empty())
    );

    for forbidden in [
        "tenant_external_id",
        "key_id",
        "by_key",
        "key_alias",
        "principal",
        "route_id",
        "by_session",
        "upstream_account_id",
        "by_upstream",
        "upstream_grouping",
        "session_id",
    ] {
        assert!(body.get(forbidden).is_none(), "{forbidden} leaked: {body}");
    }
    for forbidden_value in [
        same_tenant_foreign.key_id.to_string(),
        same_tenant_foreign.tenant_id.to_string(),
        other_tenant.key_id.to_string(),
        "Foreign Key".to_owned(),
        "foreign-key-model".to_owned(),
        "foreign_key_error".to_owned(),
        "foreign-tenant-model".to_owned(),
        "foreign_tenant_error".to_owned(),
    ] {
        assert!(
            !body.to_string().contains(&forbidden_value),
            "foreign identity or usage leaked: {forbidden_value}: {body}"
        );
    }

    let (status, filtered) = get_json(
        &state,
        &format!("{path}&model=self-image-model&protocol=generation&status=success"),
        &viewer_credential,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{filtered}");
    assert_eq!(filtered["summary"]["requests"], 1, "{filtered}");
    assert_eq!(filtered["summary"]["generation_units"], 4, "{filtered}");

    for selector in [
        format!("key_id={}", same_tenant_foreign.key_id),
        format!("tenant_external_id={tenant}"),
        "key_alias=Foreign%20Key".to_owned(),
        "principal=Foreign%20Key".to_owned(),
        format!("route_id={}", Uuid::now_v7()),
        "upstream_account_id=unassigned".to_owned(),
        format!("session_id={}", Uuid::now_v7()),
    ] {
        let (status, rejected) = get_json(
            &state,
            &format!("/self/v1/usage-analysis?{selector}"),
            &viewer_credential,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{selector}: {rejected}");
    }

    let (status, foreign_body) = get_json(&state, &path, &foreign_credential).await;
    assert_eq!(status, StatusCode::OK, "{foreign_body}");
    assert_eq!(foreign_body["summary"]["requests"], 1, "{foreign_body}");
    assert_eq!(foreign_body["summary"]["costs"][0]["currency"], "CNY");
    assert!(!foreign_body.to_string().contains("self-text-model"));

    let (status, _) = get_json(&state, &path, "not-a-client-credential").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn sqlite_self_usage_analysis_is_stable_key_scoped_and_identity_safe() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("self-usage-analysis.db").display()
    );
    assert_self_usage_analysis_is_stable_key_scoped(database_url, "self-usage-sqlite").await;
}

#[tokio::test]
async fn postgres_self_usage_analysis_is_stable_key_scoped_and_identity_safe() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    assert_self_usage_analysis_is_stable_key_scoped(database_url, "self-usage-postgres").await;
}

#[tokio::test]
async fn sqlite_usage_analysis_keeps_currency_cache_scope_and_prefix_filters_exact() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("usage-analysis.db").display()
    );
    let mut config = Config::for_test(database_url);
    config.key_pepper = String::from_utf8(PEPPER.to_vec()).unwrap();
    let state = AppState::initialize(config).await.unwrap();

    let usd = issue(&state, "usage-tenant", "Alice-Usage", "Alpha-USD", "USD").await;
    let cny = issue(&state, "usage-tenant", "Bob-Usage", "Beta-CNY", "CNY").await;
    let other = issue(&state, "usage-other", "Other-Usage", "Other-USD", "USD").await;
    finish(
        &state,
        &usd,
        UsageSample {
            model: "analysis-model",
            status_code: 200,
            duration_ms: 20,
            input_tokens: 100,
            cached_input_tokens: 30,
            cache_write_tokens: 20,
            output_tokens: 7,
            cost_micros: 1_000_000,
            error_code: None,
        },
    )
    .await;
    finish(
        &state,
        &cny,
        UsageSample {
            model: "analysis-model",
            status_code: 502,
            duration_ms: 180,
            input_tokens: 80,
            cached_input_tokens: 10,
            cache_write_tokens: 5,
            output_tokens: 3,
            cost_micros: 2_000_000,
            error_code: Some("usage_failure"),
        },
    )
    .await;
    finish(
        &state,
        &other,
        UsageSample {
            model: "analysis-model",
            status_code: 200,
            duration_ms: 500,
            input_tokens: 999,
            cached_input_tokens: 0,
            cache_write_tokens: 0,
            output_tokens: 1,
            cost_micros: 9_000_000,
            error_code: None,
        },
    )
    .await;

    let scoped = state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: "usage-tenant-reader".to_owned(),
                scopes: vec!["requests:read".to_owned()],
                tenant_external_id: Some("usage-tenant".to_owned()),
            },
            PEPPER,
        )
        .await
        .unwrap();
    let now = memeloop_token_center::db::unix_millis();
    let path = format!(
        "/internal/v1/usage-analysis?from_created_at={}&to_created_at={now}&granularity=hour&protocol=openai&model=analysis-model",
        now.saturating_sub(86_400_000)
    );
    let (status, body) = get_json(&state, &path, &scoped.token).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["granularity"], "hour");
    assert_eq!(body["time_zone"], "UTC");
    assert_eq!(body["p95_is_approximate"], true);
    assert_eq!(body["upstream_grouping"], "stable_account");
    assert_eq!(body["summary"]["requests"], 2);
    assert_eq!(body["summary"]["success"], 1);
    assert_eq!(body["summary"]["failed"], 1);
    assert_eq!(body["summary"]["input_tokens"], 115);
    assert_eq!(body["summary"]["cached_input_tokens"], 40);
    assert_eq!(body["summary"]["cache_write_tokens"], 25);
    assert_eq!(body["summary"]["output_tokens"], 10);
    assert_eq!(body["summary"]["p95_duration_ms"], 250);
    assert_eq!(
        body["summary"]["costs"].as_array().unwrap().len(),
        2,
        "{body}"
    );
    assert_eq!(body["summary"]["costs"][0]["currency"], "CNY");
    assert_eq!(body["summary"]["costs"][0]["cost"], "2");
    assert_eq!(body["summary"]["costs"][1]["currency"], "USD");
    assert_eq!(body["summary"]["costs"][1]["cost"], "1");
    assert_eq!(body["by_key"].as_array().unwrap().len(), 2);
    let by_session = body["by_session"].as_array().unwrap();
    assert_eq!(by_session.len(), 2, "{body}");
    assert_eq!(
        by_session
            .iter()
            .map(|bucket| bucket["input_tokens"].as_i64().unwrap())
            .sum::<i64>(),
        115,
        "{body}"
    );
    assert_eq!(
        by_session
            .iter()
            .map(|bucket| bucket["cached_input_tokens"].as_i64().unwrap())
            .sum::<i64>(),
        40,
        "{body}"
    );
    assert_eq!(
        by_session
            .iter()
            .map(|bucket| bucket["cache_write_tokens"].as_i64().unwrap())
            .sum::<i64>(),
        25,
        "{body}"
    );
    assert!(
        body["by_status"]
            .as_array()
            .unwrap()
            .iter()
            .any(|bucket| bucket["id"] == "error" && bucket["requests"] == 1),
        "{body}"
    );
    assert!(
        body["heatmap"]
            .as_array()
            .is_some_and(|rows| !rows.is_empty())
    );
    let (status, errors_only) = get_json(
        &state,
        &format!(
            "/internal/v1/usage-analysis?from_created_at={}&to_created_at={now}&status=error",
            now.saturating_sub(86_400_000)
        ),
        &scoped.token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{errors_only}");
    assert_eq!(errors_only["summary"]["requests"], 1, "{errors_only}");
    let (status, unassigned_only) = get_json(
        &state,
        &format!(
            "/internal/v1/usage-analysis?from_created_at={}&to_created_at={now}&upstream_account_id=unassigned",
            now.saturating_sub(86_400_000)
        ),
        &scoped.token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{unassigned_only}");
    assert_eq!(
        unassigned_only["summary"]["requests"], 2,
        "{unassigned_only}"
    );
    let (status, daily) = get_json(
        &state,
        &format!(
            "/internal/v1/usage-analysis?from_created_at={}&to_created_at={now}&granularity=day&model=analysis-model",
            now.saturating_sub(7 * 86_400_000)
        ),
        &scoped.token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{daily}");
    assert_eq!(daily["granularity"], "day");
    assert_eq!(daily["summary"]["requests"], 2);
    assert_eq!(
        daily["time_series"][0]["bucket_start"].as_i64().unwrap() % 86_400_000,
        0
    );

    let (status, filtered) = get_json(
        &state,
        &format!(
            "/internal/v1/usage-analysis?from_created_at={}&to_created_at={now}&key_alias=alpha&principal=alice",
            now.saturating_sub(86_400_000)
        ),
        &scoped.token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{filtered}");
    assert_eq!(filtered["summary"]["requests"], 1);
    assert_eq!(filtered["by_key"][0]["id"], usd.key_id.to_string());
    assert_eq!(filtered["by_key"][0]["label"], "Alpha-USD");

    let (status, escaped) = get_json(
        &state,
        &format!(
            "/internal/v1/usage-analysis?from_created_at={}&to_created_at={now}&key_alias=%25&principal=_",
            now.saturating_sub(86_400_000)
        ),
        &scoped.token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{escaped}");
    assert_eq!(escaped["summary"]["requests"], 0);

    let (status, _) = get_json(
        &state,
        &format!(
            "/internal/v1/usage-analysis?tenant_external_id=usage-other&from_created_at={}&to_created_at={now}",
            now.saturating_sub(86_400_000)
        ),
        &scoped.token,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    for index in 0..101 {
        let error_code = format!("stable-error-{index:03}");
        finish(
            &state,
            &usd,
            UsageSample {
                model: "top-error-model",
                status_code: 500,
                duration_ms: 1,
                input_tokens: 0,
                cached_input_tokens: 0,
                cache_write_tokens: 0,
                output_tokens: 0,
                cost_micros: 0,
                error_code: Some(&error_code),
            },
        )
        .await;
    }
    let top_now = memeloop_token_center::db::unix_millis();
    let (status, top_errors) = get_json(
        &state,
        &format!(
            "/internal/v1/usage-analysis?from_created_at={}&to_created_at={top_now}&model=top-error-model",
            top_now.saturating_sub(86_400_000)
        ),
        &scoped.token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{top_errors}");
    assert_eq!(top_errors["errors"].as_array().unwrap().len(), 100);
    assert_eq!(top_errors["errors"][0]["id"], "stable-error-000");
    assert_eq!(top_errors["errors"][99]["id"], "stable-error-099");

    let too_old = now.saturating_sub(32 * 86_400_000);
    let (status, _) = get_json(
        &state,
        &format!(
            "/internal/v1/usage-analysis?from_created_at={too_old}&to_created_at={now}&granularity=hour"
        ),
        &scoped.token,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn sqlite_usage_analysis_uses_inclusive_exact_boundary_buckets() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory
            .path()
            .join("usage-analysis-boundary.db")
            .display()
    );
    let mut config = Config::for_test(database_url.clone());
    config.key_pepper = String::from_utf8(PEPPER.to_vec()).unwrap();
    let state = AppState::initialize(config).await.unwrap();
    let key = issue(
        &state,
        "usage-boundary-tenant",
        "Boundary-Principal",
        "Boundary-Key",
        "USD",
    )
    .await;

    let mut request_ids = Vec::new();
    for input_tokens in [1, 10, 100, 1_000] {
        request_ids.push(
            finish(
                &state,
                &key,
                UsageSample {
                    model: "boundary-model",
                    status_code: 200,
                    duration_ms: 20,
                    input_tokens,
                    cached_input_tokens: 0,
                    cache_write_tokens: 0,
                    output_tokens: 1,
                    cost_micros: input_tokens * 1_000_000,
                    error_code: None,
                },
            )
            .await,
        );
    }

    let now = memeloop_token_center::db::unix_millis();
    let hour_start = now.div_euclid(3_600_000) * 3_600_000;
    let from = hour_start + 15 * 60_000;
    let to = hour_start + 45 * 60_000;
    for (request_id, created_at) in request_ids.into_iter().zip([from - 1, from, to, to + 1]) {
        move_request_fact(&database_url, request_id, created_at).await;
    }

    let service = state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: "usage-boundary-reader".to_owned(),
                scopes: vec!["requests:read".to_owned()],
                tenant_external_id: Some("usage-boundary-tenant".to_owned()),
            },
            PEPPER,
        )
        .await
        .unwrap();
    let (status, body) = get_json(
        &state,
        &format!(
            "/internal/v1/usage-analysis?from_created_at={from}&to_created_at={to}&granularity=hour&model=boundary-model"
        ),
        &service.token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_exact_boundary_response(&body, hour_start);

    let (status, daily) = get_json(
        &state,
        &format!(
            "/internal/v1/usage-analysis?from_created_at={from}&to_created_at={to}&granularity=day&model=boundary-model"
        ),
        &service.token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{daily}");
    assert_exact_boundary_response(&daily, hour_start.div_euclid(86_400_000) * 86_400_000);
}

#[tokio::test]
async fn sqlite_usage_analysis_combines_edge_facts_and_interior_rollups_once() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("usage-multibucket.db").display()
    );
    assert_multibucket_usage_analysis(database_url, "usage-multibucket-sqlite".to_owned()).await;
}

#[tokio::test]
async fn postgres_usage_analysis_grouping_sets_match_currency_safe_contract() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let mut config = Config::for_test(database_url.clone());
    config.key_pepper = String::from_utf8(PEPPER.to_vec()).unwrap();
    let state = AppState::initialize(config).await.unwrap();
    let unique = Uuid::now_v7();
    let tenant = format!("usage-pg-{unique}");
    let usd = issue(&state, &tenant, "Pg-USD", "Pg-USD", "USD").await;
    let cny = issue(&state, &tenant, "Pg-CNY", "Pg-CNY", "CNY").await;
    finish(
        &state,
        &usd,
        UsageSample {
            model: "pg-analysis-model",
            status_code: 200,
            duration_ms: 40,
            input_tokens: 40,
            cached_input_tokens: 10,
            cache_write_tokens: 5,
            output_tokens: 3,
            cost_micros: 750_000,
            error_code: None,
        },
    )
    .await;
    finish(
        &state,
        &cny,
        UsageSample {
            model: "pg-analysis-model",
            status_code: 503,
            duration_ms: 1_200,
            input_tokens: 20,
            cached_input_tokens: 4,
            cache_write_tokens: 1,
            output_tokens: 2,
            cost_micros: 1_250_000,
            error_code: Some("pg_usage_failure"),
        },
    )
    .await;
    let service = state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: format!("usage-pg-reader-{unique}"),
                scopes: vec!["requests:read".to_owned()],
                tenant_external_id: Some(tenant),
            },
            PEPPER,
        )
        .await
        .unwrap();
    let now = memeloop_token_center::db::unix_millis();
    let (status, body) = get_json(
        &state,
        &format!(
            "/internal/v1/usage-analysis?from_created_at={}&to_created_at={now}&granularity=hour&protocol=openai&model=pg-analysis-model",
            now.saturating_sub(86_400_000)
        ),
        &service.token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["summary"]["requests"], 2);
    assert_eq!(body["summary"]["input_tokens"], 40);
    assert_eq!(body["summary"]["cached_input_tokens"], 14);
    assert_eq!(body["summary"]["cache_write_tokens"], 6);
    assert_eq!(body["summary"]["costs"].as_array().unwrap().len(), 2);
    assert_eq!(body["errors"][0]["id"], "pg_usage_failure");
    assert_eq!(body["by_key"].as_array().unwrap().len(), 2);

    let mut boundary_request_ids = Vec::new();
    for input_tokens in [1, 10, 100, 1_000] {
        boundary_request_ids.push(
            finish(
                &state,
                &usd,
                UsageSample {
                    model: "pg-boundary-model",
                    status_code: 200,
                    duration_ms: 20,
                    input_tokens,
                    cached_input_tokens: 0,
                    cache_write_tokens: 0,
                    output_tokens: 1,
                    cost_micros: input_tokens * 1_000_000,
                    error_code: None,
                },
            )
            .await,
        );
    }
    let boundary_now = memeloop_token_center::db::unix_millis();
    let hour_start = boundary_now.div_euclid(3_600_000) * 3_600_000;
    let from = hour_start + 15 * 60_000;
    let to = hour_start + 45 * 60_000;
    for (request_id, created_at) in
        boundary_request_ids
            .into_iter()
            .zip([from - 1, from, to, to + 1])
    {
        move_request_fact(&database_url, request_id, created_at).await;
    }
    let (status, boundary) = get_json(
        &state,
        &format!(
            "/internal/v1/usage-analysis?from_created_at={from}&to_created_at={to}&granularity=hour&model=pg-boundary-model"
        ),
        &service.token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{boundary}");
    assert_exact_boundary_response(&boundary, hour_start);

    assert_multibucket_usage_analysis(
        database_url,
        format!("usage-pg-multibucket-{}", Uuid::now_v7()),
    )
    .await;
}
