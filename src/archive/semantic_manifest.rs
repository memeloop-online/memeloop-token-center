//! One semantic prefix is the permanent body authority. Instance pages contain
//! identities and state only; they must never duplicate message/tool text.
//! These are individual read primitives, not a manifest-anchored traversal or
//! a proof of complete coverage. No HTTP materializer is provided here.
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

/// Per-occurrence metadata. Message contents are held only by the atom object.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticTurnInstance {
    pub instance_id: String,
    pub request_id: Uuid,
    pub attempt_id: Option<String>,
    pub message_id: Option<String>,
    pub turn_id: Option<String>,
    pub branch_id: String,
    pub parent_instance_id: Option<String>,
    pub origin: String,
    pub ordinal: u64,
    pub atom_hash: String,
    pub node_hash: String,
    pub terminal_state: String,
    pub body: CasArchiveObject,
}

/// Immutable bounded metadata chain, anchored by one exact manifest revision.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticInstancePage {
    pub format_version: u32,
    pub tenant_id: Uuid,
    pub session_id: String,
    pub branch_id: String,
    pub first_ordinal: u64,
    pub previous: Option<CasArchiveObject>,
    pub instances: Vec<SemanticTurnInstance>,
}

impl SemanticInstancePage {
    fn validate(&self, manifest: &SemanticManifestV2) -> Result<(), AppError> {
        if self.format_version != 2
            || self.tenant_id != manifest.tenant_id
            || self.session_id != manifest.session_id
            || self.branch_id != manifest.branch_id
            || self.instances.is_empty()
            || self.instances.len() > 64
        {
            return Err(AppError::storage("semantic_instance_page_invalid"));
        }
        if let Some(previous) = &self.previous {
            previous.validate_tenant(self.tenant_id)?;
        }
        let mut ids = std::collections::HashSet::new();
        for (offset, item) in self.instances.iter().enumerate() {
            if item.instance_id.is_empty()
                || !ids.insert(&item.instance_id)
                || item.branch_id != self.branch_id
                || item.origin.is_empty()
                || item.terminal_state.is_empty()
                || !digest(&item.atom_hash)
                || !digest(&item.node_hash)
                || self.first_ordinal.checked_add(offset as u64) != Some(item.ordinal)
            {
                return Err(AppError::storage("semantic_instance_page_invalid"));
            }
            item.body.validate_tenant(self.tenant_id)?;
        }
        Ok(())
    }
}

