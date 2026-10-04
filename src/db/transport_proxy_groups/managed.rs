use super::*;
use crate::db::transport_proxy_management::{BindingStamp, CONFIG_KEY, Member};

pub(super) struct ManagedEntry {
    pub stamp: BindingStamp,
    pub credential_generation: i64,
    pub entry: Option<Arc<Entry>>,
    members: Vec<Member>,
}

impl TransportProxyGroups {
    #[cfg(test)]
    pub(crate) async fn synchronize_managed_for_test(
        &self,
        pool: &AnyPool,
    ) -> Result<(), AppError> {
        self.refresh_managed(pool).await?;
        for entry in self.persistence_entries()? {
            entry.synchronize(pool).await?;
        }
        Ok(())
    }

    pub(crate) async fn refresh_managed(&self, pool: &AnyPool) -> Result<(), AppError> {
        let rows = sqlx::query("SELECT b.account_id, b.version AS binding_version, b.group_id, b.initial_member_id, b.replacement_member_id, g.version AS group_version, g.members_ciphertext, a.credential_generation, a.config_json, s.group_fingerprint AS saved_fingerprint, s.credential_generation AS saved_generation, s.selection_generation AS saved_epoch, s.base_index AS saved_base, s.selected_index AS saved_selected FROM transport_proxy_bindings b JOIN upstream_accounts a ON a.id = b.account_id LEFT JOIN transport_proxy_groups g ON g.id = b.group_id LEFT JOIN upstream_transport_proxy_selections s ON s.account_id = b.account_id WHERE b.group_id IS NOT NULL ORDER BY b.account_id LIMIT 257")
            .fetch_all(pool).await?;
        let mut next = BTreeMap::new();
        let mut active = 0usize;
        let mut bytes = 0usize;
        for row in rows {
            let account_id = parse_uuid(row.try_get("account_id")?)?;
            let config: Value = serde_json::from_str(&row.try_get::<String, _>("config_json")?)
                .map_err(|_| unavailable())?;
            let stamp: BindingStamp =
                serde_json::from_value(config.get(CONFIG_KEY).ok_or_else(unavailable)?.clone())
                    .map_err(|_| unavailable())?;
            let group_id: Option<String> = row.try_get("group_id")?;
            if stamp.binding_version != row.try_get::<i64, _>("binding_version")?
                || stamp.group_id.map(|id| id.to_string()) != group_id
                || stamp.group_version != row.try_get::<Option<i64>, _>("group_version")?
            {
                return Err(unavailable());
            }
            let generation: i64 = row.try_get("credential_generation")?;
            let members: Vec<Member> = if group_id.is_some() {
                active += 1;
                open_private_json(
                    &row.try_get::<String, _>("members_ciphertext")?,
                    &self.key,
                    b"mtc/transport-proxy-members/v1",
                )?
            } else {
                Vec::new()
            };
            let entry = if group_id.is_some() {
                let proxies = members
                    .iter()
                    .map(|member| member.proxy_url.clone())
                    .collect::<Vec<_>>();
                let encoded = serde_json::to_string(&vec![Group {
                    account_id,
                    version: stamp.selection_version,
                    proxies,
                }])
                .map_err(|_| unavailable())?;
                bytes += encoded.len() + 512;
                if active > 256 || bytes > 256 * 1024 {
                    return Err(unavailable());
                }
                let parsed = Self::parse(&encoded, &self.key)?;
                let candidate = parsed
                    .groups
                    .get(&account_id)
                    .ok_or_else(unavailable)?
                    .clone();
                let previous = self.managed.read().map_err(|_| unavailable())?;
                if let Some(old) = previous
                    .get(&account_id)
                    .filter(|old| old.stamp == stamp && old.credential_generation == generation)
                {
                    old.entry.clone()
                } else {
                    let initial = parse_uuid(row.try_get("initial_member_id")?)?;
                    let base = members
                        .iter()
                        .position(|member| member.id == initial)
                        .ok_or_else(unavailable)?;
                    if row
                        .try_get::<Option<String>, _>("saved_fingerprint")?
                        .as_deref()
                        == Some(candidate.fingerprint.as_str())
                        && row.try_get::<Option<i64>, _>("saved_generation")? == Some(generation)
                    {
                        let selected = usize::try_from(row.try_get::<i64, _>("saved_selected")?)
                            .map_err(|_| unavailable())?;
                        let saved_base = usize::try_from(row.try_get::<i64, _>("saved_base")?)
                            .map_err(|_| unavailable())?;
                        if selected >= members.len() || saved_base != base {
                            return Err(unavailable());
                        }
                        let saved = Snapshot {
                            credential_generation: u32::try_from(generation)
                                .map_err(|_| unavailable())?,
                            epoch: u64::try_from(row.try_get::<i64, _>("saved_epoch")?)
                                .map_err(|_| unavailable())?,
                            base,
                            selected,
                        }
                        .encode()?;
                        candidate.state.store(saved, Ordering::Release);
                        candidate.persisted.store(saved, Ordering::Release);
                        candidate.durable.store(saved, Ordering::Release);
                    }
                    if let Some(old) = previous
                        .get(&account_id)
                        .filter(|old| old.stamp.group_id == stamp.group_id)
                        && let Some(old_entry) = &old.entry
                    {
                        let state = old_entry.state.load(Ordering::Acquire);
                        if state != 0 {
                            if candidate.fingerprint == old_entry.fingerprint
                                && candidate.durable.load(Ordering::Acquire) == 0
                            {
                                candidate.durable.store(
                                    old_entry.durable.load(Ordering::Acquire),
                                    Ordering::Release,
                                );
                            }
                            let snapshot = Snapshot::decode(state);
                            let retained =
                                old.members.get(snapshot.selected).and_then(|old_member| {
                                    members.iter().position(|member| {
                                        member.id == old_member.id
                                            && member.proxy_url == old_member.proxy_url
                                    })
                                });
                            let replacement: Option<String> =
                                row.try_get("replacement_member_id")?;
                            let selected = retained
                                .or_else(|| {
                                    members.iter().position(|member| {
                                        Some(member.id.to_string()) == replacement
                                    })
                                })
                                .unwrap_or(base);
                            candidate.state.store(
                                Snapshot {
                                    credential_generation: u32::try_from(generation)
                                        .map_err(|_| unavailable())?,
                                    epoch: snapshot.epoch + 1,
                                    base,
                                    selected,
                                }
                                .encode()?,
                                Ordering::Release,
                            );
                        }
                    }
                    Some(candidate)
                }
            } else {
                None
            };
            next.insert(
                account_id,
                ManagedEntry {
                    stamp,
                    credential_generation: generation,
                    entry,
                    members,
                },
            );
        }
        let mut current = self.managed.write().map_err(|_| unavailable())?;
        for (id, old) in current.iter() {
            if let Some(entry) = &old.entry
                && !next
                    .get(id)
                    .and_then(|new| new.entry.as_ref())
                    .is_some_and(|new| Arc::ptr_eq(new, entry))
            {
                entry.blocked.store(true, Ordering::Release);
            }
        }
        *current = next;
        Ok(())
    }

