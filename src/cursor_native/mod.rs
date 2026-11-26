//! Native read-only Cursor OAuth Connect RPCs, not an OpenAI or CLI bridge.
//! Protocol provenance and intentionally unsupported inference: docs/cursor-native-protocol.md.

pub(crate) mod models;
pub(crate) mod proto;

use crate::{AppState, network, provider::UpstreamCredential};

pub(crate) const DRIVER: &str = "cursor";
const BACKEND: &str = "https://api2.cursor.sh";

#[derive(Clone, Copy)]
pub(crate) enum Method {
    UsableModels,
    CurrentPeriodUsage,
    PlanInfo,
}

impl Method {
    fn path(self) -> &'static str {
        match self {
            Self::UsableModels => "/aiserver.v1.AiService/GetUsableModels",
            Self::CurrentPeriodUsage => "/aiserver.v1.DashboardService/GetCurrentPeriodUsage",
            Self::PlanInfo => "/aiserver.v1.DashboardService/GetPlanInfo",
        }
    }
}

/// Only fixed, read-only methods. No account-configured endpoint/headers, no
/// refresh, retry, server-config redirect, CLI process, or paid inference.
pub(crate) async fn unary(
    state: &AppState,
    credential: &UpstreamCredential,
    method: Method,
) -> Result<Vec<u8>, &'static str> {
    unary_with_rejection(state, credential, method, None).await
}

/// Only the main usage RPC can supply quota-triggered renewal evidence.
/// Optional plan/model RPCs retain the existing read-only error contract.
pub(crate) async fn quota_usage(
    state: &AppState,
    credential: &UpstreamCredential,
    account: &crate::provider::UpstreamAccountView,
    trigger: crate::upstream_quota::QuotaReadTrigger,
) -> Result<Vec<u8>, &'static str> {
    unary_with_rejection(
        state,
        credential,
        Method::CurrentPeriodUsage,
        quota_rejection(account.id, account.credential_generation, trigger),
    )
    .await
}

fn quota_rejection(
    account: uuid::Uuid,
    generation: i64,
    trigger: crate::upstream_quota::QuotaReadTrigger,
) -> Option<(uuid::Uuid, i64)> {
    (trigger != crate::upstream_quota::QuotaReadTrigger::PostRenewalVerification)
        .then_some((account, generation))
}

async fn unary_with_rejection(
    state: &AppState,
    credential: &UpstreamCredential,
    method: Method,
    rejection: Option<(uuid::Uuid, i64)>,
) -> Result<Vec<u8>, &'static str> {
    let now = chrono::Utc::now().timestamp_millis();
    credential.validate(now).map_err(|_| "credential_invalid")?;
    crate::oauth::cursor_account_id(credential).map_err(|_| "credential_invalid")?;
    let UpstreamCredential::OAuth { access_token, .. } = credential else {
        return Err("credential_invalid");
    };
    let client =
        network::client_for_oauth_url_no_retry(&state.http, BACKEND, credential.proxy(), false)
            .await
            .map_err(|_| "destination_invalid")?;
    send_with_rejection(
        &client,
        &format!("{BACKEND}{}", method.path()),
        access_token,
        rejection.map(|(id, generation)| (state, id, generation)),
    )
    .await
}

#[cfg(test)]
async fn send(
    client: &reqwest::Client,
    url: &str,
    access_token: &str,
) -> Result<Vec<u8>, &'static str> {
    send_with_rejection(client, url, access_token, None).await
}

