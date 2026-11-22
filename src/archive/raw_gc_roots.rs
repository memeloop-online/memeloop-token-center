//! Typed canonical edges for a raw-GC mark pass. This is one root family, not
//! an exhaustive proof. Raw provenance never substitutes for reader ownership.
use serde::{
    Deserialize, Serialize,
    de::{Error as _, IgnoredAny, MapAccess, Visitor},
};

use super::{ArchiveStore, CasArchiveObject, SemanticHead, SemanticPublication};
use crate::error::AppError;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AtomReferences {
    role: String,
    parts: Vec<PartReferences>,
}

struct PartReferences {
    object_locator: Option<String>,
    content_blake3: Option<String>,
    byte_length: Option<u64>,
}

impl<'de> Deserialize<'de> for PartReferences {
    fn deserialize<D: serde::Deserializer<'de>>(decoder: D) -> Result<Self, D::Error> {
        struct ReferencesVisitor;
        impl<'de> Visitor<'de> for ReferencesVisitor {
            type Value = PartReferences;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("canonical part reference metadata")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut result = PartReferences {
                    object_locator: None,
                    content_blake3: None,
                    byte_length: None,
                };
                let mut kind_seen = false;
                let mut fields = std::collections::BTreeSet::new();
                while let Some(key) = map.next_key::<String>()? {
                    if !fields.insert(key.clone()) {
                        return Err(M::Error::custom("duplicate canonical part field"));
                    }
                    match key.as_str() {
                        "object_locator" => result.object_locator = map.next_value()?,
                        "content_blake3" => result.content_blake3 = map.next_value()?,
                        "byte_length" => result.byte_length = map.next_value()?,
                        "kind" => {
                            let kind: String = map.next_value()?;
                            if !matches!(
                                kind.as_str(),
                                "text"
                                    | "reasoning"
                                    | "tool_call"
                                    | "tool_result"
                                    | "image"
                                    | "audio"
                                    | "video"
                                    | "file"
                                    | "media"
                            ) {
                                return Err(M::Error::custom("unknown canonical part kind"));
                            }
                            kind_seen = true;
                        }
                        "part_index" | "text" | "media_type" | "call_id" | "tool_call_id"
                        | "reasoning_source" | "tool_name" | "arguments" | "arguments_text"
                        | "arguments_encoding" | "completeness" | "result_content"
                        | "result_status" => {
                            map.next_value::<IgnoredAny>()?;
                        }
                        _ => {
                            return Err(M::Error::custom("unknown canonical part reference field"));
                        }
                    }
                }
                if !kind_seen {
                    return Err(M::Error::custom("canonical part kind missing"));
                }
                Ok(result)
            }
        }
        decoder.deserialize_map(ReferencesVisitor)
    }
}

impl PartReferences {
    fn object(self) -> Result<Option<CasArchiveObject>, AppError> {
        match (self.object_locator, self.content_blake3, self.byte_length) {
            (None, None, _) => Ok(None),
            (Some(object_locator), Some(blake3_digest), Some(size_bytes)) => {
                Ok(Some(CasArchiveObject {
                    object_locator,
                    blake3_digest,
                    size_bytes,
                }))
            }
            _ => Err(AppError::storage("raw_gc_media_reference_incomplete")),
        }
    }
}

#[derive(Serialize)]
pub struct RawGcReferenceEvidence {
    pub authority: &'static str,
    pub request_id: uuid::Uuid,
    pub revision: Option<u64>,
    pub object: CasArchiveObject,
    pub candidate: CasArchiveObject,
}

fn mark(
    publication: &SemanticPublication,
    object: &CasArchiveObject,
    candidates: &[CasArchiveObject],
    evidence: &mut Vec<RawGcReferenceEvidence>,
    authority: &'static str,
) -> Result<(), AppError> {
    mark_object(
        publication.tenant_id,
        publication.request_id,
        Some(publication.revision),
        object,
        candidates,
        evidence,
        authority,
    )
}

fn mark_object(
    tenant: uuid::Uuid,
    request: uuid::Uuid,
    revision: Option<u64>,
    object: &CasArchiveObject,
    candidates: &[CasArchiveObject],
    evidence: &mut Vec<RawGcReferenceEvidence>,
    authority: &'static str,
) -> Result<(), AppError> {
    object.validate_tenant(tenant)?;
    for candidate in candidates {
        // Backend aliases may address the same physical namespace. Until
        // equivalence is proven, retain matching content on every backend.
        if candidate.blake3_digest != object.blake3_digest {
            continue;
        }
        if candidate.size_bytes != object.size_bytes {
            return Err(AppError::storage("raw_gc_reference_identity_conflict"));
        }
        evidence.push(RawGcReferenceEvidence {
            authority,
            request_id: request,
            revision,
            object: object.clone(),
            candidate: candidate.clone(),
        });
    }
    Ok(())
}

