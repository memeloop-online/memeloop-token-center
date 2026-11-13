use super::*;

impl ApplicationPlugins {
    /// Only verified contribution metadata may establish that a cold application
    /// inventory is independent of ordinary requests. Executable bytes need not
    /// exist for a UI/data-only installation to leave the core gateway available.
    pub(super) async fn baseline_is_independent(&self) -> Result<bool, AppError> {
        let mut cached = self.request_dependencies.lock().await;
        let Some(head) = self.db.optional_application_plugin_head().await? else {
            // The normal refresh publishes a verified empty state once and
            // preserves the missing-head/history guard. Subsequent requests
            // then use the warm in-memory path without repeated DB reads.
            return Ok(false);
        };
        if let Some((receipt, independent)) = &*cached
            && receipt == &head
        {
            return Ok(*independent);
        }
        let reviewed = self
            .db
            .application_plugin_review_manifests(&head.inventory_id)
            .await?;
        let manifests = if let Some(reviewed) = reviewed {
            serde_json::from_value::<Vec<super::super::PluginManifest>>(reviewed).ok()
        } else {
            let entry = self.inventory.read().await.get(&head.inventory_id).cloned();
            match entry {
                Some(entry) => tokio::task::spawn_blocking(move || {
                    // Read manifests without loading/compiling components. The
                    // published contract digest below authenticates the complete
                    // set, including omissions and changed contribution flags.
                    super::super::plugin_directories(&entry.root)
                        .ok()?
                        .into_iter()
                        .map(|path| {
                            let path = path.join("plugin.json");
                            super::super::require_file_size(
                                &path,
                                super::super::PLUGIN_MANIFEST_BYTES,
                                "plugin manifest",
                            )
                            .ok()?;
                            serde_json::from_slice(&std::fs::read(path).ok()?).ok()
                        })
                        .collect::<Option<Vec<super::super::PluginManifest>>>()
                })
                .await
                .ok()
                .flatten(),
                None => None,
            }
        };
        let Some(manifests) = manifests else {
            return Ok(false);
        };
        let contracts: BTreeMap<_, _> = manifests
            .iter()
            .map(|manifest| {
                (
                    manifest.id.clone(),
                    json!({
                        "wit_version": manifest.wit_version,
                        "capabilities": manifest.capabilities,
                        "contributions": manifest.contributions,
                    }),
                )
            })
            .collect();
        if super::super::plugin_configuration_schema_digest(&json!(contracts))?
            != head.contract_digest
        {
            return Ok(false);
        }
        let independent = manifests.iter().all(|manifest| {
            let contributions = &manifest.contributions;
            !contributions.traffic_policy
                && !contributions.request_rewrite
                && contributions.wire_shim.is_none()
                // Provider selection remains explicit, but baseline providers
                // must never silently replace an installed provider override.
                && contributions.providers.iter().all(|provider|
                    !self.baseline_provider_ids.contains(&provider.id))
            // Missing group strategies already retain native ordering, and
            // network selection retains its existing authorized proxy + stale
            // status. Neither contribution changes authentication or billing.
        });
        if self.db.optional_application_plugin_head().await?.as_ref() != Some(&head) {
            return Ok(false);
        }
        *cached = Some((head, independent));
        Ok(independent)
    }
}
