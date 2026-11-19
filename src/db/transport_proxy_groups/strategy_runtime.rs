use super::*;
use crate::db::network_proxies::signals::candidate_signal;
use crate::plugin::PluginRuntime;
use crate::plugin::network_proxy_strategy::ProxyStrategyInput;

impl TransportProxyGroups {
    pub(crate) async fn evaluate_strategies(
        &self,
        pool: &AnyPool,
        plugins: &PluginRuntime,
    ) -> Result<(), AppError> {
        let entries = {
            let managed = self.managed.read().map_err(|_| unavailable())?;
            managed
                .iter()
                .filter_map(|(account, item)| {
                    item.entry.as_ref().map(|entry| {
                        (
                            *account,
                            entry.clone(),
                            item.members.clone(),
                            item.stamp.clone(),
                            item.credential_generation,
                            item.initial_member_id,
                        )
                    })
                })
                .collect::<Vec<_>>()
        };
        let now = unix_millis();
        for (account, entry, members, stamp, generation, initial) in entries {
            let policy = sqlx::query("SELECT a.tenant_id, a.credential_generation, p.version, p.mode, p.plugin_id, p.plugin_version, p.config_json, p.group_id, b.group_id AS binding_group_id, b.version AS binding_version FROM upstream_accounts a JOIN transport_proxy_bindings b ON b.account_id = a.id LEFT JOIN network_proxy_selection_policies p ON p.account_id = a.id WHERE a.id = $1")
                .bind(account.to_string()).fetch_optional(pool).await?;
            let Some(policy) = policy else {
                continue;
            };
            if policy.try_get::<i64, _>("credential_generation")? != generation
                || policy.try_get::<i64, _>("binding_version")? != stamp.binding_version
                || policy.try_get::<Option<String>, _>("binding_group_id")?
                    != stamp.group_id.map(|id| id.to_string())
            {
                continue;
            }
            let version = policy.try_get::<Option<i64>, _>("version")?.unwrap_or(0);
            let mode = policy
                .try_get::<Option<String>, _>("mode")?
                .unwrap_or_else(|| "auto".into());
            let config: Value = serde_json::from_str(
                &policy
                    .try_get::<Option<String>, _>("config_json")?
                    .unwrap_or_else(|| "{}".into()),
            )
            .map_err(|_| unavailable())?;
            let automatic = if mode == "auto" {
                serde_json::from_value::<strategy::AutoConfig>(config.clone())
                    .map_err(|_| unavailable())?
            } else {
                strategy::AutoConfig::default()
            };
            automatic.validate()?;
            let mut candidates = Vec::new();
            let mut consistent = true;
            for member in &members {
                match candidate_signal(
                    pool,
                    member.id,
                    member.version,
                    now.saturating_sub(automatic.signal_window_ms),
                    now,
                )
                .await
                {
                    Ok(candidate) => candidates.push(candidate),
                    Err(AppError::NotFound) => {
                        consistent = false;
                        break;
                    }
                    Err(error) => return Err(error),
                }
            }
            if !consistent || candidates.is_empty() {
                continue;
            }
            let state = entry.state.load(Ordering::Acquire);
            let old = Snapshot::decode(state);
            let base = if state == 0 {
                members
                    .iter()
                    .position(|member| member.id == initial)
                    .unwrap_or(0)
            } else {
                old.base
            };
            let current = members
                .get(if state == 0 { base } else { old.selected })
                .map(|member| member.id);
            let previous = self
                .strategy_status
                .read()
                .map_err(|_| unavailable())?
                .get(&account)
                .cloned();
            let last_change = previous
                .as_ref()
                .and_then(|status| status["selected_at"].as_i64())
                .unwrap_or(0);
            let mut stale = false;
            let mut plugin_revision = None;
            let selected = match mode.as_str() {
                "manual" => current,
                "plugin" => {
                    let plugin_id: String = policy.try_get("plugin_id")?;
                    let plugin_version: String = policy.try_get("plugin_version")?;
                    plugin_revision = plugins
                        .network_proxy_strategies()
                        .into_iter()
                        .find(|item| {
                            item["id"].as_str() == Some(plugin_id.as_str())
                                && item["plugin_version"].as_str() == Some(plugin_version.as_str())
                        })
                        .map(|item| item["revision"].clone());
                    let input = ProxyStrategyInput {
                        tenant_id: policy.try_get("tenant_id")?,
                        policy_revision: version,
                        current_proxy_id: current,
                        observed_at: now,
                        config: config.clone(),
                        candidates,
                    };
                    let plugins = plugins.clone();
                    match tokio::task::spawn_blocking(move || {
                        plugins.execute_network_proxy_strategy(&plugin_id, &plugin_version, &input)
                    })
                    .await
                    {
                        Ok(Ok(plan)) => plan.candidates.first().map(|candidate| candidate.proxy_id),
                        _ => {
                            stale = true;
                            current
                        }
                    }
                }
                _ => strategy::choose_auto(&candidates, current, now, last_change, &automatic),
            };
            let Some(selected) =
                selected.and_then(|chosen| members.iter().position(|member| member.id == chosen))
            else {
                continue;
            };
            if entry.blocked.load(Ordering::Acquire) {
                continue;
            }
            let next = Snapshot {
                credential_generation: u32::try_from(generation).map_err(|_| unavailable())?,
                epoch: if state == 0 {
                    1
                } else if selected != old.selected {
                    old.epoch.saturating_add(1)
                } else {
                    old.epoch
                },
                base,
                selected,
            }
            .encode()?;
            if entry
                .state
                .compare_exchange(state, next, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                continue;
            }
            let changed = current != Some(members[selected].id);
            let reason = if stale {
                "plugin_error_retained_current_exit"
            } else if mode == "manual" {
                "manual_configuration"
            } else if changed {
                "background_strategy_selection"
            } else {
                "retained_current_attemptable_exit"
            };
            self.strategy_status.write().map_err(|_| unavailable())?.insert(account, serde_json::json!({
                "selected_proxy_id": members[selected].id, "selected_proxy_version": members[selected].version,
                "selection_epoch": Snapshot::decode(next).epoch, "policy_revision": version,
                "configuration_revision": stamp.selection_version, "credential_generation": generation,
                "binding_version": stamp.binding_version,
                "plugin_revision": plugin_revision, "reason": reason, "stale": stale, "observed_at": now,
                "selected_at": if changed || last_change == 0 { now } else { last_change }, "scope": "process" }));
        }
        Ok(())
    }

    pub(crate) fn describe_network_proxy_selection(&self, account: Uuid, body: &mut Value) {
        if let Ok(status) = self.strategy_status.read() {
            body["applied"] = status
                .get(&account)
                .filter(|status| {
                    status["policy_revision"] == body["selection_version"]
                        && status["configuration_revision"] == body["configuration_revision"]
                        && status["credential_generation"] == body["credential_generation"]
                        && status["binding_version"] == body["binding_version"]
                })
                .cloned()
                .unwrap_or_else(|| serde_json::json!({"pending": true, "scope": "process"}));
        }
    }
}
