use bytes::Bytes;
use object_store::{ObjectStoreExt, PutMode, PutOptions, PutPayload};
use uuid::Uuid;

use super::{ArchiveStore, CasArchiveObject, path::archive_path};
use crate::error::AppError;

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawResponseAttempt {
    pub outbound_attempt: usize,
    pub upstream_account_id: Uuid,
    pub model_route_id: Uuid,
    pub status_code: u16,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawResponseAttemptCapture {
    pub format_version: u8,
    pub attempt: RawResponseAttempt,
    pub response: CasArchiveObject,
}

fn attempt_root(tenant: Uuid, request: Uuid, ordinal: usize) -> Result<String, AppError> {
    if ordinal == 0 {
        return Err(AppError::BadRequest("invalid outbound attempt".into()));
    }
    Ok(format!(
        "tenants/{tenant}/raw/v1/{request}/attempts/{ordinal}/response.json"
    ))
}

fn root(tenant: Uuid, request: Uuid, side: &str) -> Result<String, AppError> {
    if !matches!(side, "request" | "response") {
        return Err(AppError::BadRequest("invalid raw archive side".into()));
    }
    Ok(format!("tenants/{tenant}/raw/v1/{request}/{side}.json"))
}

/// A permanent, small retirement fence. Its source is provenance only: this
/// metadata does not keep a retired raw CAS body alive in a GC reference set.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawRetirement {
    pub format_version: u32,
    pub tenant_id: Uuid,
    pub request_id: Uuid,
    pub side: String,
    pub source: CasArchiveObject,
    pub semantic_revision: u64,
    pub semantic_leaf: String,
    pub release_generation: Uuid,
}

fn retirement_root(tenant: Uuid, request: Uuid, side: &str) -> Result<String, AppError> {
    root(tenant, request, side)?;
    Ok(format!(
        "tenants/{tenant}/semantic/v2/requests/{request}/retired-{side}.json"
    ))
}

#[derive(Clone, Debug)]
pub(crate) enum RawPublicationSettlement {
    Published,
    Retired {
        semantic_revision: u64,
        semantic_leaf: String,
    },
}

impl ArchiveStore {
    /// Retire only the small raw-root reference. Shared CAS bytes are retained
    /// until physical GC separately proves all reader/publication/history refs.
    pub(crate) async fn retire_covered_raw_roots(
        &self,
        publication: &super::SemanticPublication,
        binding: &super::SemanticBindingProof,
    ) -> Result<(), AppError> {
        self.confirm_semantic_coverage(publication, binding).await?;
        let manifest = self
            .get_semantic_manifest_streamed(publication.tenant_id, &publication.manifest)
            .await?;
        let sources = manifest
            .raw_sources
            .ok_or_else(|| AppError::storage("raw_retirement_source_proof_missing"))?;
        for (side, source) in [
            ("request", Some(sources.request)),
            ("response", sources.response),
        ] {
            let Some(source) = source else {
                continue;
            };
            let fence = match self
                .raw_retirement(publication.tenant_id, publication.request_id, side)
                .await?
            {
                Some(existing) => {
                    if existing.source != source || existing.semantic_leaf != binding.node_hash {
                        return Err(AppError::Conflict(
                            "raw retirement identity conflict".into(),
                        ));
                    }
                    existing
                }
                None => RawRetirement {
                    format_version: 1,
                    tenant_id: publication.tenant_id,
                    request_id: publication.request_id,
                    side: side.into(),
                    source: source.clone(),
                    semantic_revision: publication.revision,
                    semantic_leaf: binding.node_hash.clone(),
                    release_generation: Uuid::from_u128(publication.revision as u128),
                },
            };
            let root_key = root(fence.tenant_id, fence.request_id, side)?;
            let path = archive_path(&root_key)?;
            let metadata = self.inner.head(&path).await?;
            let original = self.get_bounded(&root_key, 4096).await?;
            let marker = Bytes::from(
                serde_json::to_vec(&serde_json::json!({
                    "format_version":1,"retired":true,"release_generation":fence.release_generation
                }))
                .map_err(|_| AppError::internal("raw_retirement_encode"))?,
            );
            if original == marker {
                continue;
            }
            let current: CasArchiveObject = serde_json::from_slice(&original)
                .map_err(|_| AppError::storage("raw_capture_reference_invalid"))?;
            if current != source {
                return Err(AppError::Conflict("raw retirement root changed".into()));
            }
            if metadata.e_tag.is_none() && metadata.version.is_none() {
                return Err(AppError::storage(
                    "raw_retirement_conditional_update_unavailable",
                ));
            }
            self.restore_durable_root(
                &retirement_root(fence.tenant_id, fence.request_id, side)?,
                Bytes::from(
                    serde_json::to_vec(&fence)
                        .map_err(|_| AppError::internal("raw_retirement_encode"))?,
                ),
            )
            .await?;
            // This comparison is performed by the backend. GET followed by an
            // unconditional DELETE would allow a concurrent relocation to lose.
            let result = self
                .inner
                .put_opts(
                    &path,
                    PutPayload::from_bytes(marker.clone()),
                    PutOptions {
                        mode: PutMode::Update(object_store::UpdateVersion {
                            e_tag: metadata.e_tag,
                            version: metadata.version,
                        }),
                        ..Default::default()
                    },
                )
                .await;
            match result {
                Ok(_) => {}
                Err(error) => {
                    // Lost ACK is success only after exact marker readback.
                    // A conflicting root leaves the durable obligation pending.
                    if self.get_bounded(&root_key, 4096).await? != marker {
                        return Err(error.into());
                    }
                }
            }
            #[cfg(test)]
            crate::db::Database::semantic_coverage_test_boundary("raw_retirement_marker_ack")?;
        }
        Ok(())
    }

