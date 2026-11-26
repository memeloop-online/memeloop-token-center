use super::*;
use crate::{
    api,
    config::{Config, RuntimeRole},
    db::{
        CreateKeyInput, CreateRoutedModelRouteInput, CreateUpstreamAccountInput,
        DiscoveredUpstreamModel,
    },
    model::KeyPolicy,
    provider::UpstreamCredential,
};
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use rust_decimal::Decimal;
use serde_json::json;
use tower::ServiceExt;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

#[tokio::test]
async fn oauth_rejection_delivery_crosses_gateway_control_and_worker_process_roles() {
    let directory = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let mut config = Config::for_test(format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("oauth-roles.db").display()
    ));
    config.pricing_models_dev_url = format!("{}/prices", server.uri());
    config.pricing_litellm_url = format!("{}/prices", server.uri());
    config.pricing_openrouter_url = format!("{}/prices", server.uri());
    let control = AppState::initialize_for_role(config.clone(), RuntimeRole::Control)
        .await
        .unwrap();
    let gateway = AppState::initialize_for_role(config.clone(), RuntimeRole::Gateway)
        .await
        .unwrap();
    let worker = AppState::initialize_for_role(config, RuntimeRole::Worker)
        .await
        .unwrap();
    assert!(!Arc::ptr_eq(
        &control.upstream_quota,
        &gateway.upstream_quota
    ));
    assert!(!Arc::ptr_eq(
        &control.upstream_quota,
        &worker.upstream_quota
    ));
    let tenant = "oauth-role-fixture";
    let mut ids = Vec::new();
    for (name, driver) in [
        ("gateway", "http-json"),
        ("control", crate::provider::antigravity::DRIVER),
    ] {
        Mock::given(method("POST"))
            .and(path(format!("/refresh-{name}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"accessToken":format!("new-{name}")})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let account = control.db.create_upstream_account(CreateUpstreamAccountInput {
            tenant_external_id: tenant.into(), name: name.into(), driver: driver.into(),
            config: json!({"base_url":server.uri(), "control_url":server.uri(), "project_id":"fixture", "network_scope":"private"}),
            credential: UpstreamCredential::OAuth {
                access_token: format!("old-{name}"), refresh_token: Some(format!("refresh-{name}")),
                expires_at: Some(crate::db::unix_millis()+86_400_000), header:"authorization".into(), prefix:"Bearer ".into(),
                adapter_state:None, proxy_url:None, proxy_network_scope:None,
            }, oauth_session_id:Some(Uuid::now_v7()), oauth_driver:Some("cursor".into()),
            oauth_refresh_url:Some(format!("{}/refresh-{name}",server.uri())),
        }, control.config.key_pepper.as_bytes()).await.unwrap();
        ids.push(account.id);
    }
    Mock::given(path("/v1internal:retrieveUserQuotaSummary"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    Mock::given(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_json(json!({"error":{"message":"fixture rejected"}})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":[]})))
        .mount(&server)
        .await;
    control
        .db
        .upsert_model_price("fixture-model", "USD", Decimal::ZERO, Decimal::ZERO)
        .await
        .unwrap();
    let key = control
        .db
        .create_key(
            CreateKeyInput {
                tenant_external_id: tenant.into(),
                principal_external_id: "fixture".into(),
                alias: "fixture".into(),
                currency: "USD".into(),
                policy: KeyPolicy {
                    allowed_models: vec!["fixture-model".into()],
                    ..KeyPolicy::default()
                },
                initial_balance: Decimal::TEN,
                idempotency_key: None,
            },
            control.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let lease = Uuid::now_v7();
    assert!(
        control
            .db
            .claim_upstream_model_catalog_sync(ids[0], tenant, 1, lease)
            .await
            .unwrap()
    );
    control
        .db
        .replace_upstream_model_catalog(
            ids[0],
            tenant,
            1,
            lease,
            "openai_v1",
            &[DiscoveredUpstreamModel {
                model_id: "fixture-model".into(),
                protocol: "openai".into(),
                context_window: None,
                reservation_token_bound: None,
                reservation_bound_source: None,
            }],
        )
        .await
        .unwrap();
    control
        .db
        .create_routed_model_route(CreateRoutedModelRouteInput {
            tenant_external_id: tenant.into(),
            public_model: "fixture-model".into(),
            upstream_model: "fixture-model".into(),
            protocol: "openai".into(),
            priority: 0,
            enabled: true,
            upstream_account_ids: vec![ids[0]],
            included_provider_group_ids: vec![],
            excluded_provider_group_ids: vec![],
            route_group_ids: vec![],
            route_group_names: vec![],
            granted_credential_ids: vec![key.key_id],
            custom_model_confirmed: false,
        })
        .await
        .unwrap();
    let (stop, shutdown) = watch::channel(false);
    // Same role lifecycle as serve, with distinct process-local state. No test
    // calls flush and no gateway/control role runs the OAuth refresh worker.
    let gateway_task = tokio::spawn(run_for_role_until_shutdown(
        gateway.clone(),
        RuntimeRole::Gateway,
        shutdown.clone(),
        shutdown.clone(),
    ));
    let control_task = tokio::spawn(run_for_role_until_shutdown(
        control.clone(),
        RuntimeRole::Control,
        shutdown.clone(),
        shutdown.clone(),
    ));
    let response=tokio::time::timeout(Duration::from_secs(2),api::router_for_role(gateway,RuntimeRole::Gateway).oneshot(
        Request::post("/v1/chat/completions").header(header::AUTHORIZATION,format!("Bearer {}",key.key)).header(header::CONTENT_TYPE,"application/json")
            .body(Body::from(json!({"model":"fixture-model","max_tokens":64,"messages":[{"role":"user","content":"fixture"}]}).to_string())).unwrap()
    )).await.unwrap().unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let response = tokio::time::timeout(
        Duration::from_secs(2),
        api::router_for_role(control.clone(), RuntimeRole::Control).oneshot(
            Request::get(format!(
                "/internal/v1/upstreams/{}/quota?tenant_external_id={tenant}&fresh=true",
                ids[1]
            ))
            .header(header::AUTHORIZATION, "Bearer test-service-token")
            .body(Body::empty())
            .unwrap(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
            .unwrap();
    assert_eq!(body["error_code"], "quota_not_authorized");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let candidates = worker
                .db
                .list_managed_oauth_refresh_candidates(crate::db::unix_millis(), 20)
                .await
                .unwrap();
            if candidates.len() == 2 && ids.iter().all(|id| candidates.contains(&(*id, 1))) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("both producer roles must publish without a manual flush");
    let worker_task = tokio::spawn(run_for_role_until_shutdown(
        worker.clone(),
        RuntimeRole::Worker,
        shutdown.clone(),
        shutdown,
    ));
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let mut done = true;
            for id in &ids {
                done &= worker
                    .db
                    .upstream_account_with_credential(*id, worker.config.key_pepper.as_bytes())
                    .await
                    .unwrap()
                    .0
                    .credential_generation
                    == 2;
            }
            if done {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("worker alone rotates each rejected generation once");
    stop.send(true).unwrap();
    gateway_task.await.unwrap();
    control_task.await.unwrap();
    worker_task.await.unwrap();
    server.verify().await;
}
