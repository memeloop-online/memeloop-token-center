use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use super::*;

mod managed;
mod persistence;
#[cfg(test)]
mod tests;

const MAX_EPOCH: u64 = (1 << 28) - 1;

#[cfg(test)]
tokio::task_local! {
    static TEST_BEFORE_PROXY_ADVANCE: std::cell::RefCell<Option<Box<dyn FnOnce()>>>;
}

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
    managed: std::sync::RwLock<BTreeMap<Uuid, managed::ManagedEntry>>,
    key: Vec<u8>,
}

pub(crate) struct ProxySelection {
    pub(crate) credential: UpstreamCredential,
    pub(crate) generation: i64,
    ticket: Option<(Arc<Entry>, u64)>,
    member_override: Option<usize>,
}

fn unavailable() -> AppError {
    AppError::Conflict("account transport proxy selection is unavailable".into())
}

impl TransportProxyGroups {
    #[cfg(test)]
    pub(crate) async fn with_test_connect_failure_interleaving<F: std::future::Future>(
        interleave: impl FnOnce() + 'static,
        future: F,
    ) -> F::Output {
        TEST_BEFORE_PROXY_ADVANCE
            .scope(std::cell::RefCell::new(Some(Box::new(interleave))), future)
            .await
    }

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
            let fingerprint =
                Self::fingerprint(group.account_id, group.version, group.proxies.clone(), key)?;
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
        Ok(Self {
            groups,
            managed: std::sync::RwLock::new(BTreeMap::new()),
            key: key.to_vec(),
        })
    }

    pub(crate) fn fingerprint(
        account_id: Uuid,
        version: i64,
        proxies: Vec<String>,
        key: &[u8],
    ) -> Result<String, AppError> {
        let mut digest = Sha256::new();
        digest.update(b"mtc/transport-proxy-group/v1\0");
        digest.update(key);
        digest.update(
            serde_json::to_vec(&Group {
                account_id,
                version,
                proxies,
            })
            .map_err(|_| unavailable())?,
        );
        Ok(format!("{:x}", digest.finalize()))
    }

    #[cfg(test)]
    pub(crate) fn select(
        &self,
        account_id: Uuid,
        credential_generation: i64,
        credential: &UpstreamCredential,
    ) -> Result<ProxySelection, AppError> {
        self.select_config(account_id, credential_generation, credential, &Value::Null)
    }

    pub(crate) fn select_config(
        &self,
        account_id: Uuid,
        credential_generation: i64,
        credential: &UpstreamCredential,
        config: &Value,
    ) -> Result<ProxySelection, AppError> {
        let managed = self.managed.read().map_err(|_| unavailable())?;
        let stamp = config
            .get(super::transport_proxy_management::CONFIG_KEY)
            .map(|value| {
                serde_json::from_value::<super::transport_proxy_management::BindingStamp>(
                    value.clone(),
                )
                .map_err(|_| unavailable())
            })
            .transpose()?;
        let entry = match (stamp.as_ref(), managed.get(&account_id)) {
            (Some(stamp), None) if stamp.group_id.is_none() && stamp.group_version.is_none() => {
                None
            }
            (Some(stamp), Some(current))
                if &current.stamp == stamp
                    && credential_generation >= current.credential_generation =>
            {
                current.entry.as_ref()
            }
            (Some(_), _) | (None, Some(_)) => return Err(unavailable()),
            (None, None) => self.groups.get(&account_id),
        };
        let Some(entry) = entry else {
            return Ok(ProxySelection {
                credential: credential.clone(),
                generation: 0,
                ticket: None,
                member_override: None,
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
            member_override: None,
        })
    }
}

impl ProxySelection {
    pub(crate) fn member_index(&self) -> Option<usize> {
        self.ticket.as_ref().map(|(_, state)| {
            self.member_override
                .unwrap_or_else(|| Snapshot::decode(*state).selected)
        })
    }

    pub(crate) fn member_count(&self) -> usize {
        self.ticket
            .as_ref()
            .map_or(0, |(entry, _)| entry.group.proxies.len())
    }

    pub(crate) fn group_selection_version(&self) -> Option<i64> {
        self.ticket.as_ref().map(|(entry, _)| entry.group.version)
    }

    pub(crate) fn is_request_local(&self) -> bool {
        self.member_override.is_some()
    }

    pub(crate) fn member(&self) -> Option<usize> {
        self.member_index().filter(|_| self.member_count() > 1)
    }

    pub(crate) fn select_unattempted(&mut self, attempted: &[usize]) -> Result<bool, AppError> {
        let Some((entry, state)) = &self.ticket else {
            return Ok(true);
        };
        let snapshot = Snapshot::decode(*state);
        if entry.blocked.load(Ordering::Acquire)
            || u64::from(snapshot.credential_generation)
                < entry.observed_generation.load(Ordering::Acquire)
            || Snapshot::decode(entry.state.load(Ordering::Acquire)).credential_generation
                != snapshot.credential_generation
        {
            return Err(unavailable());
        }
        let Some(member) = self.member().filter(|member| attempted.contains(member)) else {
            return Ok(true);
        };
        let count = entry.group.proxies.len();
        let next = (1..count)
            .map(|offset| (member + offset) % count)
            .find(|candidate| !attempted.contains(candidate));
        let Some(selected) = next else {
            return Ok(false);
        };
        self.credential = self
            .credential
            .clone()
            .with_transport_proxy(entry.group.proxies[selected].clone())?;
        self.member_override = (selected != snapshot.selected).then_some(selected);
        Ok(true)
    }

    pub(crate) fn advance_after_connect_failure(
        &self,
        attempted: &[usize],
    ) -> Result<bool, AppError> {
        Ok(self.advance_after_connect_failure_outcome(attempted)? == "advanced")
    }

    pub(crate) fn advance_after_connect_failure_outcome(
        &self,
        attempted: &[usize],
    ) -> Result<&'static str, AppError> {
        let Some((entry, state)) = &self.ticket else {
            return Ok("not_grouped");
        };
        let snapshot = Snapshot::decode(*state);
        #[cfg(test)]
        if let Some(interleave) = TEST_BEFORE_PROXY_ADVANCE
            .try_with(|hook| hook.borrow_mut().take())
            .ok()
            .flatten()
        {
            interleave();
        }
        if entry.blocked.load(Ordering::Acquire)
            || u64::from(snapshot.credential_generation)
                < entry.observed_generation.load(Ordering::Acquire)
        {
            return Ok("stale");
        }
        if self.is_request_local() {
            return Ok("request_local");
        }
        let count = entry.group.proxies.len();
        let next = (1..count)
            .map(|offset| (snapshot.selected + offset) % count)
            .find(|candidate| !attempted.contains(candidate));
        let Some(selected) = next else {
            return Ok("no_untried_member");
        };
        let next = Snapshot {
            selected,
            epoch: snapshot.epoch + 1,
            ..snapshot
        }
        .encode()?;
        Ok(
            if entry
                .state
                .compare_exchange(*state, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                "advanced"
            } else {
                "contended"
            },
        )
    }
}
