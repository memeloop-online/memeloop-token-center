use super::oauth::poll_codex_oauth_for_worker;
use crate::{
    api::tests::test_state,
    db::{
        BeginOAuthLoginSession, CreateUpstreamAccountInput, OAuthLoginClaim,
        OAuthLoginSessionReference, unix_millis,
    },
    oauth::{
        OAuthReauthorizationTarget,
        codex_device::{
            CodexDevicePollScope, ReadyCodexDeviceLogin, recover_codex_device_session_token,
        },
    },
    provider::{UpstreamCredential, seal_private_json},
};
use serde_json::json;
use uuid::Uuid;

fn credential(value: &str) -> UpstreamCredential {
    UpstreamCredential::OAuth {
        access_token: value.into(),
        refresh_token: Some("synthetic-refresh".into()),
        expires_at: Some(unix_millis() + 3_600_000),
        header: "authorization".into(),
        prefix: "Bearer ".into(),
        adapter_state: Some(
            json!({"schema":"openai-codex-oauth-v1","account_id":"synthetic-identity"}),
        ),
        proxy_url: None,
        proxy_network_scope: None,
    }
}

#[tokio::test]
async fn worker_recovers_staged_login_after_browser_loss_and_expiry_without_creating_another_account()
 {
    staged_login_recovery(false).await;
}

#[tokio::test]
async fn worker_reports_stale_account_conflict_without_replacing_or_creating_an_account() {
    staged_login_recovery(true).await;
}

