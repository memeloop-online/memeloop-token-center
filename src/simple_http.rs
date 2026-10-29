//! Shared outbound HTTP client. TLS uses rustls with the ring crypto provider.
//! There is no browser fingerprint, BoringSSL, or per-request replay.

use std::time::Duration;

use crate::provider::{CodexHttpProtocol, CodexTransportPolicy};

pub(crate) const CODEX_HTTP2_KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(30);

/// Install ring once. Later calls are ignored if a provider is already set.
pub(crate) fn install_ring_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[cfg(test)]
pub(crate) fn codex_http_client_builder(
    policy: CodexTransportPolicy,
    keep_alive_interval: Duration,
) -> reqwest::ClientBuilder {
    codex_http_client_builder_with_proxy(policy, keep_alive_interval, None)
        .expect("codex client without a proxy")
}

#[cfg(test)]
pub(crate) fn codex_http_client_builder_with_proxy(
    policy: CodexTransportPolicy,
    keep_alive_interval: Duration,
    proxy_url: Option<&str>,
) -> Result<reqwest::ClientBuilder, reqwest::Error> {
    let snapshot = crate::http_settings::HttpClientFactory::default().snapshot();
    configured_codex_builder(&snapshot, policy, keep_alive_interval, proxy_url)
}

pub(crate) fn configured_codex_builder(
    snapshot: &crate::http_settings::HttpSnapshot,
    policy: CodexTransportPolicy,
    keep_alive_interval: Duration,
    proxy_url: Option<&str>,
) -> Result<reqwest::ClientBuilder, reqwest::Error> {
    let mut builder = snapshot
        .builder(crate::http_settings::HttpPurpose::Codex)
        .redirect(reqwest::redirect::Policy::none())
        // Responses POSTs are not safe for an HTTP client's implicit retry.
        // Candidate failover remains explicit and pre-delivery in the proxy.
        .retry(reqwest::retry::never())
        .no_proxy()
        .tcp_keepalive(None::<Duration>)
        .tcp_keepalive_interval(None::<Duration>)
        .tcp_keepalive_retries(None::<u32>)
        .http2_keep_alive_interval(keep_alive_interval)
        .http2_keep_alive_timeout(Duration::from_millis(policy.request_timeout_millis))
        .http2_keep_alive_while_idle(false)
        .http2_initial_stream_window_size(6_291_456)
        .http2_max_header_list_size(262_144);
    if let Some(proxy_url) = proxy_url {
        builder = builder.proxy(reqwest::Proxy::all(proxy_url)?);
    }
    if policy.http_protocol == CodexHttpProtocol::Http1Only {
        builder = builder.http1_only();
    }
    Ok(builder)
}

#[cfg(test)]
pub(crate) fn build_codex_http_client_with_policy(
    policy: CodexTransportPolicy,
) -> Result<reqwest::Client, reqwest::Error> {
    build_codex_http_client_with_proxy(policy, None)
}

#[cfg(test)]
pub(crate) fn build_codex_http_client_with_proxy(
    policy: CodexTransportPolicy,
    proxy_url: Option<&str>,
) -> Result<reqwest::Client, reqwest::Error> {
    codex_http_client_builder_with_proxy(policy, CODEX_HTTP2_KEEP_ALIVE_INTERVAL, proxy_url)?
        .build()
}

#[cfg(test)]
pub(crate) fn build_codex_http_client() -> Result<reqwest::Client, reqwest::Error> {
    build_codex_http_client_with_policy(CodexTransportPolicy::default())
}

#[cfg(test)]
pub(crate) fn build_codex_http2_test_client(
    policy: CodexTransportPolicy,
    keep_alive_interval: Duration,
) -> Result<reqwest::Client, reqwest::Error> {
    codex_http_protocol_test_builder(policy, keep_alive_interval, http::Version::HTTP_2)?.build()
}

