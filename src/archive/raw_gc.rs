//! Read-only raw retirement inventory. A candidate is never deletion authority.
use std::collections::BTreeMap;

use serde::Serialize;
use uuid::Uuid;

use super::{ArchiveStore, CasArchiveObject, SemanticPublication};
use crate::error::AppError;

#[derive(Clone, Serialize)]
pub struct RawGcCandidate {
    pub tenant_id: Uuid,
    pub source: CasArchiveObject,
    /// Physical object bytes, which differ from logical bytes for compression.
    pub physical_bytes: u64,
    pub e_tag: Option<String>,
    pub object_version: Option<String>,
    pub retired_sides: u64,
    /// This inventory deliberately cannot grant permission to delete.
    pub pending_reason: &'static str,
}

#[derive(Default, Serialize)]
pub struct RawGcCandidateBatch {
    pub candidates: Vec<RawGcCandidate>,
    pub unique_physical_bytes: u64,
    pub requests_without_retirement: Vec<Uuid>,
}

impl ArchiveStore {
    /// Uses the existing validated retirement receipt and backend routing. It
    /// performs HEAD only on body objects and never downloads or deletes them.
    pub(crate) async fn inventory_retired_raw(
        &self,
        publication: &SemanticPublication,
    ) -> Result<Vec<RawGcCandidate>, AppError> {
        let mut candidates = Vec::new();
        for side in ["request", "response"] {
            let Some(fence) = self
                .raw_retirement(publication.tenant_id, publication.request_id, side)
                .await?
            else {
                continue;
            };
            if fence.semantic_revision != publication.revision {
                return Err(AppError::storage("raw_gc_retirement_revision_mismatch"));
            }
            let (store, path) = self.resolve(&fence.source.object_locator)?;
            let metadata = store.head(&path).await?;
            candidates.push(RawGcCandidate {
                tenant_id: publication.tenant_id,
                source: fence.source,
                physical_bytes: metadata.size,
                e_tag: metadata.e_tag,
                object_version: metadata.version,
                retired_sides: 1,
                pending_reason: "full_root_proof_and_writer_fence_required",
            });
        }
        Ok(candidates)
    }
}

impl RawGcCandidateBatch {
    pub(crate) fn from_candidates(
        candidates: impl IntoIterator<Item = RawGcCandidate>,
        requests_without_retirement: Vec<Uuid>,
    ) -> Result<Self, AppError> {
        // A locator carries the validated backend identity and encoding. The
        // same logical hash on another backend is another physical object.
        let mut unique: BTreeMap<(Uuid, String), RawGcCandidate> = BTreeMap::new();
        for candidate in candidates {
            let key = (candidate.tenant_id, candidate.source.object_locator.clone());
            if let Some(existing) = unique.get_mut(&key) {
                if existing.source != candidate.source
                    || existing.physical_bytes != candidate.physical_bytes
                    || existing.e_tag != candidate.e_tag
                    || existing.object_version != candidate.object_version
                {
                    return Err(AppError::storage("raw_gc_inventory_changed"));
                }
                existing.retired_sides = existing
                    .retired_sides
                    .checked_add(candidate.retired_sides)
                    .ok_or_else(|| AppError::storage("raw_gc_inventory_overflow"))?;
            } else {
                unique.insert(key, candidate);
            }
        }
        let unique_physical_bytes = unique.values().try_fold(0_u64, |sum, value| {
            sum.checked_add(value.physical_bytes)
                .ok_or_else(|| AppError::storage("raw_gc_inventory_overflow"))
        })?;
        Ok(Self {
            candidates: unique.into_values().collect(),
            unique_physical_bytes,
            requests_without_retirement,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(tenant: Uuid, locator: &str, bytes: u64) -> RawGcCandidate {
        RawGcCandidate {
            tenant_id: tenant,
            source: CasArchiveObject {
                object_locator: locator.into(),
                blake3_digest: "a".repeat(64),
                size_bytes: 100,
            },
            physical_bytes: bytes,
            e_tag: Some("version-one".into()),
            object_version: None,
            retired_sides: 1,
            pending_reason: "full_root_proof_and_writer_fence_required",
        }
    }

    #[test]
    fn raw_gc_inventory_counts_physical_unique_objects_without_granting_delete() {
        let tenant = Uuid::now_v7();
        let first = candidate(tenant, "legacy-object", 37);
        let other_backend = candidate(tenant, "training-object", 41);
        let other_tenant = candidate(Uuid::now_v7(), "legacy-object", 37);
        let report = RawGcCandidateBatch::from_candidates(
            [first.clone(), first, other_backend, other_tenant],
            Vec::new(),
        )
        .unwrap();
        assert_eq!(report.candidates.len(), 3);
        assert_eq!(report.unique_physical_bytes, 115);
        assert_eq!(
            report
                .candidates
                .iter()
                .map(|c| c.retired_sides)
                .sum::<u64>(),
            4
        );
        assert!(
            report
                .candidates
                .iter()
                .all(|c| c.pending_reason == "full_root_proof_and_writer_fence_required")
        );
    }

    #[test]
    fn raw_gc_inventory_rejects_changed_object_version_instead_of_undercounting() {
        let original = candidate(Uuid::now_v7(), "same-object", 37);
        let mut changed = original.clone();
        changed.e_tag = Some("new-version".into());
        assert!(RawGcCandidateBatch::from_candidates([original, changed], Vec::new()).is_err());
    }
}
