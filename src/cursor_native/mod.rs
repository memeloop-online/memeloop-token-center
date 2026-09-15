//! Native read-only Cursor OAuth Connect RPCs, not an OpenAI or CLI bridge.
//! Protocol provenance and intentionally unsupported inference: docs/cursor-native-protocol.md.

pub(crate) mod models;
pub(crate) mod proto;

use crate::{AppState, network, provider::UpstreamCredential};
use futures_util::StreamExt;
use std::time::Duration;

pub(crate) const DRIVER: &str = "cursor";
const BACKEND: &str = "https://api2.cursor.sh";
pub(crate) const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Clone, Copy)]
pub(crate) enum Method {
    GetUsableModels,
    GetCurrentPeriodUsage,
    GetPlanInfo,
}

impl Method {
    fn path(self) -> &'static str {
        match self {
            Self::GetUsableModels => "/aiserver.v1.AiService/GetUsableModels",
            Self::GetCurrentPeriodUsage => "/aiserver.v1.DashboardService/GetCurrentPeriodUsage",
            Self::GetPlanInfo => "/aiserver.v1.DashboardService/GetPlanInfo",
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
    let now = chrono::Utc::now().timestamp_millis();
    credential.validate(now).map_err(|_| "credential_invalid")?;
    crate::oauth::cursor_account_id(credential).map_err(|_| "credential_invalid")?;
    let UpstreamCredential::OAuth { access_token, .. } = credential else {
        return Err("credential_invalid");
    };
    // The deadline includes DNS/proxy setup, headers and the entire body.
    tokio::time::timeout(TIMEOUT, async {
        let client = if credential.proxy().is_some() {
            // Match OAuth's approved SOCKS5H boundary: do not resolve the
            // fixed supplier hostname locally when the proxy owns DNS.
            network::client_for_oauth_url_no_retry(&state.http, BACKEND, credential.proxy(), false)
                .await
        } else {
            network::client_for_config_url_no_retry(
                &state.http,
                BACKEND,
                &serde_json::json!({"network_scope":"public"}),
                None,
                false,
            )
            .await
        }
        .map_err(|_| "destination_invalid")?;
        send(
            &client,
            &format!("{BACKEND}{}", method.path()),
            access_token,
        )
        .await
    })
    .await
    .map_err(|_| "connection_failed")?
}

async fn send(
    client: &reqwest::Client,
    url: &str,
    access_token: &str,
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
        .timeout(TIMEOUT)
        // Empty protobuf: no custom model IDs or team override.
        .body(Vec::new())
        .send()
        .await
        .map_err(|_| "connection_failed")?;
    let status = response.status();
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
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err("response_too_large");
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| "connection_failed")?;
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err("response_too_large");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_bytes, header, method, path},
    };

    #[tokio::test]
    async fn binary_unary_contract_and_redacted_errors() {
        let server = MockServer::start().await;
        let path_value = Method::GetUsableModels.path();
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
    async fn response_size_is_bounded_with_and_without_content_length() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let client = crate::build_no_retry_http_client(None, &[]).unwrap();
        for chunked in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/catalog", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0; 4096];
                let _ = socket.read(&mut request).await;
                if chunked {
                    socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/proto\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
                    let bytes = vec![0_u8; MAX_RESPONSE_BYTES + 1];
                    socket
                        .write_all(format!("{:x}\r\n", bytes.len()).as_bytes())
                        .await
                        .unwrap();
                    // The receiver may close immediately on reaching the cap.
                    let _ = socket.write_all(&bytes).await;
                    let _ = socket.write_all(b"\r\n0\r\n\r\n").await;
                } else {
                    socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/proto\r\nContent-Length: {}\r\n\r\n", MAX_RESPONSE_BYTES + 1).as_bytes()).await.unwrap();
                }
            });
            assert_eq!(
                send(&client, &url, "fixture-token").await,
                Err("response_too_large")
            );
            server.await.unwrap();
        }
    }
}
