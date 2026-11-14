//! Publication primitives for the single semantic body authority. Publishing
//! objects is not acknowledgement of SQL binding or permission to delete raw.
use bytes::Bytes;
use serde::Serialize;
use uuid::Uuid;

use super::{ArchiveStore, CasArchiveObject, SemanticInstancePage, SemanticManifestV2};
use crate::error::AppError;

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticPublication {
    pub format_version: u32,
    pub tenant_id: Uuid,
    pub request_id: Uuid,
    pub projection_version: u32,
    pub revision: u64,
    pub manifest: CasArchiveObject,
}

impl ArchiveStore {
    /// Store exactly the existing relational prefix identity as recoverable
    /// metadata, not another body/event tree. Conflicts are never overwritten.
    pub async fn put_semantic_prefix_node(
        &self,
        manifest: &SemanticManifestV2,
        node: &super::SemanticNode,
    ) -> Result<(), AppError> {
        use object_store::{ObjectStoreExt, PutMode, PutOptions, PutPayload};
        manifest.validate(manifest.tenant_id)?;
        node.validate(&super::SemanticHead {
            node_hash: node.node_hash.clone(),
            depth: node.depth,
        })?;
        let store = match manifest.backend_identity.as_deref() {
            None => &self.inner,
            Some(identity) => {
                &self
                    .training
                    .as_ref()
                    .filter(|training| training.identity == identity)
                    .ok_or_else(|| AppError::storage("semantic_manifest_backend_mismatch"))?
                    .store
            }
        };
        let path = super::path::archive_path(&format!(
            "tenants/{}/semantic/v2/nodes/{}.json",
            manifest.tenant_id, node.node_hash,
        ))?;
        let bytes = Bytes::from(
            serde_json::to_vec(node).map_err(|_| AppError::internal("semantic_prefix_encode"))?,
        );
        match store
            .put_opts(
                &path,
                PutPayload::from_bytes(bytes),
                PutOptions {
                    mode: PutMode::Create,
                    ..Default::default()
                },
            )
            .await
        {
            Ok(_) | Err(object_store::Error::AlreadyExists { .. }) => {}
            Err(error) => return Err(error.into()),
        }
        let read = self
            .get_semantic_prefix_node(
                manifest,
                &super::SemanticHead {
                    node_hash: node.node_hash.clone(),
                    depth: node.depth,
                },
            )
            .await?;
        if read.atom_hash != node.atom_hash || read.parent_hash != node.parent_hash {
            return Err(AppError::storage("semantic_prefix_identity_conflict"));
        }
        Ok(())
    }

    /// Publish an immutable metadata page only after its referenced payloads
    /// are readable. This does not duplicate their bytes in the page.
    pub async fn put_semantic_instance_page(
        &self,
        manifest: &SemanticManifestV2,
        page: &SemanticInstancePage,
    ) -> Result<CasArchiveObject, AppError> {
        manifest.validate(manifest.tenant_id)?;
        page.validate(manifest)?;
        for instance in &page.instances {
            self.verify_tenant_content(manifest.tenant_id, &instance.body)
                .await?;
        }
        if let Some(previous) = &page.previous {
            let previous_page = self.get_semantic_instance_page(manifest, previous).await?;
            if previous_page
                .first_ordinal
                .checked_add(previous_page.instances.len() as u64)
                != Some(page.first_ordinal)
            {
                return Err(AppError::storage("semantic_instance_sequence_invalid"));
            }
        } else if page.first_ordinal != 0 {
            return Err(AppError::storage("semantic_instance_sequence_invalid"));
        }
        let encoded =
            serde_json::to_vec(page).map_err(|_| AppError::internal("semantic_instance_encode"))?;
        // Same metadata bound as the reader; never a cap on body contents.
        if encoded.len() > 256 * 1024 {
            return Err(AppError::storage("semantic_instance_page_invalid"));
        }
        self.put_tenant_content(manifest.tenant_id, Bytes::from(encoded))
            .await
    }

    /// The durable index pins a manifest before any optional SQL handoff. It
    /// contains references only. A separate exact binding/coverage receipt is
    /// still required before releasing temporary extraction/retry sources.
    pub async fn publish_semantic_manifest(
        &self,
        manifest: &SemanticManifestV2,
    ) -> Result<SemanticPublication, AppError> {
        manifest.validate(manifest.tenant_id)?;
        let mut page_ref = manifest.instances_head.clone();
        let mut expected_end = None;
        let mut expected_node = manifest.semantic_head.clone();
        while let Some(reference) = page_ref {
            let page = self
                .get_semantic_instance_page(manifest, &reference)
                .await?;
            let end = page
                .first_ordinal
                .checked_add(page.instances.len() as u64)
                .ok_or_else(|| AppError::storage("semantic_instance_sequence_invalid"))?;
            if expected_end.is_some_and(|expected| expected != end) {
                return Err(AppError::storage("semantic_instance_sequence_invalid"));
            }
            if page.previous.is_some() && page.first_ordinal == 0 {
                return Err(AppError::storage("semantic_instance_sequence_invalid"));
            }
            // Instance order and canonical prefix order must describe the same
            // atoms. A valid CAS body or page alone is not coverage proof.
            for instance in page.instances.iter().rev() {
                let head = expected_node
                    .as_ref()
                    .ok_or_else(|| AppError::storage("semantic_instance_prefix_mismatch"))?;
                if head.node_hash != instance.node_hash {
                    return Err(AppError::storage("semantic_instance_prefix_mismatch"));
                }
                let node = self.get_semantic_prefix_node(manifest, head).await?;
                if node.atom_hash != instance.atom_hash || node.depth != instance.ordinal + 1 {
                    return Err(AppError::storage("semantic_instance_prefix_mismatch"));
                }
                expected_node = node.parent_hash.map(|node_hash| super::SemanticHead {
                    node_hash,
                    depth: node.depth - 1,
                });
            }
            expected_end = Some(page.first_ordinal);
            page_ref = page.previous;
        }
        if expected_node.is_some() || expected_end.is_some_and(|first| first != 0) {
            return Err(AppError::storage("semantic_instance_sequence_invalid"));
        }
        if manifest.semantic_head.is_some() != manifest.instances_head.is_some() {
            return Err(AppError::storage("semantic_manifest_invalid"));
        }
        if let Some(head) = &manifest.semantic_head {
            self.get_semantic_prefix_node(manifest, head).await?;
        }
        let encoded = serde_json::to_vec(manifest)
            .map_err(|_| AppError::internal("semantic_manifest_encode"))?;
        let object = self
            .put_tenant_content(manifest.tenant_id, Bytes::from(encoded))
            .await?;
        // Validate actual selected backend rather than trusting write flags.
        self.get_semantic_manifest_streamed(manifest.tenant_id, &object)
            .await?;
        let publication = SemanticPublication {
            format_version: 2,
            tenant_id: manifest.tenant_id,
            request_id: manifest.request_id,
            projection_version: manifest.projection_version,
            revision: manifest.revision,
            manifest: object,
        };
        self.restore_durable_root(
            &format!(
                "tenants/{}/semantic/v2/requests/{}/{}.json",
                manifest.tenant_id, manifest.request_id, manifest.revision,
            ),
            Bytes::from(
                serde_json::to_vec(&publication)
                    .map_err(|_| AppError::internal("semantic_publication_encode"))?,
            ),
        )
        .await?;
        Ok(publication)
    }
}
