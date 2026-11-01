use std::{io::Read, pin::Pin};

use bytes::Bytes;
use futures_util::{Stream, StreamExt, stream};
use serde::{Deserialize, de::DeserializeOwned};
use uuid::Uuid;

use super::{
    ArchiveStore, CasArchiveObject, ConversationManifest, ConversationMedia, ConversationPrefix,
    conversation::{PrefixNode, validate_manifest, validate_node},
};
use crate::error::AppError;

#[derive(Deserialize)]
struct EventReferences {
    event_id: String,
    branch_id: String,
    parent_event_id: Option<String>,
    media: Vec<ConversationMedia>,
    gaps: Vec<String>,
}

struct Chunks {
    receiver: tokio::sync::mpsc::Receiver<Bytes>,
    current: Bytes,
}

impl Read for Chunks {
    fn read(&mut self, destination: &mut [u8]) -> std::io::Result<usize> {
        if destination.is_empty() {
            return Ok(0);
        }
        while self.current.is_empty() {
            let Some(chunk) = self.receiver.blocking_recv() else {
                return Ok(0);
            };
            self.current = chunk;
        }
        let count = destination.len().min(self.current.len());
        destination[..count].copy_from_slice(&self.current[..count]);
        self.current = self.current.slice(count..);
        Ok(count)
    }
}

struct Export {
    archive: ArchiveStore,
    tenant: Uuid,
    manifest: ConversationManifest,
    cursor: Option<ConversationPrefix>,
    remaining: u64,
    first: bool,
    done: bool,
}

fn line(value: serde_json::Value) -> Result<Bytes, AppError> {
    let mut bytes =
        serde_json::to_vec(&value).map_err(|_| AppError::internal("conversation_export_encode"))?;
    bytes.push(b'\n');
    Ok(Bytes::from(bytes))
}

