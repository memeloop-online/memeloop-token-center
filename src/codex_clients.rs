//! Bounded clients keyed by the immutable account transport snapshot.
use std::{collections::VecDeque, sync::Mutex};

mod dispatch;
pub(crate) use dispatch::{DispatchError, DispatchPermit};

use sha2::{Digest, Sha256};

use crate::provider::{CodexTransportPolicy, ResolvedUpstream};

#[derive(Hash, PartialEq, Eq)]
struct ClientKey {
    account_id: uuid::Uuid,
    revision: i64,
    proxy_fingerprint: [u8; 32],
    timeouts: [u64; 3],
}

impl ClientKey {
    fn new(route: &ResolvedUpstream, policy: CodexTransportPolicy) -> Self {
        Self::for_account(
            route.account_id,
            route.transport_revision,
            &route.credential,
            policy,
        )
    }

    fn for_account(
        account_id: uuid::Uuid,
        revision: i64,
        credential: &crate::provider::UpstreamCredential,
        policy: CodexTransportPolicy,
    ) -> Self {
        Self {
            account_id,
            revision,
            proxy_fingerprint: Sha256::digest(
                credential.proxy().map_or("", |(url, _)| url).as_bytes(),
            )
            .into(),
            timeouts: [
                policy.connect_timeout_millis,
                policy.read_timeout_millis,
                policy.request_timeout_millis,
            ],
        }
    }
}

#[derive(Default)]
pub(crate) struct CodexClients {
    clients: Mutex<VecDeque<(ClientKey, wreq::Client)>>,
    dispatch: dispatch::DispatchLanes,
}

impl CodexClients {
    pub(crate) async fn acquire_dispatch(
        &self,
        route: &ResolvedUpstream,
        metrics: &crate::metrics::Metrics,
    ) -> Result<DispatchPermit, DispatchError> {
        self.dispatch.acquire(route, metrics).await
    }

