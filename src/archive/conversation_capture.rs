use base64::Engine as _;
use bytes::Bytes;
use object_store::{PutMode, PutOptions, PutPayload};
use serde_json::value::RawValue;
use uuid::Uuid;

use super::{
    ArchiveStore, CasArchiveObject, ConversationEvent, ConversationManifest, ConversationMedia,
    ConversationPrefix, ConversationReadLimits, RawConversationCapture, RawConversationFormat,
    path::archive_path,
};
use crate::{conversation::ConversationHints, error::AppError};

const MAX_OBJECT_BYTES: usize = 128 * 1024 * 1024;

fn limits() -> ConversationReadLimits {
    ConversationReadLimits {
        max_events: 1_000_000,
        max_object_bytes: MAX_OBJECT_BYTES,
        max_total_bytes: 4 * 1024 * 1024 * 1024,
    }
}

fn request_root(tenant_id: Uuid, request_id: Uuid) -> String {
    format!("tenants/{tenant_id}/conversations/v1/requests/{request_id}.json")
}

impl ArchiveStore {
    pub async fn capture_conversation_documents(
        &self,
        tenant_id: Uuid,
        request_id: Uuid,
        hints: &ConversationHints,
        request: Bytes,
        response: Bytes,
        response_complete: bool,
    ) -> Result<CasArchiveObject, AppError> {
        let raw_request = self.put_tenant_content(tenant_id, request.clone()).await?;
        let raw_response = self.put_tenant_content(tenant_id, response.clone()).await?;
        self.publish_raw_capture(tenant_id, request_id, "request", &raw_request)
            .await?;
        self.publish_raw_capture(tenant_id, request_id, "response", &raw_response)
            .await?;
        let branch = hints.branch_id.as_deref().unwrap_or("main");
        let mut head = None;
        let mut gaps = Vec::new();
        if !response_complete {
            gaps.push("response_not_complete".into());
        }
        match serde_json::from_slice::<std::collections::BTreeMap<String, Box<RawValue>>>(&request)
        {
            Ok(mut envelope) => {
                for field in ["messages", "input"] {
                    if let Some(content) = envelope.remove(field) {
                        let items = if content.get().starts_with('[') {
                            serde_json::from_str::<Vec<Box<RawValue>>>(content.get()).map_err(
                                |_| AppError::storage("conversation_input_array_invalid"),
                            )?
                        } else {
                            vec![content]
                        };
                        for item in items {
                            head = Some(
                                self.append_captured_document(
                                    tenant_id, request_id, branch, hints, head, "request", field,
                                    item,
                                )
                                .await?,
                            );
                        }
                    }
                }
                let envelope = RawValue::from_string(
                    serde_json::to_string(&envelope)
                        .map_err(|_| AppError::internal("conversation_envelope_encode"))?,
                )
                .map_err(|_| AppError::internal("conversation_envelope_decode"))?;
                head = Some(
                    self.append_captured_document(
                        tenant_id, request_id, branch, hints, head, "request", "envelope", envelope,
                    )
                    .await?,
                );
                let execution = RawValue::from_string(
                    serde_json::to_string(hints)
                        .map_err(|_| AppError::internal("conversation_execution_encode"))?,
                )
                .map_err(|_| AppError::internal("conversation_execution_decode"))?;
                head = Some(
                    self.append_captured_document(
                        tenant_id,
                        request_id,
                        branch,
                        hints,
                        head,
                        "request",
                        "execution",
                        execution,
                    )
                    .await?,
                );
            }
            Err(_) => gaps.push("request_json_invalid".into()),
        }
        let response_format;
        if let Ok(document) = serde_json::from_slice::<Box<RawValue>>(&response) {
            response_format = RawConversationFormat::Json;
            head = Some(
                self.append_captured_document(
                    tenant_id, request_id, branch, hints, head, "response", "document", document,
                )
                .await?,
            );
        } else {
            response_format = RawConversationFormat::Sse;
            let text = std::str::from_utf8(&response)
                .map_err(|_| AppError::storage("conversation_response_encoding_invalid"))?;
            let mut data = Vec::new();
            let mut frames = Vec::new();
            for line in text.lines().chain(std::iter::once("")) {
                if line.is_empty() {
                    if !data.is_empty() {
                        frames.push(data.join("\n"));
                        data.clear();
                    }
                } else if let Some(value) = line.strip_prefix("data:") {
                    data.push(value.strip_prefix(' ').unwrap_or(value));
                }
            }
            if frames.is_empty() {
                gaps.push("response_events_missing".into());
            }
            for frame in frames {
                if frame.trim() == "[DONE]" {
                    continue;
                }
                match RawValue::from_string(frame) {
                    Ok(document) => {
                        head = Some(
                            self.append_captured_document(
                                tenant_id, request_id, branch, hints, head, "response", "sse_data",
                                document,
                            )
                            .await?,
                        );
                    }
                    Err(_) => gaps.push("response_event_json_invalid".into()),
                }
            }
        }
        let manifest = ConversationManifest {
            format_version: 1,
            tenant_id,
            session_id: hints
                .session_id
                .clone()
                .unwrap_or_else(|| request_id.to_string()),
            branch_id: branch.into(),
            parent_branch_id: None,
            parent_event_id: None,
            head,
            raw_captures: vec![
                RawConversationCapture {
                    request_id: request_id.to_string(),
                    side: "request".into(),
                    format: RawConversationFormat::Json,
                    object: raw_request,
                },
                RawConversationCapture {
                    request_id: request_id.to_string(),
                    side: "response".into(),
                    format: response_format,
                    object: raw_response,
                },
            ],
            gaps,
        };
        let object = self.put_conversation_manifest(&manifest, limits()).await?;
        self.publish_request_conversation(tenant_id, request_id, &object)
            .await?;
        Ok(object)
    }

