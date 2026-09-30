use super::*;

#[tokio::test]
async fn interrupted_exchange_is_not_replayed_after_worker_restart() {
    let directory = tempfile::tempdir().unwrap();
    let database = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("device-restart.db").display()
    ))
    .await
    .unwrap();
    database.migrate().await.unwrap();
    let now = crate::db::unix_millis();
    let key = b"device-restart-fixture-key-at-least-32-bytes";
    let session_id = Uuid::now_v7();
    let state = CodexDeviceLoginState {
        session_id,
        tenant_external_id: "restart-fixture".into(),
        account_name: "Synthetic".into(),
        provider_config: json!({}),
        proxy_url: None,
        operator_service_id: None,
        device_auth_id: "synthetic-device".into(),
        user_code: "FIXTURE".into(),
        poll_interval_seconds: 1,
        not_before: now,
        expires_at: now + 60_000,
        reauthorize: None,
        device_token: Some(DeviceTokenResponse {
            authorization_code: "synthetic-code".into(),
            code_verifier: "synthetic-verifier".into(),
            code_challenge: "synthetic-challenge".into(),
        }),
        exchange_dispatched: true,
        issued_token: None,
    };
    database
        .begin_oauth_login_session(BeginOAuthLoginSession {
            session_id,
            flow_kind: OAUTH_DRIVER.into(),
            tenant_external_id: state.tenant_external_id.clone(),
            operator_service_id: None,
            state_ciphertext: seal_private_json(&state, key, STATE_AAD).unwrap(),
            next_poll_at: now,
            expires_at: state.expires_at,
        })
        .await
        .unwrap();
    let scope = CodexDevicePollScope {
        required_tenant: Some("restart-fixture"),
        operator_service_id: None,
    };
    let token = recover_codex_device_session_token(&database, session_id, key, scope)
        .await
        .unwrap();
    let server = wiremock::MockServer::start().await;
    let result = poll_codex_device_login_at(
        &crate::build_http_client().unwrap(),
        &database,
        &token,
        CodexDevicePollRuntime {
            key_material: key,
            now,
            scope,
            allow_test_loopback: true,
            endpoints: &CodexDeviceEndpoints::for_test(&server.uri()),
        },
    )
    .await;
    assert!(matches!(result, Err(AppError::Conflict(_))));
    assert!(server.received_requests().await.unwrap().is_empty());
    assert!(
        database
            .due_codex_login_sessions(now + 1000, 16)
            .await
            .unwrap()
            .is_empty()
    );
}