    /// Settle a late publisher explicitly against the durable semantic
    /// retirement obligation. This is never acknowledgement of a new raw root.
    pub(crate) async fn settle_raw_capture(
        &self,
        tenant: Uuid,
        request: Uuid,
        side: &str,
        object: &CasArchiveObject,
    ) -> Result<RawPublicationSettlement, AppError> {
        if let Some(retired) = self.raw_retirement(tenant, request, side).await? {
            if retired.source != *object {
                return Err(AppError::Conflict(
                    "raw retired source identity conflict".into(),
                ));
            }
            return Ok(RawPublicationSettlement::Retired {
                semantic_revision: retired.semantic_revision,
                semantic_leaf: retired.semantic_leaf,
            });
        }
        match self
            .publish_raw_capture(tenant, request, side, object)
            .await
        {
            Ok(()) => Ok(RawPublicationSettlement::Published),
            Err(error) => {
                // A retirement may have won while the old PUT was in flight.
                // Re-read its immutable identity rather than retrying forever.
                if let Some(retired) = self.raw_retirement(tenant, request, side).await? {
                    if retired.source == *object {
                        return Ok(RawPublicationSettlement::Retired {
                            semantic_revision: retired.semantic_revision,
                            semantic_leaf: retired.semantic_leaf,
                        });
                    }
                }
                Err(error)
            }
        }
    }

    pub(crate) async fn raw_capture_is_retired(
        &self,
        tenant: Uuid,
        request: Uuid,
        side: &str,
    ) -> Result<bool, AppError> {
        Ok(self.raw_retirement(tenant, request, side).await?.is_some())
    }