async fn send_with_rejection(
    client: &reqwest::Client,
    url: &str,
    access_token: &str,
    rejection: Option<(&AppState, uuid::Uuid, i64)>,
) -> Result<Vec<u8>, &'static str> {
    let mut authorization =
        reqwest::header::HeaderValue::from_str(&format!("Bearer {access_token}"))
            .map_err(|_| "credential_invalid")?;
    authorization.set_sensitive(true);
    let response = client
        .post(url)
        .version(reqwest::Version::HTTP_11)
        .header(reqwest::header::AUTHORIZATION, authorization)
        .header(reqwest::header::CONTENT_TYPE, "application/proto")
        .header(reqwest::header::ACCEPT, "application/proto")
        .header("connect-protocol-version", "1")
        .header("x-ghost-mode", "true")
        .header("x-cursor-client-version", "cli-2026.07.23-e383d2b")
        .header("x-cursor-client-type", "cli")
        .header("x-request-id", uuid::Uuid::now_v7().to_string())
        // Empty protobuf: no custom model IDs or team override.
        .body(Vec::new())
        .send()
        .await
        .map_err(|_| "connection_failed")?;
    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED
        && let Some((state, account_id, generation)) = rejection
    {
        state
            .upstream_quota
            .oauth_rejections
            .submit(account_id, generation);
    }
    if status.is_redirection() {
        return Err("redirect_rejected");
    }
    if matches!(status.as_u16(), 401 | 403) {
        return Err("authentication_failed");
    }
    if status.as_u16() == 429 {
        return Err("rate_limited");
    }
    if !status.is_success() {
        return Err("upstream_unavailable");
    }
    if response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        != Some("application/proto")
    {
        return Err("invalid_response");
    }
    response
        .bytes()
        .await
        .map(|body| body.to_vec())
        .map_err(|_| "connection_failed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_bytes, header, method, path},
    };

    #[tokio::test]
    async fn cursor_usage_rejection_distinguishes_verification_and_optional_reads() {
        use crate::upstream_quota::QuotaReadTrigger;
        let directory = tempfile::tempdir().unwrap();
        let state = AppState::initialize(crate::config::Config::for_test(format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("cursor-rejection.db").display()
        )))
        .await
        .unwrap();
        let account = state
            .db
            .create_upstream_account(
                crate::db::CreateUpstreamAccountInput {
                    tenant_external_id: "cursor-rejection".into(),
                    name: "fixture".into(),
                    driver: "cursor".into(),
                    config: serde_json::json!({}),
                    credential: UpstreamCredential::OAuth {
                        access_token: "fixture-token".into(),
                        refresh_token: Some("fixture-refresh".into()),
                        expires_at: Some(crate::db::unix_millis() + 86_400_000),
                        header: "authorization".into(),
                        prefix: "Bearer ".into(),
                        adapter_state: None,
                        proxy_url: None,
                        proxy_network_scope: None,
                    },
                    oauth_session_id: Some(uuid::Uuid::now_v7()),
                    oauth_driver: Some("cursor".into()),
                    oauth_refresh_url: Some("https://refresh.example.test".into()),
                },
                state.config.key_pepper.as_bytes(),
            )
            .await
            .unwrap();
        let server = MockServer::start().await;
        for status in [401, 403] {
            Mock::given(path(format!("/{status}")))
                .respond_with(ResponseTemplate::new(status))
                .mount(&server)
                .await;
        }
        let client = crate::build_no_retry_http_client(None, &[]).unwrap();
        for (status, evidence) in [
            (
                403,
                quota_rejection(account.id, 1, QuotaReadTrigger::Manual),
            ),
            (
                401,
                quota_rejection(account.id, 1, QuotaReadTrigger::PostRenewalVerification),
            ),
            (401, None), // Optional plan/model RPCs never carry account evidence.
        ] {
            assert_eq!(
                send_with_rejection(
                    &client,
                    &format!("{}/{status}", server.uri()),
                    "fixture-token",
                    evidence.map(|(id, generation)| (&state, id, generation))
                )
                .await,
                Err("authentication_failed")
            );
            state.upstream_quota.oauth_rejections.flush(&state).await;
            assert!(
                !state
                    .db
                    .has_rejected_oauth_generation(account.id, 1)
                    .await
                    .unwrap()
            );
        }
        let evidence = quota_rejection(account.id, 1, QuotaReadTrigger::BackgroundRecovery);
        let mut blocked = state.db.begin_write_transaction().await.unwrap();
        sqlx::query("UPDATE upstream_oauth_rejected_generations SET observed_at=observed_at")
            .execute(&mut *blocked)
            .await
            .unwrap();
        // The consumer is paused/SQL is locked: raw supplier rejection still
        // returns immediately, and duplicate same-generation signals coalesce.
        for _ in 0..3 {
            assert_eq!(
                tokio::time::timeout(
                    std::time::Duration::from_secs(1),
                    send_with_rejection(
                        &client,
                        &format!("{}/401", server.uri()),
                        "fixture-token",
                        evidence.map(|(id, generation)| (&state, id, generation))
                    )
                )
                .await
                .expect("supplier 401 must not wait for evidence SQL"),
                Err("authentication_failed")
            );
        }
        state.upstream_quota.oauth_rejections.flush(&state).await;
        assert!(
            !state
                .db
                .has_rejected_oauth_generation(account.id, 1)
                .await
                .unwrap()
        );
        blocked.rollback().await.unwrap();
        state.upstream_quota.oauth_rejections.flush(&state).await;
        assert_eq!(
            state
                .db
                .list_managed_oauth_refresh_candidates(crate::db::unix_millis(), 20)
                .await
                .unwrap(),
            vec![(account.id, 1)]
        );
        state.upstream_quota.oauth_rejections.flush(&state).await;
        assert!(
            state
                .db
                .has_rejected_oauth_generation(account.id, 1)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn binary_unary_contract_and_redacted_errors() {
        let server = MockServer::start().await;
        let path_value = Method::UsableModels.path();
        Mock::given(method("POST"))
            .and(path(path_value))
            .and(header("authorization", "Bearer fixture-token"))
            .and(header("content-type", "application/proto"))
            .and(header("connect-protocol-version", "1"))
            .and(header("x-ghost-mode", "true"))
            .and(body_bytes(Vec::<u8>::new()))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(vec![10, 3, 10, 1, b'x'], "application/proto"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let client = crate::build_no_retry_http_client(None, &[]).unwrap();
        assert_eq!(
            send(
                &client,
                &format!("{}{path_value}", server.uri()),
                "fixture-token"
            )
            .await
            .unwrap(),
            [10, 3, 10, 1, b'x']
        );
        for (status, expected) in [
            (302, "redirect_rejected"),
            (401, "authentication_failed"),
            (403, "authentication_failed"),
            (429, "rate_limited"),
            (500, "upstream_unavailable"),
        ] {
            Mock::given(path(format!("/status/{status}")))
                .respond_with(
                    ResponseTemplate::new(status)
                        .insert_header("location", "http://secret.invalid/credential")
                        .set_body_string("private supplier detail"),
                )
                .mount(&server)
                .await;
            assert_eq!(
                send(
                    &client,
                    &format!("{}/status/{status}", server.uri()),
                    "fixture-token"
                )
                .await,
                Err(expected)
            );
        }
        Mock::given(path("/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"data":[]})))
            .mount(&server)
            .await;
        assert_eq!(
            send(&client, &format!("{}/json", server.uri()), "fixture-token").await,
            Err("invalid_response")
        );
    }

    #[tokio::test]
    async fn http_body_has_no_fixed_size_cap_with_or_without_content_length() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let client = crate::build_no_retry_http_client(None, &[]).unwrap();
        for chunked in [false, true] {
            let size = 2 * 1024 * 1024 + 1;
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/catalog", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0; 4096];
                assert!(socket.read(&mut request).await.unwrap() > 0);
                let bytes = vec![0_u8; size];
                if chunked {
                    socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/proto\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
                    socket
                        .write_all(format!("{:x}\r\n", bytes.len()).as_bytes())
                        .await
                        .unwrap();
                    socket.write_all(&bytes).await.unwrap();
                    socket.write_all(b"\r\n0\r\n\r\n").await.unwrap();
                } else {
                    socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/proto\r\nContent-Length: {size}\r\n\r\n").as_bytes()).await.unwrap();
                    socket.write_all(&bytes).await.unwrap();
                }
            });
            assert_eq!(
                send(&client, &url, "fixture-token").await.unwrap().len(),
                size
            );
            server.await.unwrap();
        }
    }
}
