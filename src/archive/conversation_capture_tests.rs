use bytes::Bytes;
use futures_util::TryStreamExt;
use uuid::Uuid;

use super::{ConversationReadLimits, tests::memory_store};
use crate::conversation::ConversationHints;

#[tokio::test]
async fn conversation_stream_reads_large_opaque_event_without_document_budget() {
    let store = memory_store();
    let tenant = Uuid::now_v7();
    let document = serde_json::json!({"role":"tool", "content":"工具".repeat(350000)}).to_string();
    let event = super::ConversationEvent {
        event_id: "tool-1".into(),
        request_id: Some("request-1".into()),
        branch_id: "main".into(),
        parent_event_id: None,
        agent_id: None,
        parent_agent_id: None,
        origin: "response".into(),
        role: Some("tool".into()),
        kind: "tool_result".into(),
        document: serde_json::value::RawValue::from_string(document).unwrap(),
        media: Vec::new(),
        gaps: Vec::new(),
    };
    let head = store
        .append_conversation_event(tenant, None, &event, 65536)
        .await
        .unwrap();
    let manifest = super::ConversationManifest {
        format_version: 1,
        tenant_id: tenant,
        session_id: "session".into(),
        branch_id: "main".into(),
        parent_branch_id: None,
        parent_event_id: None,
        head: Some(head),
        raw_captures: Vec::new(),
        gaps: Vec::new(),
    };
    let object = store.put_conversation_manifest(&manifest).await.unwrap();
    let lines: Vec<Bytes> = store
        .stream_conversation_export(tenant, &object)
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    assert_eq!(lines.len(), 3);
    assert!(lines.iter().all(|line| line.len() < 65536));
    let entry: serde_json::Value = serde_json::from_slice(&lines[1]).unwrap();
    assert_eq!(entry["ordinal"], 0);
    assert!(entry["object"]["size_bytes"].as_u64().unwrap() > 2 * 1024 * 1024);
}

#[tokio::test]
async fn missing_response_is_not_fabricated_and_late_capture_preserves_prefix() {
    let store = memory_store();
    let tenant = Uuid::now_v7();
    let request_id = Uuid::now_v7();
    let request = Bytes::from_static(br#"{"messages":[{"role":"user","content":"retained"}]}"#);
    let hints = ConversationHints::default();
    let incomplete = store
        .capture_conversation_documents(tenant, request_id, &hints, request.clone(), None, false)
        .await
        .unwrap();
    let manifest = store
        .get_conversation_manifest(tenant, &incomplete, 65536)
        .await
        .unwrap();
    assert_eq!(manifest.raw_captures.len(), 1);
    assert!(manifest.gaps.iter().any(|gap| gap == "response_missing"));
    assert!(
        store
            .raw_capture_reference(tenant, request_id, "response")
            .await
            .unwrap()
            .is_none()
    );
    let limits = ConversationReadLimits {
        max_events: 100,
        max_object_bytes: 65536,
        max_total_bytes: 1024 * 1024,
    };
    assert!(
        store
            .export_conversation(tenant, &incomplete, limits)
            .await
            .is_err()
    );
    let complete = store
        .capture_conversation_documents(
            tenant,
            request_id,
            &hints,
            request.clone(),
            Some(Bytes::from_static(
                br#"{"output":[{"type":"message","content":"recovered"}]}"#,
            )),
            true,
        )
        .await
        .unwrap();
    let exported = store
        .export_conversation(tenant, &complete, limits)
        .await
        .unwrap();
    assert!(exported.manifest.gaps.is_empty());
    assert_eq!(exported.manifest.raw_captures.len(), 2);
    assert_eq!(
        store
            .request_conversation_reference(tenant, request_id)
            .await
            .unwrap(),
        complete
    );
    assert!(
        store
            .publish_request_conversation(tenant, request_id, &incomplete)
            .await
            .is_err()
    );
    assert_eq!(
        store
            .get_tenant_content(tenant, &manifest.raw_captures[0].object, 65536)
            .await
            .unwrap(),
        request
    );
}

#[tokio::test]
async fn captured_raw_sse_tools_and_media_survive_independent_cache_deletion() {
    let store = memory_store();
    let tenant = Uuid::now_v7();
    let request_id = Uuid::now_v7();
    let request = Bytes::from_static(
        br#"{"model":"synthetic","messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,aW1hZ2U="}}]},{"role":"assistant","tool_calls":[{"id":"call-1","function":{"name":"lookup","arguments":"{\"q\":1}"}}]},{"role":"tool","tool_call_id":"call-1","content":"result"}],"unknown":18446744073709551616}"#,
    );
    let response = Bytes::from_static(
        b"event: message\r\ndata: { \"choices\":[{\"delta\":{\"content\":\"answer\"}}] }\r\n\r\ndata: [DONE]\r\n\r\n",
    );
    let cache = format!("raw-cache/tenants/{tenant}/requests/{request_id}/response");
    store.put(&cache, response.clone()).await.unwrap();
    let root = store
        .capture_conversation_documents(
            tenant,
            request_id,
            &ConversationHints::default(),
            request.clone(),
            Some(response.clone()),
            true,
        )
        .await
        .unwrap();
    store.delete(&cache).await.unwrap();
    let exported = store
        .export_conversation(
            tenant,
            &root,
            ConversationReadLimits {
                max_events: 100,
                max_object_bytes: 65536,
                max_total_bytes: 1024 * 1024,
            },
        )
        .await
        .unwrap();
    assert_eq!(exported.events.len(), 6);
    let lines: Vec<Bytes> = store
        .stream_conversation_export(tenant, &root)
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    assert_eq!(lines.len(), 8);
    let first: serde_json::Value = serde_json::from_slice(&lines[0]).unwrap();
    assert_eq!(first["record"], "manifest");
    let ordinals: Vec<u64> = lines[1..7]
        .iter()
        .map(|line| {
            let event: serde_json::Value = serde_json::from_slice(line).unwrap();
            event["ordinal"].as_u64().unwrap()
        })
        .collect();
    assert_eq!(ordinals, vec![5, 4, 3, 2, 1, 0]);
    let end: serde_json::Value = serde_json::from_slice(lines.last().unwrap()).unwrap();
    assert_eq!(end["verified"], true);
    assert_eq!(exported.events[0].media.len(), 1);
    assert_eq!(exported.events[0].request_id, None);
    assert!(exported.events[1].document.get().contains("call-1"));
    assert!(exported.events[2].document.get().contains("tool_call_id"));
    for (capture, expected) in exported
        .manifest
        .raw_captures
        .iter()
        .zip([request, response])
    {
        assert_eq!(
            store
                .get_tenant_content(tenant, &capture.object, 65536)
                .await
                .unwrap(),
            expected
        );
        assert!(store.delete(&capture.object.object_locator).await.is_err());
    }
}
