use super::*;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Instant,
};

const CAPACITY: usize = 256;
const TTL: Duration = Duration::from_secs(1);
const INPUT_LIMIT: usize = 4096;

struct Entry {
    identity: [u8; 32],
    expires: Instant,
    value: Option<(Uuid, Uuid)>,
    observed: Option<(i64, Uuid)>,
    version: Uuid,
}

pub(super) fn identity(
    key: &AuthenticatedKey,
    session: &str,
    model: &str,
    protocol: &str,
) -> [u8; 32] {
    let mut digest = blake3::Hasher::new_derive_key("session preference cache v1");
    for part in [
        key.tenant_id.as_bytes().as_slice(),
        key.principal_id.as_bytes(),
        key.key_id.as_bytes(),
        &key.credential_generation.to_le_bytes(),
        session.as_bytes(),
        model.as_bytes(),
        protocol.as_bytes(),
    ] {
        digest.update(&(part.len() as u64).to_le_bytes());
        digest.update(part);
    }
    *digest.finalize().as_bytes()
}

#[derive(Default)]
pub(crate) struct SessionPreferences {
    entries: Mutex<VecDeque<Entry>>,
}

impl SessionPreferences {
    pub(crate) fn invalidate(&self, state: &AppState, identity: [u8; 32]) {
        if let Ok(mut entries) = self.entries.try_lock() {
            entries.retain(|entry| entry.identity != identity);
        } else {
            state.routing_persistence.record_capacity_gap();
        }
    }

    pub(crate) fn observe(
        &self,
        state: &AppState,
        input: crate::db::SessionRoutingTerminalInput<'_>,
    ) {
        let identity = identity(
            input.key,
            input.explicit_session_id,
            input.model,
            input.protocol,
        );
        let Ok(mut entries) = self.entries.try_lock() else {
            state.routing_persistence.record_capacity_gap();
            return;
        };
        let observed = (input.observed_at, input.request_id);
        if entries.iter().any(|entry| {
            entry.identity == identity && entry.observed.is_some_and(|previous| previous > observed)
        }) {
            return;
        }
        entries.retain(|entry| entry.identity != identity && entry.expires > Instant::now());
        if entries.len() == CAPACITY {
            entries.pop_front();
        }
        let value = crate::db::is_session_avoid_terminal(input.status_code, input.error_code)
            .then_some(input.model_route_id.zip(input.upstream_account_id))
            .flatten();
        entries.push_back(Entry {
            identity,
            expires: Instant::now() + TTL,
            value,
            observed: Some(observed),
            version: Uuid::new_v4(),
        });
    }

    pub(crate) fn lookup(
        self: &Arc<Self>,
        state: &AppState,
        key: &AuthenticatedKey,
        request_id: Uuid,
        session: &str,
        model: &str,
        protocol: &str,
    ) -> Option<(Uuid, Uuid)> {
        if session
            .len()
            .saturating_add(model.len())
            .saturating_add(protocol.len())
            > INPUT_LIMIT
        {
            state.routing_persistence.record_capacity_gap();
            return None;
        }
        let identity = identity(key, session, model, protocol);
        let Ok(mut entries) = self.entries.try_lock() else {
            state.routing_persistence.record_capacity_gap();
            return None;
        };
        let now = Instant::now();
        entries.retain(|entry| entry.expires > now);
        if let Some(entry) = entries.iter().find(|entry| entry.identity == identity) {
            return entry.value;
        }
        if entries.len() == CAPACITY {
            entries.pop_front();
        }
        let expires = now + TTL;
        let version = Uuid::new_v4();
        entries.push_back(Entry {
            identity,
            expires,
            value: None,
            observed: None,
            version,
        });
        drop(entries);
        let cache = self.clone();
        let database = state.routing_persistence_db.clone();
        let key_identity = (key.tenant_id, key.principal_id, key.key_id);
        let session = session.to_owned();
        let model = model.to_owned();
        let protocol = protocol.to_owned();
        state.routing_persistence.submit(INPUT_LIMIT + 4096, async move {
            let value = tokio::time::timeout(Duration::from_millis(250), database.latest_session_transport_route_to_avoid_for_identity(key_identity, &session, &model, &protocol))
                .await.map_err(|_| AppError::Internal)??;
            let mut entries = cache.entries.lock().map_err(|_| AppError::Internal)?;
            if let Some(entry) = entries.iter_mut().find(|entry| entry.identity == identity && entry.version == version) {
                entry.value = value;
            }
            tracing::debug!(%request_id, stage = "session_preference_refresh", "optional session preference refreshed");
            Ok(())
        });
        None
    }
}
