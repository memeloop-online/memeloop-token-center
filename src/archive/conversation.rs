use serde::{Deserialize, Serialize, de::DeserializeOwned};
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

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationBodyReference {
    pub conversation_body_v1: CasArchiveObject,
    pub chars: u64,
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
    pub(super) async fn get_conversation_json<Content: DeserializeOwned>(
        &self,
        tenant_id: Uuid,
        object: &CasArchiveObject,
        maximum: usize,
    ) -> Result<Content, AppError> {
        let bytes = self.get_tenant_content(tenant_id, object, maximum).await?;
        serde_json::from_slice(&bytes).map_err(|_| invalid())
    }

    pub async fn request_conversation_reference(
        &self,
        tenant_id: Uuid,
        request_id: Uuid,
    ) -> Result<CasArchiveObject, AppError> {
        let encoded = self
            .get_bounded(
                &format!("tenants/{tenant_id}/conversations/v1/requests/{request_id}.json"),
                4096,
            )
            .await?;
        let reference: CasArchiveObject = serde_json::from_slice(&encoded)
            .map_err(|_| AppError::storage("conversation_root_invalid"))?;
        reference.validate_tenant(tenant_id)?;
        Ok(reference)
    }
}

#[cfg(test)]
mod tests;