#[cfg(test)]
pub(crate) fn codex_http_protocol_test_builder(
    policy: CodexTransportPolicy,
    keep_alive_interval: Duration,
    version: http::Version,
) -> Result<reqwest::ClientBuilder, reqwest::Error> {
    let builder = codex_http_client_builder(policy, keep_alive_interval);
    Ok(match version {
        http::Version::HTTP_11 => builder.http1_only(),
        http::Version::HTTP_2 => builder.http2_prior_knowledge(),
        _ => panic!("unsupported synthetic protocol arm"),
    })
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct TransportClass {
    pub proxy_connect: bool,
    pub dns: bool,
    pub tls: bool,
    pub connect: bool,
    pub timeout: bool,
    pub connection_reset: bool,
}

impl TransportClass {
    pub(crate) fn pre_delivery(self) -> bool {
        self.proxy_connect || self.dns || self.tls || self.connect
    }

    pub(crate) fn stage(self) -> &'static str {
        if self.proxy_connect {
            "proxy_connect"
        } else if self.dns {
            "dns"
        } else if self.tls {
            "tls"
        } else if self.connect {
            "connect"
        } else if self.timeout {
            "timeout"
        } else if self.connection_reset {
            "connection_reset"
        } else {
            "transport"
        }
    }
}

pub(crate) fn classify_transport(error: &reqwest::Error) -> TransportClass {
    let mut class = TransportClass {
        proxy_connect: false,
        dns: false,
        tls: false,
        connect: error.is_connect(),
        timeout: error.is_timeout(),
        connection_reset: false,
    };
    let mut source = std::error::Error::source(error);
    while let Some(current) = source {
        let name = std::any::type_name_of_val(current);
        if name.contains("rustls") {
            class.tls = true;
        }
        if name.contains("tokio_socks") || name.contains("Socks") || name.contains("Proxy") {
            class.proxy_connect = true;
        }
        if name.contains("Dns")
            || name.contains("dns")
            || name.contains("Resolve")
            || name.contains("Gai")
        {
            class.dns = true;
        }
        if let Some(io_error) = current.downcast_ref::<std::io::Error>() {
            if io_error.kind() == std::io::ErrorKind::ConnectionReset {
                class.connection_reset = true;
            }
            if io_error.kind() == std::io::ErrorKind::TimedOut {
                class.timeout = true;
            }
        }
        source = current.source();
    }
    if class.proxy_connect || class.dns || class.tls {
        class.connect = true;
    }
    class
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    async fn stalled_tls_request(
        settings: crate::http_settings::HttpSettings,
    ) -> (
        tokio::task::JoinHandle<Result<reqwest::Response, reqwest::Error>>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("https://{}", listener.local_addr().unwrap());
        let (hello_sent, hello_received) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut hello = [0_u8; 4096];
            assert!(socket.read(&mut hello).await.unwrap() > 0);
            hello_sent.send(()).unwrap();
            std::future::pending::<()>().await;
            drop(socket);
        });
        let factory = crate::http_settings::HttpClientFactory::default();
        factory.publish(crate::http_settings::HttpSettingsDocument {
            revision: 1,
            settings,
        });
        let client = configured_codex_builder(
            &factory.snapshot(),
            CodexTransportPolicy {
                connect_timeout_millis: Some(1),
                read_timeout_millis: Some(1),
                ..CodexTransportPolicy::default()
            },
            CODEX_HTTP2_KEEP_ALIVE_INTERVAL,
            None,
        )
        .unwrap()
        .build()
        .unwrap();
        let request =
            tokio::spawn(async move { client.post(endpoint).body("one POST").send().await });
        hello_received.await.unwrap();
        (request, server)
    }

    #[tokio::test]
    async fn default_connection_survives_old_fixed_windows_and_can_be_cancelled() {
        let (request, server) = stalled_tls_request(Default::default()).await;
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(1_300)).await;
        tokio::task::yield_now().await;
        assert!(!request.is_finished());
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        server.abort();
    }

    #[tokio::test]
    async fn explicit_connection_window_still_bounds_tls_establishment() {
        let (request, server) = stalled_tls_request(
            serde_json::from_value(serde_json::json!({
                "defaults": {"connect_timeout_ms": 1000}
            }))
            .unwrap(),
        )
        .await;
        tokio::time::pause();
        tokio::time::advance(Duration::from_millis(1001)).await;
        assert!(request.await.unwrap().unwrap_err().is_timeout());
        server.abort();
    }

    #[tokio::test]
    async fn purpose_null_disables_global_and_legacy_connection_deadlines() {
        let (request, server) = stalled_tls_request(
            serde_json::from_value(serde_json::json!({
                "defaults": {"connect_timeout_ms": 1000, "read_timeout_ms": 1000},
                "purposes": {"codex": {"connect_timeout_ms": null, "read_timeout_ms": null}}
            }))
            .unwrap(),
        )
        .await;
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(1_300)).await;
        tokio::task::yield_now().await;
        assert!(!request.is_finished());
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        server.abort();
    }
}