    #[allow(clippy::too_many_arguments)]
    async fn append_captured_document(
        &self,
        tenant_id: Uuid,
        request_id: Uuid,
        branch: &str,
        hints: &ConversationHints,
        head: Option<ConversationPrefix>,
        origin: &str,
        kind: &str,
        document: Box<RawValue>,
    ) -> Result<ConversationPrefix, AppError> {
        let depth = head.as_ref().map_or(0, |head| head.event_count);
        let digest = blake3::hash(document.get().as_bytes()).to_hex().to_string();
        let context = origin == "request" && matches!(kind, "messages" | "input");
        let mut event = ConversationEvent {
            event_id: format!("{origin}:{kind}:{depth}:{digest}"),
            request_id: if context {
                None
            } else {
                Some(request_id.to_string())
            },
            branch_id: branch.into(),
            parent_event_id: None,
            agent_id: if context {
                None
            } else {
                hints.agent_id.clone()
            },
            parent_agent_id: if context {
                None
            } else {
                hints.parent_agent_id.clone()
            },
            origin: origin.into(),
            role: None,
            kind: kind.into(),
            document,
            media: Vec::new(),
            gaps: Vec::new(),
        };
        let content: serde_json::Value = serde_json::from_str(event.document.get())
            .map_err(|_| AppError::storage("conversation_event_media_scan_failed"))?;
        let mut media = Vec::new();
        collect_media(&content, "", &mut media, &mut event.gaps);
        for (json_pointer, media_type, bytes) in media {
            event.media.push(ConversationMedia {
                json_pointer,
                media_type,
                object: self
                    .put_tenant_content(tenant_id, Bytes::from(bytes))
                    .await?,
            });
        }
        self.append_conversation_event(tenant_id, head, &event, MAX_OBJECT_BYTES)
            .await
    }

    pub async fn publish_request_conversation(
        &self,
        tenant_id: Uuid,
        request_id: Uuid,
        manifest: &CasArchiveObject,
    ) -> Result<(), AppError> {
        self.get_conversation_manifest(tenant_id, manifest, MAX_OBJECT_BYTES)
            .await?;
        let encoded = serde_json::to_vec(manifest)
            .map_err(|_| AppError::internal("conversation_root_encode"))?;
        match self
            .inner
            .put_opts(
                &archive_path(&request_root(tenant_id, request_id))?,
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
                let existing = self
                    .request_conversation_reference(tenant_id, request_id)
                    .await?;
                if existing == *manifest {
                    Ok(())
                } else {
                    Err(AppError::Conflict(
                        "conversation root content conflict".into(),
                    ))
                }
            }
            Err(error) => Err(error.into()),
        }
    }

    pub async fn request_conversation_reference(
        &self,
        tenant_id: Uuid,
        request_id: Uuid,
    ) -> Result<CasArchiveObject, AppError> {
        let encoded = self
            .get_bounded(&request_root(tenant_id, request_id), 4096)
            .await?;
        let reference: CasArchiveObject = serde_json::from_slice(&encoded)
            .map_err(|_| AppError::storage("conversation_root_invalid"))?;
        reference.validate_tenant(tenant_id)?;
        Ok(reference)
    }
}

fn collect_media(
    value: &serde_json::Value,
    pointer: &str,
    media: &mut Vec<(String, String, Vec<u8>)>,
    gaps: &mut Vec<String>,
) {
    match value {
        serde_json::Value::String(text) => {
            if let Some(data) = text.strip_prefix("data:") {
                match data.split_once(";base64,") {
                    Some((kind, data)) if !kind.is_empty() => {
                        match base64::engine::general_purpose::STANDARD.decode(data) {
                            Ok(bytes) => media.push((pointer.into(), kind.into(), bytes)),
                            Err(_) => gaps.push("media_base64_invalid".into()),
                        }
                    }
                    _ => gaps.push("media_data_url_unresolved".into()),
                }
            }
        }
        serde_json::Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                collect_media(item, &format!("{pointer}/{index}"), media, gaps);
            }
        }
        serde_json::Value::Object(fields) => {
            let kind = fields.get("type").and_then(serde_json::Value::as_str);
            if matches!(
                kind,
                Some("image_url" | "input_image" | "url" | "file" | "input_file")
            ) && fields.iter().any(|(name, value)| {
                matches!(name.as_str(), "url" | "image_url" | "file_url" | "file_id")
                    && !value
                        .as_str()
                        .or_else(|| value.get("url").and_then(serde_json::Value::as_str))
                        .is_some_and(|value| value.starts_with("data:"))
            }) {
                gaps.push("external_media_not_captured".into());
            }
            if let (Some(kind), Some(data)) = (
                fields.get("media_type").and_then(serde_json::Value::as_str),
                fields.get("data").and_then(serde_json::Value::as_str),
            ) {
                match base64::engine::general_purpose::STANDARD.decode(data) {
                    Ok(bytes) => media.push((format!("{pointer}/data"), kind.into(), bytes)),
                    Err(_) => gaps.push("media_base64_invalid".into()),
                }
            }
            for (name, item) in fields {
                let segment = name.replace('~', "~0").replace('/', "~1");
                collect_media(item, &format!("{pointer}/{segment}"), media, gaps);
            }
        }
        _ => {}
    }
}
