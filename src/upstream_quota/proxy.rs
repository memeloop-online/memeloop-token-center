use std::sync::atomic::{AtomicUsize, Ordering};

use super::{QuotaBudget, QuotaRequestContext, USAGE_URL};
use crate::db::{ProxySelection, egress::SelectedEgress};

struct Route {
    proxy_url: Option<String>,
    egress: Option<SelectedEgress>,
}

pub(super) struct Routes(Vec<Route>);

impl Routes {
    pub(super) async fn from_selection(
        mut selection: ProxySelection,
    ) -> Result<Self, &'static str> {
        let mut routes = Vec::new();
        let mut attempted = Vec::new();
        loop {
            crate::network::validate_codex_transport(
                USAGE_URL,
                &serde_json::json!({"network_scope": "public"}),
                selection.credential.proxy(),
                false,
            )
            .await
            .map_err(|_| "quota_destination_invalid")?;
            routes.push(Route {
                proxy_url: selection.credential.proxy().map(|(url, _)| url.to_owned()),
                egress: selection.egress_snapshot(),
            });
            let Some(member) = selection.member() else {
                break;
            };
            attempted.push(member);
            if !selection
                .select_unattempted(&attempted)
                .map_err(|_| "quota_transport_failed")?
            {
                break;
            }
        }
        Ok(Self(routes))
    }

    pub(super) fn proxy_url(&self, index: usize) -> Option<&str> {
        self.0[index].proxy_url.as_deref()
    }

    pub(super) fn advance_on_failure(&self, index: &AtomicUsize, error: &reqwest::Error) {
        self.advance(
            index,
            crate::simple_http::classify_transport(error).pre_delivery(),
        );
    }

    fn advance(&self, index: &AtomicUsize, connector_failure: bool) {
        if connector_failure {
            let _ = index.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                (current + 1 < self.0.len()).then_some(current + 1)
            });
        }
    }

    pub(super) fn log_attempt(
        &self,
        context: QuotaRequestContext,
        index: usize,
        attempt: usize,
        budget: QuotaBudget,
    ) {
        let route = &self.0[index];
        let parsed = route
            .proxy_url
            .as_deref()
            .and_then(|url| url::Url::parse(url).ok());
        tracing::info!(
            operation = "quota_supplier_read",
            upstream_account_id = %context.account_id,
            credential_generation = context.credential_generation,
            endpoint_kind = context.endpoint_kind,
            trigger = context.trigger.as_str(),
            attempt,
            proxy_member_id = ?route.egress.as_ref().and_then(|value| value.member_id),
            proxy_group_id = ?route.egress.as_ref().and_then(|value| value.group_id),
            proxy_host = parsed.as_ref().and_then(|url| url.host_str()).unwrap_or("direct"),
            proxy_port = parsed.as_ref().and_then(|url| url.port()),
            total_budget_ms = budget.total.as_millis() as u64,
            request_local_egress = true,
            "quota supplier egress selected"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{db::TransportProxyGroups, provider::UpstreamCredential};
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use uuid::Uuid;

    #[tokio::test]
    async fn quota_fallback_is_request_local_and_never_changes_shared_selection() {
        let account = Uuid::from_u128(4);
        let proxies = [
            "socks5h://100.64.0.4:1080",
            "socks5h://100.64.0.5:1080",
            "socks5h://100.64.0.6:1080",
        ];
        let groups = TransportProxyGroups::parse(
            &json!([{"account_id":account,"version":1,"proxies":proxies}]).to_string(),
            b"quota-routes-fixture",
        )
        .unwrap();
        let credential = UpstreamCredential::OAuth {
            access_token: "fixture-access".into(),
            refresh_token: Some("fixture-refresh".into()),
            expires_at: None,
            header: "authorization".into(),
            prefix: "Bearer ".into(),
            adapter_state: None,
            proxy_url: Some(proxies[0].into()),
            proxy_network_scope: Some(crate::network::OutboundScope::Private),
        };
        let selected = groups.select(account, 1, &credential).unwrap();
        let generation = selected.generation;
        let routes = Routes::from_selection(selected).await.unwrap();
        let usage = AtomicUsize::new(0);
        let credits = AtomicUsize::new(0);
        routes.advance(&usage, false);
        assert_eq!(usage.load(Ordering::Relaxed), 0);
        routes.advance(&usage, true);
        assert_eq!(
            routes.proxy_url(usage.load(Ordering::Relaxed)),
            Some(proxies[1])
        );
        assert_eq!(credits.load(Ordering::Relaxed), 0);
        routes.advance(&usage, true);
        routes.advance(&usage, true);
        assert_eq!(
            routes.proxy_url(usage.load(Ordering::Relaxed)),
            Some(proxies[2])
        );
        let unchanged = groups.select(account, 1, &credential).unwrap();
        assert_eq!(unchanged.member_index(), Some(0));
        assert_eq!(unchanged.generation, generation);
        assert_eq!(unchanged.credential.proxy().unwrap().0, proxies[0]);
    }

    async fn proxy_fixture(listener: tokio::net::TcpListener, reject_connect: bool, status: u16) {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut greeting = [0; 2];
        socket.read_exact(&mut greeting).await.unwrap();
        assert_eq!(greeting[0], 5);
        let mut methods = vec![0; greeting[1] as usize];
        socket.read_exact(&mut methods).await.unwrap();
        socket.write_all(&[5, 0]).await.unwrap();
        let mut connect = [0; 5];
        socket.read_exact(&mut connect).await.unwrap();
        assert_eq!(&connect[..4], &[5, 1, 0, 3]);
        let mut destination = vec![0; connect[4] as usize + 2];
        socket.read_exact(&mut destination).await.unwrap();
        socket
            .write_all(&[5, u8::from(reject_connect), 0, 1, 127, 0, 0, 1, 0, 80])
            .await
            .unwrap();
        if reject_connect {
            return;
        }
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            assert!(request.len() < 4096);
            request.push(socket.read_u8().await.unwrap());
        }
        let body = br#"{"plan_type":"pro"}"#;
        socket
            .write_all(
                format!(
                    "HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        socket.write_all(body).await.unwrap();
    }

    async fn read_through_fixture(reject_connect: bool, first_status: u16) {
        use super::super::{
            CodexQuotaAuth, QuotaReadTrigger, codex_quota_budget, codex_quota_transport_policy,
            get_codex_json_with_routes, retry::ReadSession,
        };
        let first = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let second = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let routes = Routes(
            [&first, &second]
                .into_iter()
                .map(|listener| Route {
                    proxy_url: Some(format!("socks5h://{}", listener.local_addr().unwrap())),
                    egress: None,
                })
                .collect(),
        );
        let first_task = tokio::spawn(proxy_fixture(first, reject_connect, first_status));
        let second_task = tokio::spawn(proxy_fixture(second, false, 200));
        let budget = codex_quota_budget(&json!({})).unwrap();
        let policy = codex_quota_transport_policy(&json!({}), budget).unwrap();
        let session = ReadSession::testing(budget);
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            get_codex_json_with_routes(
                policy,
                CodexQuotaAuth {
                    credential_header: http::header::AUTHORIZATION,
                    credential_value: http::HeaderValue::from_static("Bearer fixture"),
                    account: http::HeaderValue::from_static("fixture"),
                    proxy_url: None,
                },
                "http://quota.example.invalid/usage",
                QuotaRequestContext {
                    account_id: Uuid::from_u128(4),
                    credential_generation: 1,
                    endpoint_kind: "usage",
                    trigger: QuotaReadTrigger::Manual,
                },
                budget,
                &session,
                Some(&routes),
            ),
        )
        .await
        .unwrap();
        first_task.await.unwrap();
        if reject_connect {
            assert_eq!(result.unwrap()["plan_type"], "pro");
            assert_eq!(session.attempts().len(), 2);
            second_task.await.unwrap();
        } else {
            assert_eq!(result.unwrap_err(), "quota_not_authorized");
            assert_eq!(session.attempts().len(), 1);
            assert!(!second_task.is_finished());
            second_task.abort();
            assert!(second_task.await.unwrap_err().is_cancelled());
        }
    }

    #[tokio::test]
    async fn connector_failure_uses_backup_within_existing_retry_budget() {
        read_through_fixture(true, 200).await;
    }

    #[tokio::test]
    async fn authorization_rejection_never_uses_backup() {
        read_through_fixture(false, 403).await;
    }
}
