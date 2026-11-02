//! Account-scoped background planning. Expiry windows and selection policy
//! belong to the guest; the host enforces authority and mutation preconditions.
use super::*;
use crate::AppState;
use crate::db::plugin_background_actions::BackgroundActionGrant;
use crate::upstream_quota::reset::{BackgroundResetGuard, CreditInventory};
use sha2::Sha256;

pub(crate) mod bindings {
    wasmtime::component::bindgen!({
        world: "background-actions-plugin",
        path: "wit/background-actions.wit",
    });
}
use bindings::exports::memeloop::background_actions::planner::{
    Context, Credit, ResetCreditIntent,
};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BackgroundActionContribution {
    pub interval_seconds: u64,
}

pub(super) fn validate_contribution(manifest: &PluginManifest) -> Result<(), AppError> {
    let declared = manifest
        .capabilities
        .iter()
        .any(|c| matches!(c, PluginCapability::QuotaResetCredit));
    if declared != manifest.contributions.background_actions.is_some()
        || manifest
            .contributions
            .background_actions
            .as_ref()
            .is_some_and(|action| {
                !(5..=3600).contains(&action.interval_seconds) || manifest.wasm.is_none()
            })
    {
        return Err(AppError::BadRequest("background actions need a component, quota_reset_credit capability, and an interval of 5 to 3600 seconds".into()));
    }
    Ok(())
}

pub(super) fn prepare_component(
    engine: &Engine,
    manifest: &PluginManifest,
    component: Option<&Component>,
) -> Result<Option<bindings::BackgroundActionsPluginPre<HostState>>, AppError> {
    if manifest.contributions.background_actions.is_none() {
        return Ok(None);
    }
    let component = component
        .ok_or_else(|| AppError::BadRequest("background action component missing".into()))?;
    // The guest links only its planner export and receives host-validated context.
    let linker = Linker::<HostState>::new(engine);
    let pre = linker.instantiate_pre(component).map_err(|_| {
        AppError::BadRequest(
            "background action component has an incompatible ABI or imports".into(),
        )
    })?;
    Ok(Some(
        bindings::BackgroundActionsPluginPre::new(pre).map_err(|_| {
            AppError::BadRequest("background action component has an incompatible ABI".into())
        })?,
    ))
}

impl PluginRuntime {
    pub(crate) fn background_action_targets(&self) -> Vec<(String, u64)> {
        self.plugins
            .iter()
            .filter_map(|plugin| {
                plugin
                    .manifest
                    .contributions
                    .background_actions
                    .as_ref()
                    .map(|action| (plugin.manifest.id.clone(), action.interval_seconds))
            })
            .collect()
    }

    pub(crate) fn has_background_actions(&self, plugin: &str) -> bool {
        self.plugins.iter().any(|p| {
            p.manifest.id == plugin && p.manifest.contributions.background_actions.is_some()
        })
    }

    pub(crate) fn plan_background_action(
        &self,
        grant: &BackgroundActionGrant,
        config_json: String,
        inventory: CreditInventory,
        now: i64,
    ) -> Result<Option<ResetCreditIntent>, AppError> {
        let plugin = self
            .plugins
            .iter()
            .find(|p| p.manifest.id == grant.plugin_id)
            .ok_or(AppError::NotFound)?;
        let pre = plugin
            .background_action_component
            .as_ref()
            .ok_or(AppError::NotFound)?;
        let engine = self
            .engine
            .as_ref()
            .ok_or_else(|| plugin_runtime_failure("background_action_engine"))?;
        let timeout = self.execution_timeout;
        let mut store = Store::new(
            engine,
            HostState {
                plugin_id: plugin.manifest.id.clone(),
                capabilities: Vec::new(),
                http: self
                    .http
                    .as_ref()
                    .ok_or_else(|| plugin_runtime_failure("background_action_http"))?
                    .clone(),
                runtime: self
                    .runtime
                    .as_ref()
                    .ok_or_else(|| plugin_runtime_failure("background_action_runtime"))?
                    .clone(),
                kv: None,
                limits: StoreLimitsBuilder::new()
                    .memory_size(32 * 1024 * 1024)
                    .table_elements(PLUGIN_TABLE_ELEMENTS)
                    .instances(8)
                    .tables(2)
                    .memories(2)
                    .build(),
                deadline: Instant::now() + timeout,
                http_body_limit: None,
                http_diagnostic: PluginHttpDiagnostic::default(),
            },
        );
        store.limiter(|state| &mut state.limits);
        store.set_epoch_deadline(epoch_deadline_ticks(timeout));
        store
            .set_fuel(self.fuel)
            .map_err(|_| plugin_runtime_failure("background_action_fuel"))?;
        let guest = pre
            .instantiate(&mut store)
            .map_err(|error| plugin_failure(&plugin.manifest.id, error))?;
        let context = Context {
            now_ms: now,
            tenant_external_id: grant.tenant_external_id.clone(),
            upstream_account_id: grant.upstream_account_id.clone(),
            authorization_revision: grant.revision,
            config_json,
            credits: inventory
                .credits
                .into_iter()
                .map(|c| Credit {
                    fingerprint: c.fingerprint,
                    granted_at_ms: c.granted_at_ms,
                    expires_at_ms: c.expires_at_ms,
                })
                .collect(),
            inventory_fingerprint: inventory.fingerprint,
        };
        let intent = guest
            .memeloop_background_actions_planner()
            .call_plan(&mut store, &context)
            .map_err(|error| plugin_failure(&plugin.manifest.id, error))?
            .map_err(|error| {
                plugin_reported_error(&plugin.manifest.id, "background-action-plan", &error)
            })?;
        if intent.as_ref().is_some_and(|intent| {
            intent.credit_fingerprint.len() != 64 || intent.inventory_fingerprint.len() != 64
        }) {
            return Err(AppError::BadRequest(
                "background action returned invalid evidence".into(),
            ));
        }
        Ok(intent)
    }
}