async fn staged_login_recovery(stale_account: bool) {
    let (state, _directory) = test_state().await;
    let key = state.config.key_pepper.as_bytes();
    let config = json!({"base_url":"https://chatgpt.com/backend-api/codex","network_scope":"public","reservation_token_bounds":{}});
    let original = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: "device-worker-fixture".into(),
                name: "Synthetic account".into(),
                driver: "openai-codex".into(),
                config: config.clone(),
                credential: credential("synthetic-old"),
                oauth_session_id: Some(Uuid::now_v7()),
                oauth_driver: Some("openai_codex_device".into()),
                oauth_refresh_url: Some("https://auth.openai.com/oauth/token".into()),
            },
            key,
        )
        .await
        .unwrap();
    let original = state
        .db
        .set_upstream_account_status(
            original.id,
            "device-worker-fixture",
            "disabled",
            original.updated_at,
        )
        .await
        .unwrap();
    let now = unix_millis();
    let session_id = Uuid::now_v7();
    let reference = OAuthLoginSessionReference {
        session_id,
        flow_kind: "openai_codex_device".into(),
        tenant_external_id: "device-worker-fixture".into(),
        operator_service_id: None,
        expires_at: if stale_account {
            now + 60_000
        } else {
            now - 1000
        },
    };
    let encrypted = seal_private_json(&json!({
        "session_id":session_id,"tenant_external_id":reference.tenant_external_id,"account_name":"Synthetic account",
        "provider_config":config,"operator_service_id":null,"device_auth_id":"synthetic-device","user_code":"FIXTURE",
        "poll_interval_seconds":1,"not_before":now-5000,"expires_at":reference.expires_at,"reauthorize":null,
    }), key, b"memeloop-token-center/openai-codex-device-state/v1").unwrap();
    state
        .db
        .begin_oauth_login_session(BeginOAuthLoginSession {
            session_id,
            flow_kind: reference.flow_kind.clone(),
            tenant_external_id: reference.tenant_external_id.clone(),
            operator_service_id: None,
            state_ciphertext: encrypted,
            next_poll_at: now - 5000,
            expires_at: reference.expires_at,
        })
        .await
        .unwrap();
    let claim = state
        .db
        .claim_codex_login_poll(&reference, now - 4000, 1)
        .await
        .unwrap();
    let OAuthLoginClaim::Claimed { lease_owner, .. } = claim else {
        panic!("expected claim")
    };
    let ready = ReadyCodexDeviceLogin {
        session_id,
        tenant_external_id: reference.tenant_external_id.clone(),
        account_name: original.name.clone(),
        provider_config: config,
        credential: credential("synthetic-replacement"),
        reauthorize: Some(OAuthReauthorizationTarget {
            account_id: original.id,
            expected_updated_at: original.updated_at,
            expected_credential_generation: original.credential_generation,
        }),
    };
    if stale_account {
        let active = state
            .db
            .set_upstream_account_status(
                original.id,
                "device-worker-fixture",
                "active",
                original.updated_at,
            )
            .await
            .unwrap();
        state
            .db
            .set_upstream_account_status(
                original.id,
                "device-worker-fixture",
                "disabled",
                active.updated_at,
            )
            .await
            .unwrap();
    }
    state
        .db
        .stage_oauth_login_ready(
            session_id,
            lease_owner,
            seal_private_json(
                &ready,
                key,
                b"memeloop-token-center/openai-codex-device-ready/v1",
            )
            .unwrap(),
            now - 3000,
        )
        .await
        .unwrap();
    if stale_account {
        let result = poll_codex_oauth_for_worker(&state, session_id).await;
        assert!(
            matches!(result, Err(crate::error::AppError::Conflict(_))),
            "unexpected stale worker result: {result:?}"
        );
        assert!(
            state
                .db
                .codex_login_progress(&reference, now)
                .await
                .is_err()
        );
        assert!(
            state
                .db
                .due_codex_login_sessions(now, 16)
                .await
                .unwrap()
                .is_empty()
        );
        let accounts = state
            .db
            .list_upstream_accounts("device-worker-fixture")
            .await
            .unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].id, original.id);
        assert_eq!(
            accounts[0].credential_generation,
            original.credential_generation
        );
        return;
    }
    let observe = || async {
        let headers = axum::http::HeaderMap::from_iter([(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_str(&format!("Bearer {}", state.config.service_token))
                .unwrap(),
        )]);
        super::oauth::poll_codex_oauth(
            axum::extract::State(state.clone()),
            headers,
            axum::Json(serde_json::from_value(json!({"session_id":session_id})).unwrap()),
        )
        .await
        .unwrap()
    };
    assert_eq!(observe().await.status(), axum::http::StatusCode::ACCEPTED);
    assert_eq!(
        state.db.due_codex_login_sessions(now, 16).await.unwrap(),
        vec![session_id]
    );
    let (first, second) = tokio::join!(
        poll_codex_oauth_for_worker(&state, session_id),
        poll_codex_oauth_for_worker(&state, session_id)
    );
    first.unwrap();
    second.unwrap();
    poll_codex_oauth_for_worker(&state, session_id)
        .await
        .unwrap();
    let observed = observe().await;
    assert_eq!(observed.status(), axum::http::StatusCode::OK);
    let body = axum::body::to_bytes(observed.into_body(), 1024 * 1024)
        .await
        .unwrap();
    assert!(!String::from_utf8_lossy(&body).contains("synthetic-replacement"));
    let accounts = state
        .db
        .list_upstream_accounts("device-worker-fixture")
        .await
        .unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].id, original.id);
    assert_eq!(
        accounts[0].credential_generation,
        original.credential_generation + 1
    );
    assert_eq!(accounts[0].status, "disabled");
    assert!(
        state
            .db
            .due_codex_login_sessions(now, 16)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        recover_codex_device_session_token(
            &state.db,
            session_id,
            key,
            CodexDevicePollScope {
                required_tenant: Some("other-tenant"),
                operator_service_id: None
            }
        )
        .await
        .is_err()
    );
    assert!(
        recover_codex_device_session_token(
            &state.db,
            session_id,
            key,
            CodexDevicePollScope {
                required_tenant: None,
                operator_service_id: Some(Uuid::now_v7())
            }
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn expired_pending_login_is_not_selected_or_restarted_by_worker() {
    let (state, _directory) = test_state().await;
    let now = unix_millis();
    let reference = OAuthLoginSessionReference {
        session_id: Uuid::now_v7(),
        flow_kind: "openai_codex_device".into(),
        tenant_external_id: "expired-fixture".into(),
        operator_service_id: None,
        expires_at: now - 1000,
    };
    state
        .db
        .begin_oauth_login_session(BeginOAuthLoginSession {
            session_id: reference.session_id,
            flow_kind: reference.flow_kind.clone(),
            tenant_external_id: reference.tenant_external_id.clone(),
            operator_service_id: None,
            state_ciphertext: "unused-encrypted-fixture".into(),
            next_poll_at: now - 200_000,
            expires_at: reference.expires_at,
        })
        .await
        .unwrap();
    assert!(matches!(
        state
            .db
            .claim_codex_login_poll(&reference, now - 100_000, 1)
            .await
            .unwrap(),
        OAuthLoginClaim::Claimed { .. }
    ));
    assert!(
        state
            .db
            .due_codex_login_sessions(now, 16)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        state
            .db
            .claim_codex_login_poll(&reference, now, 1)
            .await
            .is_err()
    );
    assert!(
        state
            .db
            .list_all_upstream_accounts()
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn background_device_login_stops_when_the_initiating_service_is_revoked() {
    let (state, _directory) = test_state().await;
    let issued = state
        .db
        .create_service_token(
            crate::db::CreateServiceTokenInput {
                name: "Synthetic device worker operator".into(),
                scopes: vec!["oauth:write".into()],
                tenant_external_id: None,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let reference = OAuthLoginSessionReference {
        session_id: Uuid::now_v7(),
        flow_kind: "openai_codex_device".into(),
        tenant_external_id: "synthetic-scope".into(),
        operator_service_id: Some(issued.service_id),
        expires_at: unix_millis() + 60_000,
    };
    assert!(
        state
            .db
            .oauth_login_worker_authority(&reference)
            .await
            .unwrap()
            .allows("oauth:write")
    );
    state
        .db
        .set_service_token_status(issued.service_id, "revoked")
        .await
        .unwrap();
    assert!(
        state
            .db
            .oauth_login_worker_authority(&reference)
            .await
            .is_err()
    );
}
