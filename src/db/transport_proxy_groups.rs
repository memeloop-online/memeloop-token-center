use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use super::*;

mod persistence;
#[cfg(test)]
mod tests;

const MAX_EPOCH: u64 = (1 << 28) - 1;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Group {
    account_id: Uuid,
    version: i64,
    proxies: Vec<String>,
}

struct Entry {
    group: Group,
    fingerprint: String,
    state: AtomicU64,
    persisted: AtomicU64,
    durable: AtomicU64,
    observed_generation: AtomicU64,
    blocked: AtomicBool,
}

#[derive(Clone, Copy)]
struct Snapshot {
    credential_generation: u32,
    epoch: u64,
    base: usize,
    selected: usize,
}

impl Snapshot {
    fn decode(value: u64) -> Self {
        Self {
            credential_generation: (value >> 32) as u32,
            epoch: (value >> 4) & MAX_EPOCH,
            base: ((value >> 2) & 3) as usize,
            selected: (value & 3) as usize,
        }
    }

    fn encode(self) -> Result<u64, AppError> {
        if self.credential_generation == 0
            || self.epoch == 0
            || self.epoch > MAX_EPOCH
            || self.base > 3
            || self.selected > 3
        {
            return Err(unavailable());
        }
        Ok((u64::from(self.credential_generation) << 32)
            | (self.epoch << 4)
            | ((self.base as u64) << 2)
            | self.selected as u64)
    }
}

pub(crate) struct TransportProxyGroups {
    groups: BTreeMap<Uuid, Arc<Entry>>,
}

pub(crate) struct ProxySelection {
    pub(crate) credential: UpstreamCredential,
    pub(crate) generation: i64,
    ticket: Option<(Arc<Entry>, u64)>,
}

fn unavailable() -> AppError {
    AppError::Conflict("account transport proxy selection is unavailable".into())
}

impl TransportProxyGroups {
    pub(crate) fn parse(input: &str, key: &[u8]) -> Result<Self, AppError> {
        if input.len() > 256 * 1024 {
            return Err(unavailable());
        }
        let values: Vec<Group> = serde_json::from_str(input).map_err(|_| unavailable())?;
        if values.len() > 256 {
            return Err(unavailable());
        }
        let mut groups = BTreeMap::new();
        for group in values {
            if group.version <= 0 || !(1..=4).contains(&group.proxies.len()) {
                return Err(unavailable());
            }
            for (index, proxy) in group.proxies.iter().enumerate() {
                crate::provider::validate_codex_proxy_url(proxy).map_err(|_| unavailable())?;
                if group.proxies[..index].contains(proxy) {
                    return Err(unavailable());
                }
            }
            let mut digest = Sha256::new();
            digest.update(b"mtc/transport-proxy-group/v1\0");
            digest.update(key);
            digest.update(serde_json::to_vec(&group).map_err(|_| unavailable())?);
            let fingerprint = format!("{:x}", digest.finalize());
            let account_id = group.account_id;
            let entry = Arc::new(Entry {
                group,
                fingerprint,
                state: AtomicU64::new(0),
                persisted: AtomicU64::new(0),
                durable: AtomicU64::new(0),
                observed_generation: AtomicU64::new(0),
                blocked: AtomicBool::new(false),
            });
            if groups.insert(account_id, entry).is_some() {
                return Err(unavailable());
            }
        }
        Ok(Self { groups })
    }

    pub(crate) fn select(
        &self,
        account_id: Uuid,
        credential_generation: i64,
        credential: &UpstreamCredential,
    ) -> Result<ProxySelection, AppError> {
        let Some(entry) = self.groups.get(&account_id) else {
            return Ok(ProxySelection {
                credential: credential.clone(),
                generation: 0,
                ticket: None,
            });
        };
        let generation = u32::try_from(credential_generation).map_err(|_| unavailable())?;
        if generation == 0
            || entry.blocked.load(Ordering::Acquire)
            || u64::from(generation) < entry.observed_generation.load(Ordering::Acquire)
        {
            return Err(unavailable());
        }
        let base = credential
            .proxy()
            .and_then(|(proxy, _)| {
                entry
                    .group
                    .proxies
                    .iter()
                    .position(|candidate| candidate == proxy)
            })
            .ok_or_else(unavailable)?;
        let mut state = entry.state.load(Ordering::Acquire);
        let previous = Snapshot::decode(state);
        if generation < previous.credential_generation {
            return Err(unavailable());
        }
        if state == 0 || generation > previous.credential_generation {
            let next = Snapshot {
                credential_generation: generation,
                epoch: previous.epoch + 1,
                base,
                selected: if state != 0 && base == previous.base {
                    previous.selected
                } else {
                    base
                },
            }
            .encode()?;
            state =
                match entry
                    .state
                    .compare_exchange(state, next, Ordering::AcqRel, Ordering::Acquire)
                {
                    Ok(_) => next,
                    Err(current) => current,
                };
        }
        let snapshot = Snapshot::decode(state);
        if snapshot.credential_generation != generation || snapshot.base != base {
            return Err(unavailable());
        }
        let proxy = entry
            .group
            .proxies
            .get(snapshot.selected)
            .ok_or_else(unavailable)?;
        Ok(ProxySelection {
            credential: credential.clone().with_transport_proxy(proxy.clone())?,
            generation: snapshot.epoch as i64,
            ticket: Some((entry.clone(), state)),
        })
    }
}

impl ProxySelection {
    pub(crate) fn member(&self) -> Option<usize> {
        self.ticket
            .as_ref()
            .filter(|(entry, _)| entry.group.proxies.len() > 1)
            .map(|(_, state)| Snapshot::decode(*state).selected)
    }

    pub(crate) fn advance_after_connect_failure(
        &self,
        attempted: &[usize],
    ) -> Result<bool, AppError> {
        let Some((entry, state)) = &self.ticket else {
            return Ok(false);
        };
        let snapshot = Snapshot::decode(*state);
        if entry.blocked.load(Ordering::Acquire)
            || u64::from(snapshot.credential_generation)
                < entry.observed_generation.load(Ordering::Acquire)
        {
            return Ok(false);
        }
        let count = entry.group.proxies.len();
        let next = (1..count)
            .map(|offset| (snapshot.selected + offset) % count)
            .find(|candidate| !attempted.contains(candidate));
        let Some(selected) = next else {
            return Ok(false);
        };
        let next = Snapshot {
            selected,
            epoch: snapshot.epoch + 1,
            ..snapshot
        }
        .encode()?;
        Ok(entry
            .state
            .compare_exchange(*state, next, Ordering::AcqRel, Ordering::Acquire)
            .is_ok())
    }
}
