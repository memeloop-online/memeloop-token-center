use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use tokio::sync::Semaphore;
use uuid::Uuid;

use crate::{AppState, db::SessionRoutingTerminalInput, model::AuthenticatedKey};

const ENTRIES: usize = 1024;
const TEXT_LIMIT: usize = 1024;
const TTL: Duration = Duration::from_secs(1);
type CacheKey = [u8; 32];
type Route = (Uuid, Uuid);

struct Entry {
    value: Option<Route>,
    expires: Instant,
    version: Uuid,
    observed: (i64, Uuid),
}

pub(crate) struct SessionCache {
    entries: Mutex<HashMap<CacheKey, Entry>>,
    refreshes: Arc<Semaphore>,
    rejected: AtomicU64,
    failed: AtomicU64,
}

impl Default for SessionCache {
    fn default() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            refreshes: Arc::new(Semaphore::new(4)),
            rejected: AtomicU64::new(0),
            failed: AtomicU64::new(0),
        }
    }
}

pub(crate) fn cache_key(
    key: &AuthenticatedKey,
    session: &str,
    model: &str,
    protocol: &str,
) -> Option<CacheKey> {
    if [session, model, protocol]
        .iter()
        .any(|text| text.len() > TEXT_LIMIT)
    {
        return None;
    }
    let mut digest = blake3::Hasher::new_derive_key("mtc optional session preference v1");
    for identity in [key.tenant_id, key.principal_id, key.key_id] {
        digest.update(identity.as_bytes());
    }
    for text in [session, model, protocol] {
        digest.update(&(text.len() as u64).to_le_bytes());
        digest.update(text.as_bytes());
    }
    Some(*digest.finalize().as_bytes())
}

impl SessionCache {
    pub(crate) fn invalidate(&self, identity: CacheKey) {
        if let Ok(mut entries) = self.entries.try_lock() {
            entries.remove(&identity);
        }
    }
    pub(crate) fn lookup(
        state: &AppState,
        key: &AuthenticatedKey,
        session: &str,
        model: &str,
        protocol: &str,
    ) -> Option<Route> {
        let cache = &state.observations.sessions;
        let identity = cache_key(key, session, model, protocol)?;
        let Ok(mut entries) = cache.entries.try_lock() else {
            return None;
        };
        let now = Instant::now();
        if let Some(entry) = entries.get(&identity)
            && entry.expires > now
        {
            return entry.value;
        }
        let Ok(permit) = cache.refreshes.clone().try_acquire_owned() else {
            cache.rejected.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        if entries.len() >= ENTRIES && !entries.contains_key(&identity) {
            entries.retain(|_, entry| entry.expires > now);
            if entries.len() >= ENTRIES {
                cache.rejected.fetch_add(1, Ordering::Relaxed);
                return None;
            }
        }
        let version = Uuid::new_v4();
        entries.insert(
            identity,
            Entry {
                value: None,
                expires: now + TTL,
                version,
                observed: (0, Uuid::nil()),
            },
        );
        drop(entries);
        let database = state.observation_db.clone();
        let observations = state.observations.clone();
        let scope = (key.tenant_id, key.principal_id, key.key_id);
        let (session, model, protocol) =
            (session.to_owned(), model.to_owned(), protocol.to_owned());
        tokio::spawn(async move {
            let _permit = permit;
            let result = tokio::time::timeout(
                Duration::from_millis(50),
                database.latest_session_transport_route_to_avoid_scoped(
                    scope, &session, &model, &protocol,
                ),
            )
            .await;
            let cache = &observations.sessions;
            match result {
                Ok(Ok(value)) => {
                    if let Ok(mut entries) = cache.entries.lock()
                        && let Some(entry) = entries.get_mut(&identity)
                        && entry.version == version
                    {
                        entry.value = value;
                        entry.expires = Instant::now() + TTL;
                    }
                }
                _ => {
                    cache.failed.fetch_add(1, Ordering::Relaxed);
                }
            }
        });
        None
    }

    pub(crate) fn publish(&self, input: SessionRoutingTerminalInput<'_>) {
        let Some(identity) = cache_key(
            input.key,
            input.explicit_session_id,
            input.model,
            input.protocol,
        ) else {
            return;
        };
        let Ok(mut entries) = self.entries.try_lock() else {
            return;
        };
        let now = Instant::now();
        if entries.len() >= ENTRIES && !entries.contains_key(&identity) {
            entries.retain(|_, entry| entry.expires > now);
            if entries.len() >= ENTRIES {
                return;
            }
        }
        let observed = (input.observed_at, input.request_id);
        if entries
            .get(&identity)
            .is_some_and(|entry| entry.observed > observed)
        {
            return;
        }
        let value = crate::db::is_session_avoid_terminal(input.status_code, input.error_code)
            .then(|| input.model_route_id.zip(input.upstream_account_id))
            .flatten();
        entries.insert(
            identity,
            Entry {
                value,
                expires: now + TTL,
                version: Uuid::new_v4(),
                observed,
            },
        );
    }

    pub(crate) fn render(&self) -> String {
        format!(
            "# TYPE memeloop_token_center_session_preference_refresh_jobs gauge\nmemeloop_token_center_session_preference_refresh_jobs {}\n# TYPE memeloop_token_center_session_preference_rejected_total counter\nmemeloop_token_center_session_preference_rejected_total {}\n# TYPE memeloop_token_center_session_preference_failed_total counter\nmemeloop_token_center_session_preference_failed_total {}\n",
            4 - self.refreshes.available_permits(),
            self.rejected.load(Ordering::Relaxed),
            self.failed.load(Ordering::Relaxed)
        )
    }
}