    pub(crate) fn describe_binding(&self, body: &mut Value) {
        let Some(id) = body
            .get("account_id")
            .and_then(Value::as_str)
            .and_then(|id| Uuid::parse_str(id).ok())
        else {
            return;
        };
        let Ok(current) = self.managed.read() else {
            return;
        };
        let Some(entry) = current.get(&id) else {
            if body["group_id"].is_null() {
                body["runtime"]["configuration_state"] = Value::String("unbound".into());
                body["runtime"]["applied_binding_version"] = body["binding_version"].clone();
            }
            return;
        };
        let stamp = &entry.stamp;
        if body["binding_version"] != stamp.binding_version
            || body["group_version"] != serde_json::json!(stamp.group_version)
            || body["group_id"] != serde_json::json!(stamp.group_id)
        {
            return;
        }
        body["runtime"]["configuration_state"] = Value::String(
            if stamp.group_id.is_some() {
                "applied"
            } else {
                "unbound"
            }
            .into(),
        );
        body["runtime"]["applied_binding_version"] = serde_json::json!(stamp.binding_version);
        body["runtime"]["applied_group_version"] = serde_json::json!(stamp.group_version);
        if let Some(selector) = &entry.entry {
            let state = selector.state.load(Ordering::Acquire);
            if state != 0 {
                body["runtime"]["selected_member_id"] = serde_json::json!(
                    entry
                        .members
                        .get(Snapshot::decode(state).selected)
                        .map(|member| member.id)
                );
            }
        }
    }

    pub(super) fn persistence_entries(&self) -> Result<Vec<Arc<Entry>>, AppError> {
        let managed = self.managed.read().map_err(|_| unavailable())?;
        Ok(self
            .groups
            .iter()
            .filter(|(id, _)| !managed.contains_key(id))
            .map(|(_, entry)| entry.clone())
            .chain(managed.values().filter_map(|entry| entry.entry.clone()))
            .collect())
    }
}
