use bytes::Bytes;
use uuid::Uuid;

use super::{ConversationReadLimits, tests::memory_store};
use crate::conversation::ConversationHints;

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
            response.clone(),
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
