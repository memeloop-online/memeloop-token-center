//! One interpretation of retired raw ownership for readers and GC marks.
//! This is reference classification, never permission to physically delete.
use super::{ArchiveStore, CasArchiveObject, SemanticArchiveAuthority, SemanticProjectionState};
use crate::error::AppError;
use uuid::Uuid;

#[derive(Clone, Debug)]
pub struct CanonicalRawProof {
    pub authority: SemanticArchiveAuthority,
    pub source: CasArchiveObject,
    pub release_generation: Uuid,
}

#[derive(Clone, Debug)]
pub enum RawReaderAuthority {
    Canonical(CanonicalRawProof),
    LegacyBound,
    /// Missing binding or unequal source remains a live/unknown obligation.
    Unknown,
}

impl ArchiveStore {
    /// The caller supplies the exact authorized published semantic binding.
    /// A legacy locator can be released only when it names this receipt's
    /// source; a matching hash on a different backend is insufficient.
    pub async fn raw_reader_authority(
        &self,
        tenant: Uuid,
        request: Uuid,
        side: &str,
        bound_locator: Option<&str>,
        candidate: Option<&CasArchiveObject>,
        authority: Option<&SemanticArchiveAuthority>,
    ) -> Result<RawReaderAuthority, AppError> {
        if !matches!(side, "request" | "response") {
            return Err(AppError::BadRequest("invalid raw archive side".into()));
        }
        if let Some(candidate) = candidate {
            candidate.validate_tenant(tenant)?;
        }
        let Some(retirement) = self.raw_retirement(tenant, request, side).await? else {
            return Ok(RawReaderAuthority::LegacyBound);
        };
        let Some(authority) = authority else {
            return Ok(RawReaderAuthority::Unknown);
        };
        if authority.publication.tenant_id != tenant
            || authority.publication.request_id != request
            || authority.publication.revision != retirement.semantic_revision
            || authority.semantic_head.node_hash != retirement.semantic_leaf
            || authority.coverage.projection_state != SemanticProjectionState::Complete
            || !authority.coverage.observed_terminal
            || !authority.coverage.gaps.is_empty()
            || !authority.coverage.omissions.is_empty()
        {
            return Ok(RawReaderAuthority::Unknown);
        }
        if bound_locator.is_some_and(|locator| locator != retirement.source.object_locator)
            || candidate.is_some_and(|candidate| candidate != &retirement.source)
        {
            return Ok(RawReaderAuthority::Unknown);
        }
        Ok(RawReaderAuthority::Canonical(CanonicalRawProof {
            authority: authority.clone(),
            source: retirement.source,
            release_generation: retirement.release_generation,
        }))
    }
}
