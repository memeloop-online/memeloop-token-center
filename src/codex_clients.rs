//! Bounded clients keyed by the immutable account transport snapshot.
use std::{collections::HashMap, sync::Mutex};

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
        Self {
            account_id: route.account_id,
            revision: route.transport_revision,
            proxy_fingerprint: Sha256::digest(
                route
                    .credential
                    .proxy()
                    .map_or("", |(url, _)| url)
                    .as_bytes(),
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
    clients: Mutex<HashMap<ClientKey, wreq::Client>>,
}

impl CodexClients {
    pub(crate) fn snapshot(&self, route: &ResolvedUpstream) -> Result<wreq::Client, &'static str> {
        let policy = CodexTransportPolicy::parse(route.config.get("transport_policy"))?;
        let key = ClientKey::new(route, policy);
        let mut clients = self
            .clients
            .lock()
            .map_err(|_| "transport_client_unavailable")?;
        if let Some(client) = clients.get(&key) {
            return Ok(client.clone());
        }
        let client = crate::build_codex_http_client_with_policy(policy)
            .map_err(|_| "transport_client_unavailable")?;
        // Eviction never changes clients already held by in-flight requests.
        clients.retain(|cached, _| cached.account_id != key.account_id);
        if clients.len() >= 64 {
            clients.clear();
        }
        clients.insert(key, client.clone());
        Ok(client)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::UpstreamCredential;

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