    async fn raw_retirement(
        &self,
        tenant: Uuid,
        request: Uuid,
        side: &str,
    ) -> Result<Option<RawRetirement>, AppError> {
        let key = retirement_root(tenant, request, side)?;
        match self.inner.head(&archive_path(&key)?).await {
            Err(object_store::Error::NotFound { .. }) => return Ok(None),
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
        let bytes = self.get_bounded(&key, 4096).await?;
        let fence: RawRetirement = serde_json::from_slice(&bytes)
            .map_err(|_| AppError::storage("raw_retirement_invalid"))?;
        fence.source.validate_tenant(tenant)?;
        if fence.format_version != 1
            || fence.tenant_id != tenant
            || fence.request_id != request
            || fence.side != side
            || fence.semantic_revision == 0
            || blake3::Hash::from_hex(&fence.semantic_leaf).is_err()
            || fence.release_generation.is_nil()
        {
            return Err(AppError::storage("raw_retirement_invalid"));
        }
        // The fence is authoritative only together with the immutable exact
        // source coverage receipt. A projected row or a root-exists bit cannot
        // substitute for this handoff proof.
        let receipt = self
            .get_bounded(
                &format!(
                    "tenants/{tenant}/semantic/v2/requests/{request}/{}-coverage.json",
                    fence.semantic_revision
                ),
                4096,
            )
            .await?;
        let receipt: serde_json::Value = serde_json::from_slice(&receipt)
            .map_err(|_| AppError::storage("raw_retirement_coverage_invalid"))?;
        let publication = &receipt["publication"];
        let binding = &receipt["binding"];
        let coverage = &receipt["coverage"];
        if publication["tenant_id"].as_str() != Some(tenant.to_string().as_str())
            || publication["request_id"].as_str() != Some(request.to_string().as_str())
            || publication["revision"].as_u64() != Some(fence.semantic_revision)
            || binding["node_hash"].as_str() != Some(fence.semantic_leaf.as_str())
            || binding["tenant_id"] != publication["tenant_id"]
            || binding["request_id"] != publication["request_id"]
            || coverage["observed_terminal"].as_bool() != Some(true)
            || coverage["projection_state"].as_str() != Some("complete")
            || !coverage["gaps"].as_array().is_some_and(Vec::is_empty)
            || receipt["raw_sources"][side]
                != serde_json::to_value(&fence.source)
                    .map_err(|_| AppError::storage("raw_retirement_invalid"))?
        {
            return Err(AppError::storage("raw_retirement_coverage_invalid"));
        }
        Ok(Some(fence))
    }

    // Both checks are necessary: retirement may race an already in-flight PUT.
    // A retired publication is not acknowledged as a new durable raw capture.
    async fn ensure_raw_not_retired(
        &self,
        tenant: Uuid,
        request: Uuid,
        side: &str,
        object: &CasArchiveObject,
    ) -> Result<(), AppError> {
        if let Some(fence) = self.raw_retirement(tenant, request, side).await? {
            let reason = if fence.source == *object {
                "raw capture retired"
            } else {
                "raw retired source identity conflict"
            };
            return Err(AppError::Conflict(reason.into()));
        }
        Ok(())
    }

    pub async fn publish_response_attempt(
        &self,
        tenant: Uuid,
        request: Uuid,
        attempt: RawResponseAttempt,
        object: &CasArchiveObject,
    ) -> Result<(), AppError> {
        self.verify_tenant_content(tenant, object).await?;
        let key = attempt_root(tenant, request, attempt.outbound_attempt)?;
        let capture = RawResponseAttemptCapture {
            format_version: 1,
            attempt,
            response: object.clone(),
        };
        let encoded =
            serde_json::to_vec(&capture).map_err(|_| AppError::internal("raw_attempt_encode"))?;
        match self
            .inner
            .put_opts(
                &archive_path(&key)?,
                PutPayload::from_bytes(Bytes::from(encoded)),
                PutOptions {
                    mode: PutMode::Create,
                    ..Default::default()
                },
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(object_store::Error::AlreadyExists { .. }) => {
                if self
                    .response_attempt(tenant, request, attempt.outbound_attempt)
                    .await?
                    .as_ref()
                    == Some(&capture)
                {
                    Ok(())
                } else {
                    Err(AppError::Conflict("raw attempt content conflict".into()))
                }
            }
            Err(error) => Err(error.into()),
        }
    }

    pub async fn response_attempt(
        &self,
        tenant: Uuid,
        request: Uuid,
        ordinal: usize,
    ) -> Result<Option<RawResponseAttemptCapture>, AppError> {
        let key = attempt_root(tenant, request, ordinal)?;
        match self.inner.head(&archive_path(&key)?).await {
            Err(object_store::Error::NotFound { .. }) => return Ok(None),
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
        let encoded = self.get_bounded(&key, 4096).await?;
        let capture: RawResponseAttemptCapture = serde_json::from_slice(&encoded)
            .map_err(|_| AppError::storage("raw_attempt_reference_invalid"))?;
        if capture.format_version != 1 || capture.attempt.outbound_attempt != ordinal {
            return Err(AppError::storage("raw_attempt_reference_invalid"));
        }
        capture.response.validate_tenant(tenant)?;
        Ok(Some(capture))
    }

    pub(crate) async fn publish_response_attempt_failure(
        &self,
        tenant: Uuid,
        request: Uuid,
        attempt: RawResponseAttempt,
    ) -> Result<(), AppError> {
        let key = format!(
            "{}.failed",
            attempt_root(tenant, request, attempt.outbound_attempt)?
        );
        let encoded = serde_json::to_vec(&attempt)
            .map_err(|_| AppError::internal("raw_attempt_failure_encode"))?;
        match self
            .inner
            .put_opts(
                &archive_path(&key)?,
                PutPayload::from_bytes(Bytes::from(encoded)),
                PutOptions {
                    mode: PutMode::Create,
                    ..Default::default()
                },
            )
            .await
        {
            Ok(_) | Err(object_store::Error::AlreadyExists { .. }) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub async fn publish_raw_capture_failure(
        &self,
        tenant: Uuid,
        request: Uuid,
        side: &str,
    ) -> Result<(), AppError> {
        let key = format!("{}.failed", root(tenant, request, side)?);
        match self
            .inner
            .put_opts(
                &archive_path(&key)?,
                PutPayload::from_bytes(Bytes::from_static(
                    br#"{"format_version":1,"reason":"capture_failed"}"#,
                )),
                PutOptions {
                    mode: PutMode::Create,
                    ..Default::default()
                },
            )
            .await
        {
            Ok(_) | Err(object_store::Error::AlreadyExists { .. }) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub async fn raw_capture_failed(
        &self,
        tenant: Uuid,
        request: Uuid,
        side: &str,
    ) -> Result<bool, AppError> {
        let key = format!("{}.failed", root(tenant, request, side)?);
        match self.inner.head(&archive_path(&key)?).await {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    pub async fn publish_raw_capture(
        &self,
        tenant: Uuid,
        request: Uuid,
        side: &str,
        object: &CasArchiveObject,
    ) -> Result<(), AppError> {
        self.ensure_raw_not_retired(tenant, request, side, object)
            .await?;
        if self
            .verified_published_raw_capture(tenant, request, side, object)
            .await?
            .is_some()
        {
            return self
                .ensure_raw_not_retired(tenant, request, side, object)
                .await;
        }
        self.verify_tenant_content(tenant, object).await?;
        let key = root(tenant, request, side)?;
        let encoded = serde_json::to_vec(object)
            .map_err(|_| AppError::internal("raw_capture_reference_encode"))?;
        match self
            .inner
            .put_opts(
                &archive_path(&key)?,
                PutPayload::from_bytes(Bytes::from(encoded)),
                PutOptions {
                    mode: PutMode::Create,
                    ..Default::default()
                },
            )
            .await
        {
            Ok(_) => {
                self.ensure_raw_not_retired(tenant, request, side, object)
                    .await
            }
            Err(object_store::Error::AlreadyExists { .. }) => {
                if self
                    .verified_published_raw_capture(tenant, request, side, object)
                    .await?
                    .is_some()
                {
                    self.ensure_raw_not_retired(tenant, request, side, object)
                        .await
                } else {
                    Err(AppError::Conflict(
                        "raw archive root content conflict".into(),
                    ))
                }
            }
            Err(error) => Err(error.into()),
        }
    }

    // The immutable root owns the logical body; migration may relocate its
    // physical locator. Replays verify the published body, not a retired copy.
    pub(crate) async fn verified_published_raw_capture(
        &self,
        tenant: Uuid,
        request: Uuid,
        side: &str,
        proposed: &CasArchiveObject,
    ) -> Result<Option<CasArchiveObject>, AppError> {
        proposed.validate_tenant(tenant)?;
        self.resolve(&proposed.object_locator)?;
        let Some(existing) = self.raw_capture_reference(tenant, request, side).await? else {
            return Ok(None);
        };
        if existing.blake3_digest != proposed.blake3_digest
            || existing.size_bytes != proposed.size_bytes
        {
            return Err(AppError::Conflict(
                "raw archive root content conflict".into(),
            ));
        }
        self.verify_tenant_content(tenant, &existing).await?;
        Ok(Some(existing))
    }

    pub async fn raw_capture_reference(
        &self,
        tenant: Uuid,
        request: Uuid,
        side: &str,
    ) -> Result<Option<CasArchiveObject>, AppError> {
        if self.raw_retirement(tenant, request, side).await?.is_some() {
            return Ok(None);
        }
        let key = root(tenant, request, side)?;
        match self.inner.head(&archive_path(&key)?).await {
            Err(object_store::Error::NotFound { .. }) => return Ok(None),
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
        let encoded = self.get_bounded(&key, 4096).await?;
        let reference: CasArchiveObject = serde_json::from_slice(&encoded)
            .map_err(|_| AppError::storage("raw_capture_reference_invalid"))?;
        reference.validate_tenant(tenant)?;
        if self.raw_retirement(tenant, request, side).await?.is_some() {
            return Ok(None);
        }
        Ok(Some(reference))
    }
}

#[cfg(test)]
mod retirement_contracts {
    use super::*;

    async fn retire_complete_sources_contract(archive: ArchiveStore) {
        let tenant = Uuid::now_v7();
        let request = Uuid::now_v7();
        let input = archive
            .put_tenant_content(
                tenant,
                Bytes::from_static(
                    br#"{"messages":[{"role":"user","content":"shared semantic prefix"}]}"#,
                ),
            )
            .await
            .unwrap();
        let response = archive.put_tenant_content(tenant, Bytes::from_static(
            br#"{"choices":[{"message":{"role":"assistant","content":"complete answer"},"finish_reason":"stop"}]}"#
        )).await.unwrap();
        let prepared = archive
            .prepare_session_body_streamed(
                tenant,
                request,
                crate::conversation::ConversationHints::default(),
                input.object_locator.clone(),
                Some(response.object_locator.clone()),
                true,
            )
            .await
            .unwrap()
            .unwrap();
        assert!(prepared.coverage_complete);
        let proof = super::super::SemanticBindingProof {
            tenant_id: tenant,
            request_id: request,
            node_hash: prepared.nodes.last().unwrap().node_hash.clone(),
            projected_at: 1,
        };
        let path = archive_path(&root(tenant, request, "request").unwrap()).unwrap();
        let original_meta = archive.inner.head(&path).await.unwrap();
        let other = archive
            .put_tenant_content(
                tenant,
                Bytes::from_static(b"concurrent relocation identity"),
            )
            .await
            .unwrap();
        archive
            .inner
            .put_opts(
                &path,
                PutPayload::from_bytes(Bytes::from(serde_json::to_vec(&other).unwrap())),
                PutOptions {
                    mode: PutMode::Update(object_store::UpdateVersion {
                        e_tag: original_meta.e_tag.clone(),
                        version: original_meta.version.clone(),
                    }),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        // A stale ETag must really be rejected by the selected backend.
        assert!(
            archive
                .inner
                .put_opts(
                    &path,
                    PutPayload::from_bytes(Bytes::from_static(b"stale marker")),
                    PutOptions {
                        mode: PutMode::Update(object_store::UpdateVersion {
                            e_tag: original_meta.e_tag,
                            version: original_meta.version
                        }),
                        ..Default::default()
                    }
                )
                .await
                .is_err()
        );
        assert!(
            archive
                .retire_covered_raw_roots(&prepared.publication, &proof)
                .await
                .is_err()
        );
        assert_eq!(
            archive
                .raw_capture_reference(tenant, request, "request")
                .await
                .unwrap(),
            Some(other)
        );
        let current = archive.inner.head(&path).await.unwrap();
        archive
            .inner
            .put_opts(
                &path,
                PutPayload::from_bytes(Bytes::from(serde_json::to_vec(&input).unwrap())),
                PutOptions {
                    mode: PutMode::Update(object_store::UpdateVersion {
                        e_tag: current.e_tag,
                        version: current.version,
                    }),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        archive
            .retire_covered_raw_roots(&prepared.publication, &proof)
            .await
            .unwrap();
        archive
            .retire_covered_raw_roots(&prepared.publication, &proof)
            .await
            .unwrap();
        for (side, object) in [("request", input), ("response", response)] {
            assert!(
                archive
                    .raw_capture_reference(tenant, request, side)
                    .await
                    .unwrap()
                    .is_none()
            );
            assert!(matches!(
                archive
                    .settle_raw_capture(tenant, request, side, &object)
                    .await
                    .unwrap(),
                RawPublicationSettlement::Retired { .. }
            ));
            archive
                .verify_tenant_content(tenant, &object)
                .await
                .unwrap();
            let marker = archive
                .get_bounded(&root(tenant, request, side).unwrap(), 4096)
                .await
                .unwrap();
            let marker: serde_json::Value = serde_json::from_slice(&marker).unwrap();
            assert_eq!(marker["retired"], true);
            assert!(marker.get("object_locator").is_none());
        }
    }

    #[tokio::test]
    async fn complete_coverage_retires_exact_roots_and_rejects_stale_backend_versions() {
        retire_complete_sources_contract(ArchiveStore::memory_for_test()).await;
    }

    #[tokio::test]
    #[ignore = "requires isolated RustFS; executed by the exact retirement GHA profile"]
    async fn rustfs_complete_coverage_retires_exact_roots_and_rejects_stale_backend_versions() {
        let mut config = crate::config::Config::for_test("sqlite::memory:".into());
        config.archive_backend = crate::config::ArchiveBackend::S3;
        config.s3_endpoint =
            Some(std::env::var("MTC_RUSTFS_TEST_ENDPOINT").expect("RustFS endpoint required"));
        config.s3_bucket =
            Some(std::env::var("MTC_RUSTFS_TEST_BUCKET").expect("RustFS bucket required"));
        config.s3_access_key =
            Some(std::env::var("MTC_RUSTFS_TEST_ACCESS_KEY").expect("RustFS access key required"));
        config.s3_secret_key =
            Some(std::env::var("MTC_RUSTFS_TEST_SECRET_KEY").expect("RustFS secret required"));
        config.s3_region = "us-east-1".into();
        config.s3_allow_http = true;
        retire_complete_sources_contract(ArchiveStore::from_config(&config).await.unwrap()).await;
    }

    #[tokio::test]
    async fn retirement_fence_hides_late_root_and_rejects_republication_without_deleting_cas() {
        let archive = super::super::tests::memory_store();
        let tenant = Uuid::now_v7();
        let request = Uuid::now_v7();
        let bytes = Bytes::from_static(b"exact temporary original");
        let object = archive
            .put_tenant_content(tenant, bytes.clone())
            .await
            .unwrap();
        archive
            .publish_raw_capture(tenant, request, "request", &object)
            .await
            .unwrap();
        let fence = RawRetirement {
            format_version: 1,
            tenant_id: tenant,
            request_id: request,
            side: "request".into(),
            source: object.clone(),
            semantic_revision: 7,
            semantic_leaf: "a".repeat(64),
            release_generation: Uuid::now_v7(),
        };
        // This seeds future cleanup metadata only. Production activation must
        // separately prove coverage and the end of retry/publication owners.
        archive.restore_durable_root(
            &format!("tenants/{tenant}/semantic/v2/requests/{request}/7-coverage.json"),
            Bytes::from(serde_json::to_vec(&serde_json::json!({
                "publication": {"tenant_id":tenant,"request_id":request,"revision":7},
                "binding": {"tenant_id":tenant,"request_id":request,"node_hash":"a".repeat(64)},
                "coverage": {"observed_terminal":true,"projection_state":"complete","gaps":[]},
                "raw_sources": {"request":object,"response":null}
            })).unwrap()),
        ).await.unwrap();
        archive
            .restore_durable_root(
                &retirement_root(tenant, request, "request").unwrap(),
                Bytes::from(serde_json::to_vec(&fence).unwrap()),
            )
            .await
            .unwrap();
        assert!(
            archive
                .raw_capture_reference(tenant, request, "request")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            archive
                .publish_raw_capture(tenant, request, "request", &object)
                .await
                .is_err()
        );
        // Even a physically remaining/re-created old root is not live. No
        // shared body deletion is granted by this metadata preparation step.
        assert!(
            archive
                .inner
                .head(&archive_path(&root(tenant, request, "request").unwrap()).unwrap())
                .await
                .is_ok()
        );
        assert_eq!(
            archive
                .get_tenant_content(tenant, &object, bytes.len())
                .await
                .unwrap(),
            bytes
        );
        assert!(
            archive
                .raw_capture_reference(tenant, request, "response")
                .await
                .unwrap()
                .is_none()
        );
    }
}
