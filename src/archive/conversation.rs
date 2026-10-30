use std::collections::HashSet;

use bytes::Bytes;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::value::RawValue;
use uuid::Uuid;

use super::{ArchiveStore, CasArchiveObject};
use crate::error::AppError;

const FORMAT_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ConversationPrefix {
    pub object: CasArchiveObject,
    pub event_count: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationMedia {
    pub json_pointer: String,
    pub media_type: String,
    pub object: CasArchiveObject,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationEvent {
    pub event_id: String,
    pub request_id: Option<String>,
    pub branch_id: String,
    pub parent_event_id: Option<String>,
    pub agent_id: Option<String>,
    pub parent_agent_id: Option<String>,
    pub origin: String,
    pub role: Option<String>,
    pub kind: String,
    pub document: Box<RawValue>,
    pub media: Vec<ConversationMedia>,
    pub gaps: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RawConversationFormat {
    Json,
    Sse,
    Binary,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawConversationCapture {
    pub request_id: String,
    pub side: String,
    pub format: RawConversationFormat,
    pub object: CasArchiveObject,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationManifest {
    pub format_version: u32,
    pub tenant_id: Uuid,
    pub session_id: String,
    pub branch_id: String,
    pub parent_branch_id: Option<String>,
    pub parent_event_id: Option<String>,
    pub head: Option<ConversationPrefix>,
    pub raw_captures: Vec<RawConversationCapture>,
    pub gaps: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PrefixNode {
    pub(super) format_version: u32,
    pub(super) parent: Option<ConversationPrefix>,
    pub(super) event: CasArchiveObject,
}

#[cfg(test)]
#[derive(Clone, Debug, Serialize)]
pub struct ConversationExport {
    pub manifest: ConversationManifest,
    pub events: Vec<ConversationEvent>,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug)]
pub struct ConversationReadLimits {
    pub max_events: usize,
    pub max_object_bytes: usize,
    pub max_total_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationBodyReference {
    pub conversation_body_v1: CasArchiveObject,
    pub chars: u64,
}

#[cfg(test)]
struct ReadBudget {
    limits: ConversationReadLimits,
    consumed: u64,
}

#[cfg(test)]
impl ReadBudget {
    fn charge(&mut self, bytes: u64) -> Result<(), AppError> {
        self.consumed = self
            .consumed
            .checked_add(bytes)
            .filter(|total| *total <= self.limits.max_total_bytes)
            .ok_or_else(|| AppError::storage("conversation_read_budget_exceeded"))?;
        Ok(())
    }
}

fn invalid() -> AppError {
    AppError::storage("conversation_object_invalid")
}

pub(super) fn validate_manifest(
    tenant_id: Uuid,
    manifest: &ConversationManifest,
) -> Result<(), AppError> {
    if manifest.format_version != FORMAT_VERSION
        || manifest.tenant_id != tenant_id
        || manifest.session_id.is_empty()
        || manifest.branch_id.is_empty()
        || manifest.parent_branch_id.is_some() != manifest.parent_event_id.is_some()
        || manifest.parent_branch_id.as_ref() == Some(&manifest.branch_id)
        || manifest
            .parent_branch_id
            .as_ref()
            .is_some_and(String::is_empty)
        || manifest
            .parent_event_id
            .as_ref()
            .is_some_and(String::is_empty)
    {
        return Err(invalid());
    }
    if let Some(head) = &manifest.head {
        head.object.validate_tenant(tenant_id)?;
        if head.event_count == 0 {
            return Err(invalid());
        }
    }
    for capture in &manifest.raw_captures {
        capture.object.validate_tenant(tenant_id)?;
        if capture.request_id.is_empty() || !matches!(capture.side.as_str(), "request" | "response")
        {
            return Err(invalid());
        }
    }
    Ok(())
}

fn validate_event(tenant_id: Uuid, event: &ConversationEvent) -> Result<(), AppError> {
    if event.event_id.is_empty()
        || event.request_id.as_ref().is_some_and(String::is_empty)
        || event.branch_id.is_empty()
        || event.origin.is_empty()
        || event.kind.is_empty()
    {
        return Err(invalid());
    }
    let media_document = if event.media.is_empty() {
        None
    } else {
        Some(
            serde_json::from_str::<serde_json::Value>(event.document.get())
                .map_err(|_| invalid())?,
        )
    };
    let mut pointers = HashSet::new();
    for media in &event.media {
        media.object.validate_tenant(tenant_id)?;
        if media.media_type.is_empty()
            || (!media.json_pointer.is_empty() && !media.json_pointer.starts_with('/'))
            || !pointers.insert(&media.json_pointer)
            || media_document
                .as_ref()
                .is_none_or(|document| document.pointer(&media.json_pointer).is_none())
        {
            return Err(invalid());
        }
    }
    Ok(())
}

pub(super) fn validate_node(
    tenant_id: Uuid,
    head: &ConversationPrefix,
    node: &PrefixNode,
) -> Result<(), AppError> {
    node.event.validate_tenant(tenant_id)?;
    let parent_count = match &node.parent {
        Some(parent) => {
            parent.object.validate_tenant(tenant_id)?;
            if parent.event_count == 0 {
                return Err(invalid());
            }
            parent.event_count
        }
        None => 0,
    };
    if node.format_version != FORMAT_VERSION
        || parent_count.checked_add(1) != Some(head.event_count)
    {
        return Err(invalid());
    }
    Ok(())
}

impl ArchiveStore {
    pub(super) async fn conversation_has_prefix(
        &self,
        tenant_id: Uuid,
        mut head: Option<ConversationPrefix>,
        prefix: &Option<ConversationPrefix>,
        maximum: usize,
    ) -> Result<bool, AppError> {
        let Some(prefix) = prefix else {
            return Ok(true);
        };
        while let Some(current) = head {
            if current.event_count <= prefix.event_count {
                return Ok(current == *prefix);
            }
            let node: PrefixNode = self
                .get_conversation_json(tenant_id, &current.object, maximum)
                .await?;
            validate_node(tenant_id, &current, &node)?;
            head = node.parent;
        }
        Ok(false)
    }

    async fn put_conversation_json(
        &self,
        tenant_id: Uuid,
        value: &impl Serialize,
        maximum: usize,
    ) -> Result<CasArchiveObject, AppError> {
        let bytes = serde_json::to_vec(value).map_err(|_| invalid())?;
        if bytes.len() > maximum {
            return Err(AppError::storage("conversation_object_exceeds_read_limit"));
        }
        self.put_tenant_content(tenant_id, Bytes::from(bytes)).await
    }

    pub(super) async fn get_conversation_json<Content: DeserializeOwned>(
        &self,
        tenant_id: Uuid,
        object: &CasArchiveObject,
        maximum: usize,
    ) -> Result<Content, AppError> {
        let bytes = self.get_tenant_content(tenant_id, object, maximum).await?;
        serde_json::from_slice(&bytes).map_err(|_| invalid())
    }

    pub async fn append_conversation_event(
        &self,
        tenant_id: Uuid,
        parent: Option<ConversationPrefix>,
        event: &ConversationEvent,
        maximum: usize,
    ) -> Result<ConversationPrefix, AppError> {
        validate_event(tenant_id, event)?;
        let event_count = if let Some(head) = &parent {
            let node: PrefixNode = self
                .get_conversation_json(tenant_id, &head.object, maximum)
                .await?;
            validate_node(tenant_id, head, &node)?;
            head.event_count.checked_add(1).ok_or_else(invalid)?
        } else {
            1
        };
        for media in &event.media {
            self.verify_tenant_content(tenant_id, &media.object).await?;
        }
        let metadata = serde_json::json!({
            "event_id":event.event_id, "request_id":event.request_id,
            "branch_id":event.branch_id, "parent_event_id":event.parent_event_id,
            "agent_id":event.agent_id, "parent_agent_id":event.parent_agent_id,
            "origin":event.origin, "role":event.role, "kind":event.kind,
            "media":event.media, "gaps":event.gaps,
        });
        let mut metadata = serde_json::to_vec(&metadata).map_err(|_| invalid())?;
        metadata.pop();
        metadata.extend_from_slice(b",\"document\":");
        let staging = format!("conversation-staging/v1/{tenant_id}/{}", Uuid::now_v7());
        let mut writer = self.start_writer(&staging).await?;
        writer.write(Bytes::from(metadata)).await?;
        for chunk in event.document.get().as_bytes().chunks(65536) {
            writer.write(Bytes::copy_from_slice(chunk)).await?;
        }
        writer.write(Bytes::from_static(b"}")).await?;
        let staged = writer.finish_staged().await?;
        let event = self.promote_staged_text_to_cas(tenant_id, &staged).await?;
        self.delete(&staged.object_locator).await?;
        let object = self
            .put_conversation_json(
                tenant_id,
                &PrefixNode {
                    format_version: FORMAT_VERSION,
                    parent,
                    event,
                },
                maximum,
            )
            .await?;
        Ok(ConversationPrefix {
            object,
            event_count,
        })
    }

    pub async fn put_conversation_manifest(
        &self,
        manifest: &ConversationManifest,
    ) -> Result<CasArchiveObject, AppError> {
        self.validate_conversation_streaming(manifest).await?;
        let bytes = serde_json::to_vec(manifest).map_err(|_| invalid())?;
        self.put_tenant_content(manifest.tenant_id, Bytes::from(bytes))
            .await
    }

    pub async fn get_conversation_manifest(
        &self,
        tenant_id: Uuid,
        object: &CasArchiveObject,
        maximum: usize,
    ) -> Result<ConversationManifest, AppError> {
        let manifest = self
            .get_conversation_json(tenant_id, object, maximum)
            .await?;
        validate_manifest(tenant_id, &manifest)?;
        Ok(manifest)
    }

    #[cfg(test)]
    pub async fn export_conversation(
        &self,
        tenant_id: Uuid,
        object: &CasArchiveObject,
        limits: ConversationReadLimits,
    ) -> Result<ConversationExport, AppError> {
        let mut budget = ReadBudget {
            limits,
            consumed: 0,
        };
        budget.charge(object.size_bytes)?;
        let manifest = self
            .get_conversation_manifest(tenant_id, object, limits.max_object_bytes)
            .await?;
        let events = self
            .reconstruct_conversation(tenant_id, &manifest, &mut budget)
            .await?;
        if events.is_empty()
            || !manifest.gaps.is_empty()
            || events.iter().any(|event| !event.gaps.is_empty())
        {
            return Err(AppError::storage("conversation_training_export_incomplete"));
        }
        Ok(ConversationExport { manifest, events })
    }

    #[cfg(test)]
    async fn reconstruct_conversation(
        &self,
        tenant_id: Uuid,
        manifest: &ConversationManifest,
        budget: &mut ReadBudget,
    ) -> Result<Vec<ConversationEvent>, AppError> {
        validate_manifest(tenant_id, manifest)?;
        let count = manifest.head.as_ref().map_or(0, |head| head.event_count);
        if count > budget.limits.max_events as u64 {
            return Err(AppError::storage("conversation_read_budget_exceeded"));
        }
        let mut events = Vec::new();
        let mut cursor = manifest.head.clone();
        let mut verified_objects = HashSet::new();
        while let Some(head) = cursor {
            budget.charge(head.object.size_bytes)?;
            let node: PrefixNode = self
                .get_conversation_json(tenant_id, &head.object, budget.limits.max_object_bytes)
                .await?;
            validate_node(tenant_id, &head, &node)?;
            budget.charge(node.event.size_bytes)?;
            let event: ConversationEvent = self
                .get_conversation_json(tenant_id, &node.event, budget.limits.max_object_bytes)
                .await?;
            validate_event(tenant_id, &event)?;
            for media in &event.media {
                let identity = (media.object.object_locator.clone(), media.object.size_bytes);
                if verified_objects.insert(identity) {
                    budget.charge(media.object.size_bytes)?;
                    self.verify_tenant_content(tenant_id, &media.object).await?;
                }
            }
            events.push(event);
            cursor = node.parent;
        }
        events.reverse();
        let mut event_ids = HashSet::new();
        for event in &events {
            if event
                .parent_event_id
                .as_ref()
                .is_some_and(|parent| !event_ids.contains(parent))
                || !event_ids.insert(event.event_id.clone())
            {
                return Err(invalid());
            }
        }
        if events.len() as u64 != count
            || events
                .last()
                .is_some_and(|event| event.branch_id != manifest.branch_id)
            || manifest.parent_event_id.as_ref().is_some_and(|parent| {
                !events.iter().any(|event| {
                    &event.event_id == parent
                        && Some(&event.branch_id) == manifest.parent_branch_id.as_ref()
                })
            })
        {
            return Err(invalid());
        }
        for capture in &manifest.raw_captures {
            let identity = (
                capture.object.object_locator.clone(),
                capture.object.size_bytes,
            );
            if verified_objects.insert(identity) {
                budget.charge(capture.object.size_bytes)?;
                self.verify_tenant_content(tenant_id, &capture.object)
                    .await?;
            }
        }
        Ok(events)
    }
}

#[cfg(test)]
mod tests;