pub(crate) fn bootstrap_authority_hash(state: &AppState) -> String {
    use hmac::{Hmac, Mac};
    let mut mac = Hmac::<Sha256>::new_from_slice(state.config.key_pepper.as_bytes())
        .expect("HMAC supports any key length");
    mac.update(b"plugin-background-action-bootstrap-v1\0");
    mac.update(state.config.service_token.as_bytes());
    format!("{:x}", mac.finalize().into_bytes())
}

pub(crate) async fn execute_tick(
    state: AppState,
    grant: BackgroundActionGrant,
    owner: String,
    blocking: &crate::worker::BlockingTasks,
) -> Result<Option<crate::db::QuotaResetOperation>, AppError> {
    if grant.actor_service_id == "bootstrap"
        && grant.bootstrap_authority_hash != bootstrap_authority_hash(&state)
    {
        return Err(AppError::Forbidden);
    }
    state
        .db
        .authorize_background_action_tick(&grant, &owner)
        .await?;
    let account_id = Uuid::parse_str(&grant.upstream_account_id).map_err(|_| AppError::NotFound)?;
    state
        .db
        .require_upstream_tenant(account_id, &grant.tenant_external_id)
        .await?;
    let (account, credential) = state
        .db
        .upstream_account_with_credential(account_id, state.config.key_pepper.as_bytes())
        .await?;
    if account.status != "active" || account.driver != "openai-codex" {
        return Err(AppError::Forbidden);
    }
    let inventory = crate::upstream_quota::reset::background_credit_inventory(
        &state,
        &account,
        &credential,
        &grant.tenant_external_id,
    )
    .await?;
    let layers = state
        .db
        .plugin_configuration_layers(account.tenant_id)
        .await?;
    let contribution = state.plugins.configuration_contribution(&grant.plugin_id);
    let config = layers
        .iter()
        .find(|c| c.plugin_id == grant.plugin_id && c.tenant_id == Some(account.tenant_id))
        .or_else(|| {
            layers
                .iter()
                .find(|c| c.plugin_id == grant.plugin_id && c.tenant_id.is_none())
        })
        .map(|c| c.value.clone())
        .or_else(|| contribution.as_ref().map(|c| c.default.clone()))
        .unwrap_or_else(|| serde_json::json!({}));
    if let Some(contribution) = contribution {
        crate::schema::validate_instance(&contribution.schema, &config)?;
    }
    let config_json = serde_json::to_string(&config)
        .map_err(|_| plugin_runtime_failure("background_action_configuration"))?;
    let plugins = state.plugins.clone();
    let planning_grant = grant.clone();
    let intent = blocking
        .run(move || {
            plugins.plan_background_action(
                &planning_grant,
                config_json,
                inventory,
                crate::db::unix_millis(),
            )
        })
        .await
        .ok_or_else(|| plugin_runtime_failure("background_action_join"))??;
    let Some(intent) = intent else {
        return Ok(None);
    };
    // The exact host-validated snapshot is fenced against the durable runtime
    // head in the final dispatch transaction, after all supplier reads.
    let guard = BackgroundResetGuard {
        runtime_revision: state.application_plugin_revision(),
        grant,
        lease_owner: owner,
        credit_fingerprint: intent.credit_fingerprint,
        inventory_fingerprint: intent.inventory_fingerprint,
        not_before_ms: intent.not_before_ms,
        deadline_ms: intent.deadline_ms,
    };
    crate::upstream_quota::reset::background_reset(&state, &account, &credential, &guard)
        .await
        .map(Some)
}
