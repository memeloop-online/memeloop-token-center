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
    selection_generation: i64,
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
            selection_generation: 0,
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
    clients: Mutex<VecDeque<(ClientKey, CachedClient)>>,
    dispatch: dispatch::DispatchLanes,
    #[cfg(test)]
    test_client: Mutex<Option<wreq::Client>>,
}

#[derive(Clone)]
struct CachedClient {
    client: wreq::Client,
    instance_id: uuid::Uuid,
}

pub(crate) struct ClientSnapshot {
    pub(crate) client: wreq::Client,
    pub(crate) instance_id: uuid::Uuid,
    pub(crate) cache_hit: bool,
}

impl CodexClients {
    pub(crate) async fn acquire_dispatch(
        &self,
        route: &ResolvedUpstream,
        metrics: &crate::metrics::Metrics,
    ) -> Result<DispatchPermit, DispatchError> {
        self.dispatch.acquire(route, metrics).await
    }

    pub(crate) fn transport_snapshot(
        &self,
        route: &ResolvedUpstream,
        selection_generation: i64,
    ) -> Result<wreq::Client, &'static str> {
        self.transport_snapshot_with_diagnostics(route, selection_generation)
            .map(|snapshot| snapshot.client)
    }

    pub(crate) fn transport_snapshot_with_diagnostics(
        &self,
        route: &ResolvedUpstream,
        selection_generation: i64,
    ) -> Result<ClientSnapshot, &'static str> {
        #[cfg(test)]
        if let Some(client) = self
            .test_client
            .lock()
            .map_err(|_| "transport_client_unavailable")?
            .as_ref()
        {
            return Ok(ClientSnapshot {
                client: client.clone(),
                instance_id: uuid::Uuid::nil(),
                cache_hit: false,
            });
        }
        let policy = CodexTransportPolicy::parse(route.config.get("transport_policy"))?;
        let mut key = ClientKey::new(route, policy);
        key.selection_generation = selection_generation;
        self.client_snapshot(key, policy)
    }

    #[cfg(test)]
    pub(crate) fn install_test_client(&self, client: wreq::Client) {
        *self.test_client.lock().unwrap() = Some(client);
    }

    /// Control-plane reads share the generation transport's TLS profile and cache.
    pub(crate) fn account_transport_snapshot(
        &self,
        account: &crate::provider::UpstreamAccountView,
        credential: &crate::provider::UpstreamCredential,
        selection_generation: i64,
    ) -> Result<wreq::Client, &'static str> {
        let policy = CodexTransportPolicy::parse(account.config.get("transport_policy"))?;
        let mut key = ClientKey::for_account(account.id, account.updated_at, credential, policy);
        key.selection_generation = selection_generation;
        self.client(key, policy)
    }

    fn client(
        &self,
        key: ClientKey,
        policy: CodexTransportPolicy,
    ) -> Result<wreq::Client, &'static str> {
        self.client_snapshot(key, policy)
            .map(|snapshot| snapshot.client)
    }

    fn client_snapshot(
        &self,
        key: ClientKey,
        policy: CodexTransportPolicy,
    ) -> Result<ClientSnapshot, &'static str> {
        let mut clients = self
            .clients
            .lock()
            .map_err(|_| "transport_client_unavailable")?;
        if let Some(position) = clients.iter().position(|(cached, _)| cached == &key) {
            let entry = clients.remove(position).expect("cache position exists");
            let snapshot = ClientSnapshot {
                client: entry.1.client.clone(),
                instance_id: entry.1.instance_id,
                cache_hit: true,
            };
            clients.push_back(entry);
            return Ok(snapshot);
        }
        let client = crate::build_codex_http_client_with_policy(policy)
            .map_err(|_| "transport_client_unavailable")?;
        // Eviction never changes clients already held by in-flight requests.
        clients.retain(|(cached, _)| cached.account_id != key.account_id);
        if clients.len() >= 64 {
            clients.pop_front();
        }
        let instance_id = uuid::Uuid::now_v7();
        clients.push_back((
            key,
            CachedClient {
                client: client.clone(),
                instance_id,
            },
        ));
        Ok(ClientSnapshot {
            client,
            instance_id,
            cache_hit: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::UpstreamCredential;

    #[tokio::test]
    async fn http2_reset_never_replays_post_direct_or_through_socks5h() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        for use_proxy in [false, true] {
            for reason in [http2::Reason::CANCEL, http2::Reason::REFUSED_STREAM] {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let address = listener.local_addr().unwrap();
                let connections = Arc::new(AtomicUsize::new(0));
                let requests = Arc::new(AtomicUsize::new(0));
                let connection_count = connections.clone();
                let request_count = requests.clone();
                let server = tokio::spawn(async move {
                    let mut handlers = tokio::task::JoinSet::new();
                    loop {
                        let (mut socket, _) = listener.accept().await.unwrap();
                        connection_count.fetch_add(1, Ordering::SeqCst);
                        let request_count = request_count.clone();
                        handlers.spawn(async move {
                            if use_proxy {
                                let mut greeting = [0; 2];
                                socket.read_exact(&mut greeting).await.unwrap();
                                assert_eq!(greeting[0], 5);
                                let mut methods = vec![0; usize::from(greeting[1])];
                                socket.read_exact(&mut methods).await.unwrap();
                                socket.write_all(&[5, 0]).await.unwrap();
                                let mut connect = [0; 4];
                                socket.read_exact(&mut connect).await.unwrap();
                                assert_eq!(connect, [5, 1, 0, 3]);
                                let length = socket.read_u8().await.unwrap();
                                let mut destination = vec![0; usize::from(length)];
                                socket.read_exact(&mut destination).await.unwrap();
                                assert_eq!(destination, b"reset.example.test");
                                assert_eq!(socket.read_u16().await.unwrap(), 80);
                                socket
                                    .write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0])
                                    .await
                                    .unwrap();
                            }
                            let mut connection = http2::server::handshake(socket).await.unwrap();
                            while let Some(Ok((request, mut response))) = connection.accept().await
                            {
                                assert_eq!(request.method(), http::Method::POST);
                                request_count.fetch_add(1, Ordering::SeqCst);
                                response.send_reset(reason);
                            }
                        });
                    }
                });
                let client =
                    crate::build_codex_http_client_with_policy(CodexTransportPolicy::default())
                        .unwrap();
                let target = if use_proxy {
                    "http://reset.example.test/v1/responses".to_owned()
                } else {
                    format!("http://{address}/v1/responses")
                };
                let mut request = client
                    .post(target)
                    .version(http::Version::HTTP_2)
                    .body("synthetic generation");
                if use_proxy {
                    request =
                        request.proxy(wreq::Proxy::all(format!("socks5h://{address}")).unwrap());
                }
                let result =
                    tokio::time::timeout(std::time::Duration::from_secs(5), request.send()).await;
                server.abort();
                let error = result.expect("reset must terminate promptly").unwrap_err();
                let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&error);
                let mut observed = None;
                while let Some(current) = source {
                    if let Some(http2) = current.downcast_ref::<http2::Error>() {
                        observed = Some((http2.is_reset(), http2.is_remote(), http2.reason()));
                        break;
                    }
                    source = current.source();
                }
                assert_eq!(observed, Some((true, true, Some(reason))));
                assert_eq!(connections.load(Ordering::SeqCst), 1);
                assert_eq!(requests.load(Ordering::SeqCst), 1);
            }
        }
    }

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
                let client = clients.transport_snapshot(&route, 0).unwrap();
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
    fn diagnostic_client_identity_tracks_cache_reuse_and_selected_proxy() {
        let clients = CodexClients::default();
        let mut route = ResolvedUpstream {
            route_id: uuid::Uuid::nil(),
            account_id: uuid::Uuid::nil(),
            transport_revision: 1,
            credential_generation: 1,
            driver: "openai-codex".into(),
            base_url: String::new(),
            config: serde_json::json!({}),
            upstream_model: String::new(),
            credential: UpstreamCredential::ProxiedApiKey {
                value: "synthetic-secret".into(),
                header: "authorization".into(),
                prefix: String::new(),
                proxy_url: "socks5h://user:password@10.0.0.1:1080".into(),
                proxy_network_scope: crate::network::OutboundScope::Private,
            },
        };
        let first = clients
            .transport_snapshot_with_diagnostics(&route, 1)
            .unwrap();
        let reused = clients
            .transport_snapshot_with_diagnostics(&route, 1)
            .unwrap();
        assert!(!first.cache_hit);
        assert!(reused.cache_hit);
        assert!(!first.instance_id.is_nil());
        assert_eq!(first.instance_id, reused.instance_id);
        route.credential = route
            .credential
            .with_transport_proxy("socks5h://user:password@10.0.0.2:1080".into())
            .unwrap();
        let changed_proxy = clients
            .transport_snapshot_with_diagnostics(&route, 1)
            .unwrap();
        assert!(!changed_proxy.cache_hit);
        assert_ne!(first.instance_id, changed_proxy.instance_id);
        let changed_epoch = clients
            .transport_snapshot_with_diagnostics(&route, 2)
            .unwrap();
        assert!(!changed_epoch.cache_hit);
        assert_ne!(changed_proxy.instance_id, changed_epoch.instance_id);
        let same_epoch = clients
            .transport_snapshot_with_diagnostics(&route, 2)
            .unwrap();
        assert!(same_epoch.cache_hit);
        assert_eq!(changed_epoch.instance_id, same_epoch.instance_id);
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
        let mut changed_selection = ClientKey::new(&route, policy);
        changed_selection.selection_generation = 2;
        assert!(first != changed_selection);
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