    pub(crate) fn snapshot(&self, route: &ResolvedUpstream) -> Result<wreq::Client, &'static str> {
        let policy = CodexTransportPolicy::parse(route.config.get("transport_policy"))?;
        let key = ClientKey::new(route, policy);
        self.client(key, policy)
    }

    /// Control-plane reads share the generation transport's TLS profile and cache.
    pub(crate) fn account_snapshot(
        &self,
        account: &crate::provider::UpstreamAccountView,
        credential: &crate::provider::UpstreamCredential,
    ) -> Result<wreq::Client, &'static str> {
        let policy = CodexTransportPolicy::parse(account.config.get("transport_policy"))?;
        self.client(
            ClientKey::for_account(account.id, account.updated_at, credential, policy),
            policy,
        )
    }

    fn client(
        &self,
        key: ClientKey,
        policy: CodexTransportPolicy,
    ) -> Result<wreq::Client, &'static str> {
        let mut clients = self
            .clients
            .lock()
            .map_err(|_| "transport_client_unavailable")?;
        if let Some(position) = clients.iter().position(|(cached, _)| cached == &key) {
            let entry = clients.remove(position).expect("cache position exists");
            let client = entry.1.clone();
            clients.push_back(entry);
            return Ok(client);
        }
        let client = crate::build_codex_http_client_with_policy(policy)
            .map_err(|_| "transport_client_unavailable")?;
        // Eviction never changes clients already held by in-flight requests.
        clients.retain(|(cached, _)| cached.account_id != key.account_id);
        if clients.len() >= 64 {
            clients.pop_front();
        }
        clients.push_back((key, client.clone()));
        Ok(client)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::UpstreamCredential;

    #[test]
    fn sixty_fifth_client_evicts_only_the_least_recently_used_entry() {
        let clients = CodexClients::default();
        let policy = CodexTransportPolicy::default();
        let key = |id| {
            ClientKey::for_account(
                uuid::Uuid::from_u128(id),
                1,
                &UpstreamCredential::None,
                policy,
            )
        };
        for id in 0..64 {
            clients.client(key(id), policy).unwrap();
        }
        clients.client(key(0), policy).unwrap();
        clients.client(key(64), policy).unwrap();
        let cache = clients.clients.lock().unwrap();
        assert_eq!(cache.len(), 64);
        assert!(cache.iter().any(|(cached, _)| cached == &key(0)));
        assert!(!cache.iter().any(|(cached, _)| cached == &key(1)));
        assert!(cache.iter().any(|(cached, _)| cached == &key(63)));
    }

    #[tokio::test]
    async fn socks_keepalive_reuses_one_handshake_for_two_complete_responses() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = format!("socks5h://{}", listener.local_addr().unwrap());
        let handshakes = Arc::new(AtomicUsize::new(0));
        let count = handshakes.clone();
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                count.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut greeting = [0; 2];
                    stream.read_exact(&mut greeting).await.unwrap();
                    let mut methods = vec![0; usize::from(greeting[1])];
                    stream.read_exact(&mut methods).await.unwrap();
                    stream.write_all(&[5, 0]).await.unwrap();
                    let mut connect = [0; 4];
                    stream.read_exact(&mut connect).await.unwrap();
                    assert_eq!(connect, [5, 1, 0, 3]);
                    let length = stream.read_u8().await.unwrap();
                    let mut destination = vec![0; usize::from(length) + 2];
                    stream.read_exact(&mut destination).await.unwrap();
                    stream
                        .write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0])
                        .await
                        .unwrap();
                    loop {
                        let mut headers = Vec::new();
                        while !headers.ends_with(b"\r\n\r\n") {
                            let Ok(byte) = stream.read_u8().await else {
                                return;
                            };
                            headers.push(byte);
                            assert!(headers.len() < 16384);
                        }
                        if stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: keep-alive\r\n\r\nok").await.is_err() { return; }
                    }
                });
            }
        });
        let clients = CodexClients::default();
        let route = ResolvedUpstream {
            route_id: uuid::Uuid::nil(),
            account_id: uuid::Uuid::nil(),
            transport_revision: 1,
            credential_generation: 1,
            driver: "openai-codex".into(),
            base_url: String::new(),
            config: serde_json::json!({}),
            upstream_model: String::new(),
            credential: UpstreamCredential::ProxiedApiKey {
                value: String::new(),
                header: "authorization".into(),
                prefix: String::new(),
                proxy_url: proxy.clone(),
                proxy_network_scope: crate::network::OutboundScope::Private,
            },
        };
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            for _ in 0..2 {
                let client = clients.snapshot(&route).unwrap();
                let response = client
                    .get("http://keepalive.example.test/")
                    .proxy(wreq::Proxy::all(&proxy).unwrap())
                    .send()
                    .await
                    .unwrap();
                assert_eq!(response.bytes().await.unwrap().as_ref(), b"ok");
            }
        })
        .await;
        server.abort();
        result.unwrap();
        assert_eq!(
            handshakes.load(Ordering::SeqCst),
            1,
            "cache snapshots and per-request SOCKS binding must preserve the idle connection pool"
        );
    }

    #[test]
    fn cache_separates_revisions_proxies_and_timeout_snapshots() {
        let mut route = ResolvedUpstream {
            route_id: uuid::Uuid::nil(),
            account_id: uuid::Uuid::nil(),
            transport_revision: 1,
            credential_generation: 1,
            driver: "openai-codex".into(),
            base_url: String::new(),
            config: serde_json::json!({}),
            upstream_model: String::new(),
            credential: UpstreamCredential::None,
        };
        let policy = CodexTransportPolicy::default();
        let first = ClientKey::new(&route, policy);
        assert!(
            first
                == ClientKey::for_account(
                    route.account_id,
                    route.transport_revision,
                    &route.credential,
                    policy
                ),
            "directory and generation share the same account client key"
        );
        assert!(first == ClientKey::new(&route, policy));
        route.transport_revision += 1;
        assert!(first != ClientKey::new(&route, policy));
        route.transport_revision -= 1;
        let changed = CodexTransportPolicy {
            connect_timeout_millis: 1000,
            ..policy
        };
        assert!(first != ClientKey::new(&route, changed));
        route.credential = UpstreamCredential::ProxiedApiKey {
            value: String::new(),
            header: "authorization".into(),
            prefix: String::new(),
            proxy_url: "socks5h://10.0.0.1:1080".into(),
            proxy_network_scope: crate::network::OutboundScope::Private,
        };
        assert!(first != ClientKey::new(&route, policy));
    }
}