/// Only semantic body fields are allowed; origin belongs to the instance.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticAtomBody {
    pub role: String,
    pub parts: Vec<serde_json::Value>,
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

    pub async fn get_semantic_instance_page(
        &self,
        manifest: &SemanticManifestV2,
        object: &CasArchiveObject,
    ) -> Result<SemanticInstancePage, AppError> {
        manifest.validate(manifest.tenant_id)?;
        let bytes = self
            .get_tenant_content(manifest.tenant_id, object, 256 * 1024)
            .await?;
        let page: SemanticInstancePage = serde_json::from_slice(&bytes)
            .map_err(|_| AppError::storage("semantic_instance_page_invalid"))?;
        page.validate(manifest)?;
        if page.previous.as_ref() == Some(object) {
            return Err(AppError::storage("semantic_instance_page_cycle"));
        }
        Ok(page)
    }

    /// Streams the existing immutable body without building an unbudgeted JSON
    /// tree. The caller owns incremental decoding and its allocation budget.
    /// This checks physical CAS integrity, not whole-root semantic coverage.
    #[allow(
        dead_code,
        reason = "Internal streaming foundation; consumer wiring follows separately."
    )]
    pub(crate) async fn open_semantic_atom(
        &self,
        tenant: Uuid,
        instance: &SemanticTurnInstance,
    ) -> Result<super::ArchiveDownload, AppError> {
        if !digest(&instance.atom_hash) || !digest(&instance.node_hash) {
            return Err(AppError::storage("semantic_atom_invalid"));
        }
        self.open_tenant_content(tenant, &instance.body).await
    }

    // Test-only reference decoder. Production detail/export must consume the
    // stream above with an explicit decoder allocation owner, not materialize
    // arbitrary JSON through this unbudgeted helper.
    #[cfg(test)]
    async fn get_semantic_atom(
        &self,
        tenant: Uuid,
        instance: &SemanticTurnInstance,
    ) -> Result<SemanticAtomBody, AppError> {
        let mut source = self.open_semantic_atom(tenant, instance).await?.stream;
        use futures_util::TryStreamExt;
        let mut data = Vec::new();
        while let Some(chunk) = source.try_next().await? {
            data.extend_from_slice(&chunk);
        }
        let mut value: serde_json::Value = serde_json::from_slice(&data)
            .map_err(|_| AppError::storage("semantic_atom_invalid"))?;
        // Existing v1 atom objects encoded origin despite excluding it from the
        // semantic hash. Reuse their bytes; instance metadata owns that field.
        if let Some(object) = value.as_object_mut() {
            object.remove("origin");
        }
        let body: SemanticAtomBody = serde_json::from_value(value)
            .map_err(|_| AppError::storage("semantic_atom_invalid"))?;
        if body.role.is_empty()
            || body.parts.iter().any(|part| {
                part.get("kind")
                    .and_then(serde_json::Value::as_str)
                    .is_none()
            })
        {
            return Err(AppError::storage("semantic_atom_invalid"));
        }
        // Borrow parts directly in canonical key order: no second body tree.
        #[derive(Serialize)]
        struct Canonical<'a> {
            parts: &'a [serde_json::Value],
            role: &'a str,
        }
        let canonical = Canonical {
            parts: &body.parts,
            role: &body.role,
        };
        struct HashWriter(blake3::Hasher);
        impl std::io::Write for HashWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.update(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut writer = HashWriter(blake3::Hasher::new());
        serde_json::to_writer(&mut writer, &canonical)
            .map_err(|_| AppError::internal("semantic_atom_encode"))?;
        if writer.0.finalize().to_hex().as_str() != instance.atom_hash {
            return Err(AppError::storage("semantic_atom_identity_mismatch"));
        }
        Ok(body)
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

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use serde_json::json;

    fn manifest(tenant: Uuid) -> SemanticManifestV2 {
        SemanticManifestV2 {
            format_version: 2,
            projection_version: 2,
            tenant_id: tenant,
            session_id: "development-session".into(),
            branch_id: "main".into(),
            parent_branch_id: None,
            parent_turn_id: None,
            request_id: Uuid::now_v7(),
            revision: 1,
            backend_identity: None,
            semantic_head: None,
            instances_head: None,
            coverage: SemanticCoverage {
                observed_terminal: true,
                projection_state: SemanticProjectionState::Complete,
                omissions: Vec::new(),
                gaps: Vec::new(),
            },
        }
    }

    #[tokio::test]
    async fn existing_atom_bytes_are_reused_with_origin_in_occurrence_only() {
        let archive = super::super::tests::memory_store();
        let tenant = Uuid::now_v7();
        let canonical = json!({"role":"assistant", "parts":[
            {"part_index":0,"kind":"reasoning","text":"inspect the result"},
            {"part_index":1,"kind":"tool_call","call_id":"call-1",
             "tool_name":"read","arguments":{"path":"source.rs"}}
        ]});
        let atom_hash = blake3::hash(&serde_json::to_vec(&canonical).unwrap())
            .to_hex()
            .to_string();
        let mut legacy = canonical.clone();
        legacy["origin"] = json!("return");
        let object = archive
            .put_tenant_content(tenant, Bytes::from(serde_json::to_vec(&legacy).unwrap()))
            .await
            .unwrap();
        let mut instance = SemanticTurnInstance {
            instance_id: "one-occurrence".into(),
            request_id: Uuid::now_v7(),
            attempt_id: None,
            message_id: None,
            turn_id: None,
            branch_id: "main".into(),
            parent_instance_id: None,
            origin: "return".into(),
            ordinal: 0,
            atom_hash,
            node_hash: "a".repeat(64),
            terminal_state: "completed".into(),
            body: object.clone(),
        };
        let read = archive.get_semantic_atom(tenant, &instance).await.unwrap();
        assert_eq!(serde_json::to_value(read).unwrap(), canonical);
        instance.origin = "request".into();
        assert!(archive.get_semantic_atom(tenant, &instance).await.is_ok());
        // Reader reuse never rewrites the existing legacy physical object.
        let stored = archive
            .get_tenant_content(tenant, &object, 4096)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&stored).unwrap(),
            legacy
        );
        instance.atom_hash = "b".repeat(64);
        assert!(archive.get_semantic_atom(tenant, &instance).await.is_err());
        assert!(
            archive
                .get_semantic_atom(Uuid::now_v7(), &instance)
                .await
                .is_err()
        );
    }

    #[test]
    fn incomplete_prefix_and_false_complete_coverage_are_rejected() {
        let atom = "c".repeat(64);
        let mut node = SemanticNode {
            node_hash: crate::conversation::digest_parts(None, &atom),
            parent_hash: None,
            atom_hash: atom,
            depth: 1,
        };
        let expected = SemanticHead {
            node_hash: node.node_hash.clone(),
            depth: 1,
        };
        assert!(node.validate(&expected).is_ok());
        node.depth = 2;
        assert!(node.validate(&expected).is_err());
        let mut manifest = manifest(Uuid::now_v7());
        manifest.coverage.gaps.push("tool_result_missing".into());
        assert!(manifest.validate(manifest.tenant_id).is_err());
        manifest.coverage.projection_state = SemanticProjectionState::Partial;
        assert!(manifest.validate(manifest.tenant_id).is_ok());
    }
}
