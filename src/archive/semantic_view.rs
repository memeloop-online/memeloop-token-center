//! Manifest-anchored metadata pages and verified atom streams shared by UI and
//! export. No server-side whole-body JSON materialization is performed here.
use super::{
    ArchiveStore, CasArchiveObject, SemanticCoverage, SemanticManifestV2, SemanticPublication,
    SemanticTurnInstance,
};
use crate::error::AppError;
use futures_util::StreamExt;
use serde::Serialize;
use uuid::Uuid;

#[derive(Serialize)]
pub(crate) struct SemanticViewPage {
    pub manifest_revision: u64,
    pub coverage: SemanticCoverage,
    pub instances: Vec<SemanticTurnInstance>,
    pub previous: Option<CasArchiveObject>,
}

impl ArchiveStore {
    async fn semantic_view_manifest(
        &self,
        tenant: Uuid,
        request: Uuid,
        revision: u64,
    ) -> Result<SemanticManifestV2, AppError> {
        let key = format!("tenants/{tenant}/semantic/v2/requests/{request}/{revision}.json");
        let bytes = self.get_bounded(&key, 4096).await?;
        let publication: SemanticPublication = serde_json::from_slice(&bytes)
            .map_err(|_| AppError::storage("semantic_publication_invalid"))?;
        if publication.format_version != 2
            || publication.tenant_id != tenant
            || publication.request_id != request
            || publication.revision != revision
        {
            return Err(AppError::storage("semantic_publication_identity_mismatch"));
        }
        let manifest = self
            .get_semantic_manifest_streamed(tenant, &publication.manifest)
            .await?;
        if manifest.request_id != request
            || manifest.revision != revision
            || manifest.projection_version != publication.projection_version
        {
            return Err(AppError::storage("semantic_publication_identity_mismatch"));
        }
        Ok(manifest)
    }

    pub(crate) async fn published_semantic_revision(
        &self,
        tenant: Uuid,
        request: Uuid,
        leaf: &str,
    ) -> Result<Option<u64>, AppError> {
        // A per-request metadata-only discovery index. SQL authorization and
        // committed leaf identity are prerequisites; raw-root status is not.
        let prefix = object_store::path::Path::from(format!(
            "tenants/{tenant}/semantic/v2/requests/{request}"
        ));
        let mut roots = self.inner.list(Some(&prefix));
        let mut selected = None;
        while let Some(root) = roots.next().await {
            let root = root?;
            let Some(name) = root.location.as_ref().strip_prefix(&format!("{prefix}/")) else {
                continue;
            };
            let Some(revision) = name
                .strip_suffix(".json")
                .and_then(|name| name.parse::<u64>().ok())
            else {
                continue;
            };
            let manifest = self
                .semantic_view_manifest(tenant, request, revision)
                .await?;
            if manifest
                .semantic_head
                .as_ref()
                .is_some_and(|head| head.node_hash == leaf)
            {
                // Complete wins over partial for an identical committed prefix.
                let complete =
                    manifest.coverage.projection_state == super::SemanticProjectionState::Complete;
                if selected.is_none_or(|(old_complete, old_revision)| {
                    (complete, revision) > (old_complete, old_revision)
                }) {
                    selected = Some((complete, revision));
                }
            }
        }
        Ok(selected.map(|(_, revision)| revision))
    }

    pub(crate) async fn read_semantic_view_page(
        &self,
        tenant: Uuid,
        request: Uuid,
        revision: u64,
        requested: Option<&CasArchiveObject>,
    ) -> Result<SemanticViewPage, AppError> {
        let manifest = self
            .semantic_view_manifest(tenant, request, revision)
            .await?;
        let mut cursor = manifest.instances_head.clone();
        let mut expected_end = manifest.semantic_head.as_ref().map(|head| head.depth);
        while let Some(reference) = cursor {
            let page = self
                .get_semantic_instance_page(&manifest, &reference)
                .await?;
            if page.first_ordinal.checked_add(page.instances.len() as u64) != expected_end {
                return Err(AppError::storage("semantic_view_page_anchor_invalid"));
            }
            if requested.is_none()
                || requested.is_some_and(|wanted| {
                    wanted.object_locator == reference.object_locator
                        && wanted.blake3_digest == reference.blake3_digest
                        && wanted.size_bytes == reference.size_bytes
                })
            {
                return Ok(SemanticViewPage {
                    manifest_revision: revision,
                    coverage: manifest.coverage,
                    instances: page.instances,
                    previous: page.previous,
                });
            }
            expected_end = Some(page.first_ordinal);
            cursor = page.previous;
        }
        if requested.is_some() {
            return Err(AppError::BadRequest(
                "semantic page is not in this manifest".into(),
            ));
        }
        Ok(SemanticViewPage {
            manifest_revision: revision,
            coverage: manifest.coverage,
            instances: Vec::new(),
            previous: None,
        })
    }

    pub(crate) async fn open_semantic_view_atom(
        &self,
        tenant: Uuid,
        request: Uuid,
        revision: u64,
        page: Option<&CasArchiveObject>,
        instance_id: &str,
    ) -> Result<super::ArchiveDownload, AppError> {
        let page = self
            .read_semantic_view_page(tenant, request, revision, page)
            .await?;
        let instance = page
            .instances
            .iter()
            .find(|instance| instance.instance_id == instance_id)
            .ok_or_else(|| AppError::BadRequest("semantic instance is not in this page".into()))?;
        self.open_semantic_atom(tenant, instance).await
    }
}