impl ArchiveStore {
    async fn read_tenant_json_streamed<T: DeserializeOwned + Send + 'static>(
        &self,
        tenant: Uuid,
        object: &CasArchiveObject,
    ) -> Result<T, AppError> {
        let mut source = self.open_tenant_content(tenant, object).await?.stream;
        let (sender, receiver) = tokio::sync::mpsc::channel(2);
        let parsed = tokio::task::spawn_blocking(move || {
            serde_json::from_reader::<_, T>(Chunks {
                receiver,
                current: Bytes::new(),
            })
        });
        let pumped = async {
            while let Some(chunk) = source.next().await {
                let chunk =
                    chunk.map_err(|_| AppError::storage("conversation_event_stream_failed"))?;
                for piece in chunk.chunks(65536) {
                    if sender.send(Bytes::copy_from_slice(piece)).await.is_err() {
                        return Err(AppError::storage("conversation_event_invalid"));
                    }
                }
            }
            Ok::<_, AppError>(())
        };
        let result = pumped.await;
        drop(sender);
        let parsed = parsed
            .await
            .map_err(|_| AppError::storage("conversation_event_parse_failed"))?
            .map_err(|_| AppError::storage("conversation_event_invalid"))?;
        result?;
        Ok(parsed)
    }

    pub async fn get_conversation_manifest_streamed(
        &self,
        tenant: Uuid,
        object: &CasArchiveObject,
    ) -> Result<ConversationManifest, AppError> {
        let manifest = self.read_tenant_json_streamed(tenant, object).await?;
        validate_manifest(tenant, &manifest)?;
        Ok(manifest)
    }

    async fn event_references(
        &self,
        tenant: Uuid,
        object: &CasArchiveObject,
        require_complete: bool,
    ) -> Result<EventReferences, AppError> {
        let parsed: EventReferences = self.read_tenant_json_streamed(tenant, object).await?;
        if parsed.event_id.is_empty()
            || parsed.branch_id.is_empty()
            || (require_complete && !parsed.gaps.is_empty())
        {
            return Err(AppError::storage("conversation_training_export_incomplete"));
        }
        for media in &parsed.media {
            self.verify_tenant_content(tenant, &media.object).await?;
        }
        Ok(parsed)
    }

    pub(super) async fn validate_conversation_streaming(
        &self,
        manifest: &ConversationManifest,
    ) -> Result<(), AppError> {
        validate_manifest(manifest.tenant_id, manifest)?;
        let mut cursor = manifest.head.clone();
        let mut remaining = cursor.as_ref().map_or(0, |head| head.event_count);
        let mut ids = std::collections::HashSet::new();
        let mut parents = std::collections::HashSet::new();
        let mut branch_parent_found = manifest.parent_event_id.is_none();
        while let Some(head) = cursor {
            if head.event_count != remaining {
                return Err(AppError::storage("conversation_export_order_invalid"));
            }
            let node: PrefixNode = self
                .get_conversation_json(manifest.tenant_id, &head.object, 4096)
                .await?;
            validate_node(manifest.tenant_id, &head, &node)?;
            let event = self
                .event_references(manifest.tenant_id, &node.event, false)
                .await?;
            if remaining == manifest.head.as_ref().map_or(0, |head| head.event_count)
                && event.branch_id != manifest.branch_id
            {
                return Err(AppError::storage("conversation_export_branch_invalid"));
            }
            if manifest.parent_event_id.as_ref() == Some(&event.event_id)
                && manifest.parent_branch_id.as_ref() == Some(&event.branch_id)
            {
                branch_parent_found = true;
            }
            parents.remove(&event.event_id);
            if !ids.insert(event.event_id) {
                return Err(AppError::storage("conversation_duplicate_event"));
            }
            if let Some(parent) = event.parent_event_id {
                if ids.contains(&parent) {
                    return Err(AppError::storage("conversation_parent_order_invalid"));
                }
                parents.insert(parent);
            }
            remaining -= 1;
            cursor = node.parent;
        }
        if remaining != 0 || !parents.is_empty() || !branch_parent_found {
            return Err(AppError::storage("conversation_ancestry_incomplete"));
        }
        for capture in &manifest.raw_captures {
            self.verify_tenant_content(manifest.tenant_id, &capture.object)
                .await?;
        }
        Ok(())
    }

    pub async fn stream_conversation_export(
        &self,
        tenant: Uuid,
        object: &CasArchiveObject,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<Bytes, AppError>> + Send>>, AppError> {
        let manifest = self
            .get_conversation_manifest_streamed(tenant, object)
            .await?;
        validate_manifest(tenant, &manifest)?;
        if !manifest.gaps.is_empty() || manifest.head.is_none() {
            return Err(AppError::storage("conversation_training_export_incomplete"));
        }
        for capture in &manifest.raw_captures {
            self.verify_tenant_content(tenant, &capture.object).await?;
        }
        let remaining = manifest.head.as_ref().map_or(0, |head| head.event_count);
        let state = Export {
            archive: self.clone(),
            tenant,
            cursor: manifest.head.clone(),
            manifest,
            remaining,
            first: true,
            done: false,
        };
        Ok(Box::pin(stream::try_unfold(
            state,
            |mut state| async move {
                if state.done {
                    return Ok(None);
                }
                if state.first {
                    state.first = false;
                    let bytes = line(serde_json::json!({
                        "record":"manifest", "format_version":1,
                        "event_order":"descending_ordinal", "manifest":state.manifest,
                    }))?;
                    return Ok(Some((bytes, state)));
                }
                if let Some(head) = state.cursor.take() {
                    if head.event_count != state.remaining {
                        return Err(AppError::storage("conversation_export_order_invalid"));
                    }
                    let node: PrefixNode = state
                        .archive
                        .get_conversation_json(state.tenant, &head.object, 4096)
                        .await?;
                    validate_node(state.tenant, &head, &node)?;
                    let event = state
                        .archive
                        .event_references(state.tenant, &node.event, true)
                        .await?;
                    if state.remaining
                        == state
                            .manifest
                            .head
                            .as_ref()
                            .map_or(0, |head| head.event_count)
                        && event.branch_id != state.manifest.branch_id
                    {
                        return Err(AppError::storage("conversation_export_branch_invalid"));
                    }
                    state.remaining -= 1;
                    state.cursor = node.parent;
                    let bytes = line(serde_json::json!({
                        "record":"event", "ordinal":state.remaining, "event_id":event.event_id,
                        "branch_id":event.branch_id, "parent_event_id":event.parent_event_id,
                        "object":node.event, "media":event.media,
                    }))?;
                    return Ok(Some((bytes, state)));
                }
                if state.remaining != 0 {
                    return Err(AppError::storage("conversation_export_count_invalid"));
                }
                state.done = true;
                let bytes = line(serde_json::json!({
                    "record":"end", "events":state.manifest.head.as_ref().map_or(0, |head| head.event_count),
                    "verified":true,
                }))?;
                Ok(Some((bytes, state)))
            },
        )))
    }
}
