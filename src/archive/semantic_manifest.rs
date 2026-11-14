//! One semantic prefix is the permanent body authority. Instance pages contain
//! identities and state only; they must never duplicate message/tool text.
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::CasArchiveObject;
use crate::error::AppError;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SemanticHead {
    pub node_hash: String,
    pub depth: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticCoverage {
    pub observed_terminal: bool,
    pub projection_state: SemanticProjectionState,
    pub omissions: Vec<String>,
    pub gaps: Vec<String>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SemanticProjectionState {
    Complete,
    Partial,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticManifestV2 {
    pub format_version: u32,
    pub projection_version: u32,
    pub tenant_id: Uuid,
    pub session_id: String,
    pub branch_id: String,
    pub parent_branch_id: Option<String>,
    pub parent_turn_id: Option<String>,
    pub request_id: Uuid,
    pub revision: u64,
    /// Explicit metadata backend capability, independent of mutable write flags.
    pub backend_identity: Option<String>,
    pub semantic_head: Option<SemanticHead>,
    pub instances_head: Option<CasArchiveObject>,
    pub coverage: SemanticCoverage,
}

impl SemanticManifestV2 {
    pub(crate) fn validate(&self, tenant: Uuid) -> Result<(), AppError> {
        if self.format_version != 2
            || self.projection_version == 0
            || self.tenant_id != tenant
            || self.session_id.is_empty()
            || self.branch_id.is_empty()
            || self.revision == 0
            || self.parent_branch_id.as_ref() == Some(&self.branch_id)
            || self
                .semantic_head
                .as_ref()
                .is_some_and(|head| head.depth == 0 || !digest(&head.node_hash))
            || self
                .backend_identity
                .as_ref()
                .is_some_and(|identity| !digest(identity))
            || (self.coverage.projection_state == SemanticProjectionState::Complete
                && (!self.coverage.observed_terminal || !self.coverage.gaps.is_empty()))
        {
            return Err(AppError::storage("semantic_manifest_invalid"));
        }
        if let Some(page) = &self.instances_head {
            page.validate_tenant(tenant)?;
        }
        Ok(())
    }
}

/// Exactly the existing context_nodes identity; no second event-body tree.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticNode {
    pub node_hash: String,
    pub parent_hash: Option<String>,
    pub atom_hash: String,
    pub depth: u64,
}

impl SemanticNode {
    pub(crate) fn validate(&self, expected: &SemanticHead) -> Result<(), AppError> {
        if self.node_hash != expected.node_hash
            || self.depth != expected.depth
            || self.depth == 0
            || !digest(&self.atom_hash)
            || self.parent_hash.as_ref().is_some_and(|hash| !digest(hash))
            || (self.depth == 1) != self.parent_hash.is_none()
            || crate::conversation::digest_parts(self.parent_hash.as_deref(), &self.atom_hash)
                != self.node_hash
        {
            return Err(AppError::storage("semantic_prefix_node_invalid"));
        }
        Ok(())
    }
}

fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

impl super::ArchiveStore {
    /// Shared UI/export reader. Root bytes are verified by the same immutable
    /// tenant-content stream used for existing semantic body objects.
    pub async fn get_semantic_manifest_streamed(
        &self,
        tenant: Uuid,
        object: &CasArchiveObject,
    ) -> Result<SemanticManifestV2, AppError> {
        let manifest: SemanticManifestV2 = self.read_tenant_json_streamed(tenant, object).await?;
        manifest.validate(tenant)?;
        let actual = super::routing::split(&object.object_locator)?.map(|(identity, _)| identity);
        if actual != manifest.backend_identity.as_deref() {
            return Err(AppError::storage("semantic_manifest_backend_mismatch"));
        }
        Ok(manifest)
    }

    /// Load precisely one existing prefix identity, never an event containing a
    /// second copy of a request or an SSE document.
    pub async fn get_semantic_prefix_node(
        &self,
        manifest: &SemanticManifestV2,
        expected: &SemanticHead,
    ) -> Result<SemanticNode, AppError> {
        use futures_util::StreamExt;
        use object_store::ObjectStoreExt;
        manifest.validate(manifest.tenant_id)?;
        if !digest(&expected.node_hash) || expected.depth == 0 {
            return Err(AppError::storage("semantic_prefix_node_invalid"));
        }
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
            manifest.tenant_id, expected.node_hash,
        ))?;
        let download = store.get(&path).await?;
        if download.meta.size > 4096 {
            return Err(AppError::storage("semantic_prefix_node_invalid"));
        }
        let mut stream = download.into_stream();
        let mut bytes = bytes::BytesMut::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if chunk.len() > 4096usize.saturating_sub(bytes.len()) {
                return Err(AppError::storage("semantic_prefix_node_invalid"));
            }
            bytes.extend_from_slice(&chunk);
        }
        let node: SemanticNode = serde_json::from_slice(&bytes)
            .map_err(|_| AppError::storage("semantic_prefix_node_invalid"))?;
        node.validate(expected)?;
        Ok(node)
    }
}