impl ArchiveStore {
    /// Legacy manifests still own raw captures. Semantic retirement alone does
    /// not release these independent roots or their media/prefix dependencies.
    pub async fn trace_legacy_raw_gc_references(
        &self,
        tenant: uuid::Uuid,
        request: uuid::Uuid,
        candidates: &[CasArchiveObject],
    ) -> Result<Vec<RawGcReferenceEvidence>, AppError> {
        let object = self
            .request_conversation_reference(tenant, request)
            .await?
            .ok_or_else(|| AppError::storage("raw_gc_legacy_root_missing"))?;
        for candidate in candidates {
            candidate.validate_tenant(tenant)?;
        }
        let manifest = self
            .get_conversation_manifest_streamed(tenant, &object)
            .await?;
        let mut evidence = Vec::new();
        mark_object(
            tenant,
            request,
            None,
            &object,
            candidates,
            &mut evidence,
            "legacy_manifest",
        )?;
        self.visit_conversation_references(&manifest, |object, authority| {
            mark_object(
                tenant,
                request,
                None,
                object,
                candidates,
                &mut evidence,
                authority,
            )
        })
        .await?;
        Ok(evidence)
    }

    /// Traverse one exact immutable publication using the existing decoders and
    /// prefix identity. Only candidate hits are retained; body text is never
    /// collected. All revisions (including partial ones) must be supplied by the
    /// caller's complete root frontier before it can claim an exhaustive pass.
    pub async fn trace_semantic_raw_gc_references(
        &self,
        publication: &SemanticPublication,
        candidates: &[CasArchiveObject],
    ) -> Result<Vec<RawGcReferenceEvidence>, AppError> {
        let manifest = self
            .get_semantic_manifest_streamed(publication.tenant_id, &publication.manifest)
            .await?;
        if publication.format_version != 2
            || publication.tenant_id != manifest.tenant_id
            || publication.request_id != manifest.request_id
            || publication.revision != manifest.revision
            || publication.projection_version != manifest.projection_version
        {
            return Err(AppError::storage("semantic_publication_identity_mismatch"));
        }
        for candidate in candidates {
            candidate.validate_tenant(publication.tenant_id)?;
        }
        let mut evidence = Vec::new();
        mark(
            publication,
            &publication.manifest,
            candidates,
            &mut evidence,
            "semantic_manifest",
        )?;
        let mut cursor = manifest.instances_head.clone();
        let mut expected = manifest.semantic_head.clone();
        while let Some(reference) = cursor {
            mark(
                publication,
                &reference,
                candidates,
                &mut evidence,
                "semantic_instances",
            )?;
            let page = self
                .get_semantic_instance_page(&manifest, &reference)
                .await?;
            let end = page
                .first_ordinal
                .checked_add(page.instances.len() as u64)
                .ok_or_else(|| AppError::storage("semantic_view_page_anchor_invalid"))?;
            if expected.as_ref().map(|head| head.depth) != Some(end) {
                return Err(AppError::storage("semantic_view_page_anchor_invalid"));
            }
            for instance in page.instances.iter().rev() {
                let head = expected
                    .as_ref()
                    .ok_or_else(|| AppError::storage("semantic_prefix_node_invalid"))?;
                if instance.node_hash != head.node_hash
                    || instance.ordinal.checked_add(1) != Some(head.depth)
                {
                    return Err(AppError::storage("semantic_prefix_node_invalid"));
                }
                let node = self.get_semantic_prefix_node(&manifest, head).await?;
                if node.atom_hash != instance.atom_hash {
                    return Err(AppError::storage("semantic_atom_identity_mismatch"));
                }
                mark(
                    publication,
                    &instance.body,
                    candidates,
                    &mut evidence,
                    "semantic_atom",
                )?;
                let atom: AtomReferences = self
                    .read_tenant_json_streamed(publication.tenant_id, &instance.body)
                    .await?;
                if atom.role.is_empty() {
                    return Err(AppError::storage("semantic_atom_invalid"));
                }
                for part in atom.parts {
                    if let Some(object) = part.object()? {
                        mark(
                            publication,
                            &object,
                            candidates,
                            &mut evidence,
                            "semantic_media",
                        )?;
                    }
                }
                expected = node.parent_hash.map(|node_hash| SemanticHead {
                    node_hash,
                    depth: node.depth - 1,
                });
            }
            cursor = page.previous;
        }
        if expected.is_some() {
            return Err(AppError::storage("conversation_prefix_incomplete"));
        }
        Ok(evidence)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gc_part_parser_skips_body_values_and_rejects_unknown_reference_schema() {
        let large = "x".repeat(1024 * 1024);
        let body = format!(
            r#"{{"role":"assistant","parts":[{{"kind":"tool_call","arguments":{{"text":"{large}"}},"tool_name":"echo","text":null}}]}}"#
        );
        let atom: AtomReferences = serde_json::from_str(&body).unwrap();
        assert!(
            atom.parts
                .into_iter()
                .next()
                .unwrap()
                .object()
                .unwrap()
                .is_none()
        );
        for body in [
            r#"{"role":"assistant","parts":[{"kind":"future_media","text":"unrecognized"}]}"#,
            r#"{"role":"assistant","parts":[{"kind":"text","future_object":{"object_locator":"unknown"}}]}"#,
            r#"{"role":"assistant","parts":[{"kind":"text","kind":"text"}]}"#,
        ] {
            assert!(serde_json::from_str::<AtomReferences>(body).is_err());
        }
    }
}
