use object_store::{ObjectStoreExt, PutPayload};

use super::*;
use crate::archive::{path::archive_path, tests::memory_store};

fn limits() -> ConversationReadLimits {
    ConversationReadLimits {
        max_events: 100,
        max_object_bytes: 64 * 1024,
        max_total_bytes: 1024 * 1024,
    }
}

fn event(
    event_id: &str,
    branch_id: &str,
    parent: Option<&str>,
    document: &str,
) -> ConversationEvent {
    ConversationEvent {
        event_id: event_id.to_owned(),
        request_id: Some("request-1".into()),
        branch_id: branch_id.into(),
        parent_event_id: parent.map(str::to_owned),
        agent_id: Some("agent-1".into()),
        parent_agent_id: None,
        origin: "response".into(),
        role: Some("assistant".into()),
        kind: "provider_item".into(),
        document: RawValue::from_string(document.into()).unwrap(),
        media: Vec::new(),
        gaps: Vec::new(),
    }
}

fn manifest(tenant_id: Uuid, head: Option<ConversationPrefix>) -> ConversationManifest {
    ConversationManifest {
        format_version: FORMAT_VERSION,
        tenant_id,
        session_id: "session-1".into(),
        branch_id: "main".into(),
        parent_branch_id: None,
        parent_event_id: None,
        head,
        raw_captures: Vec::new(),
        gaps: Vec::new(),
    }
}

#[tokio::test]
async fn tenant_cas_reuses_exact_bytes_and_rejects_foreign_or_incorrect_references() {
    let store = memory_store();
    let tenant = Uuid::now_v7();
    let other_tenant = Uuid::now_v7();
    let bytes = Bytes::from_static(b"unchanged raw payload");
    let first = store
        .put_tenant_content(tenant, bytes.clone())
        .await
        .unwrap();
    let second = store
        .put_tenant_content(tenant, bytes.clone())
        .await
        .unwrap();
    assert_eq!(first, second);
    assert_ne!(
        first.object_locator,
        store
            .put_tenant_content(other_tenant, bytes.clone())
            .await
            .unwrap()
            .object_locator
    );
    assert_eq!(
        store.get_tenant_content(tenant, &first, 100).await.unwrap(),
        bytes
    );
    assert!(
        store
            .get_tenant_content(other_tenant, &first, 100)
            .await
            .is_err()
    );
    let mut wrong_size = first.clone();
    wrong_size.size_bytes += 1;
    assert!(
        store
            .get_tenant_content(tenant, &wrong_size, 100)
            .await
            .is_err()
    );
    assert!(
        store
            .get_tenant_content(tenant, &first, bytes.len() - 1)
            .await
            .is_err()
    );
    assert!(store.delete(&first.object_locator).await.is_err());
}

#[tokio::test]
async fn corrupt_cas_is_not_overwritten_by_an_idempotent_retry() {
    let store = memory_store();
    let tenant = Uuid::now_v7();
    let bytes = Bytes::from_static(b"good");
    let object = store
        .put_tenant_content(tenant, bytes.clone())
        .await
        .unwrap();
    store
        .inner
        .put(
            &archive_path(&object.object_locator).unwrap(),
            PutPayload::from_bytes(Bytes::from_static(b"evil")),
        )
        .await
        .unwrap();
    assert!(store.put_tenant_content(tenant, bytes).await.is_err());
    assert!(
        store
            .get_tenant_content(tenant, &object, 100)
            .await
            .is_err()
    );
    assert!(store.verify_tenant_content(tenant, &object).await.is_err());
    assert_eq!(
        store.get(&object.object_locator).await.unwrap(),
        Bytes::from_static(b"evil")
    );
}

#[tokio::test]
async fn shared_prefix_preserves_exact_tool_json_and_branch_order() {
    let store = memory_store();
    let tenant = Uuid::now_v7();
    let original = r#"{ "id":"provider-1", "type":"function_call", "call_id":"call-9", "arguments":"{\"x\":1}", "encrypted_reasoning":"opaque", "unknown":18446744073709551616 }"#;
    let first_event = event("first", "main", None, original);
    let prefix = store
        .append_conversation_event(tenant, None, &first_event, limits().max_object_bytes)
        .await
        .unwrap();
    assert_eq!(
        prefix,
        store
            .append_conversation_event(tenant, None, &first_event, limits().max_object_bytes)
            .await
            .unwrap()
    );
    for branch in ["main", "edited"] {
        let result = event(
            &format!("result-{branch}"),
            branch,
            Some("first"),
            r#"{"role":"tool","tool_call_id":"call-9","output":{"ok":true}}"#,
        );
        let head = store
            .append_conversation_event(
                tenant,
                Some(prefix.clone()),
                &result,
                limits().max_object_bytes,
            )
            .await
            .unwrap();
        let node: PrefixNode = store
            .get_conversation_json(tenant, &head.object, limits().max_object_bytes)
            .await
            .unwrap();
        assert_eq!(node.parent, Some(prefix.clone()));
        let mut root = manifest(tenant, Some(head));
        root.branch_id = branch.into();
        if branch != "main" {
            root.parent_branch_id = Some("main".into());
            root.parent_event_id = Some("first".into());
        }
        let object = store
            .put_conversation_manifest(&root, limits())
            .await
            .unwrap();
        let exported = store
            .export_conversation(tenant, &object, limits())
            .await
            .unwrap();
        assert_eq!(exported.events.len(), 2);
        assert_eq!(exported.events[0].document.get(), original);
        assert_eq!(exported.events[1].event_id, format!("result-{branch}"));
        assert_eq!(exported.manifest.branch_id, branch);
        assert!(serde_json::to_string(&exported).unwrap().contains(original));
    }
}

#[tokio::test]
async fn media_and_raw_sse_are_verified_and_missing_objects_fail_closed() {
    let store = memory_store();
    let tenant = Uuid::now_v7();
    let media_bytes = Bytes::from_static(b"\x89PNG\r\n\x1a\nsynthetic");
    let media = store.put_tenant_content(tenant, media_bytes).await.unwrap();
    let sse = Bytes::from_static(
        b"event: response.output_item.done\ndata: {\"item\":{\"call_id\":\"call-1\"}}\n\n",
    );
    let raw = store.put_tenant_content(tenant, sse.clone()).await.unwrap();
    let mut first_event = event(
        "first",
        "main",
        None,
        r#"{"content":[{"type":"image","source":{"media_ref":"image-1"}}]}"#,
    );
    first_event.media.push(ConversationMedia {
        json_pointer: "/content/0/source".into(),
        media_type: "image/png".into(),
        object: media.clone(),
    });
    let head = store
        .append_conversation_event(tenant, None, &first_event, limits().max_object_bytes)
        .await
        .unwrap();
    let mut root = manifest(tenant, Some(head));
    root.raw_captures.push(RawConversationCapture {
        request_id: "request-1".into(),
        side: "response".into(),
        format: RawConversationFormat::Sse,
        object: raw.clone(),
    });
    let object = store
        .put_conversation_manifest(&root, limits())
        .await
        .unwrap();
    store
        .export_conversation(tenant, &object, limits())
        .await
        .unwrap();
    assert_eq!(
        store
            .get_tenant_content(tenant, &raw, limits().max_object_bytes)
            .await
            .unwrap(),
        sse
    );
    store
        .inner
        .delete(&archive_path(&media.object_locator).unwrap())
        .await
        .unwrap();
    assert!(
        store
            .export_conversation(tenant, &object, limits())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn malformed_media_pointer_or_cross_tenant_media_cannot_publish() {
    let store = memory_store();
    let tenant = Uuid::now_v7();
    let other_tenant = Uuid::now_v7();
    let object = store
        .put_tenant_content(tenant, Bytes::from_static(b"media"))
        .await
        .unwrap();
    let mut first_event = event("first", "main", None, r#"{"content":{}}"#);
    first_event.media.push(ConversationMedia {
        json_pointer: "/missing".into(),
        media_type: "application/octet-stream".into(),
        object,
    });
    assert!(
        store
            .append_conversation_event(tenant, None, &first_event, limits().max_object_bytes)
            .await
            .is_err()
    );
    first_event.media[0].json_pointer = "/content".into();
    assert!(
        store
            .append_conversation_event(other_tenant, None, &first_event, limits().max_object_bytes)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn incomplete_capture_can_be_preserved_but_not_exported_as_complete_training_data() {
    let store = memory_store();
    let tenant = Uuid::now_v7();
    let mut first_event = event("first", "main", None, r#"{"output":"partial"}"#);
    first_event.gaps.push("upstream_disconnected".into());
    let head = store
        .append_conversation_event(tenant, None, &first_event, limits().max_object_bytes)
        .await
        .unwrap();
    let root = manifest(tenant, Some(head));
    let object = store
        .put_conversation_manifest(&root, limits())
        .await
        .unwrap();
    assert!(
        store
            .get_conversation_manifest(tenant, &object, 65536)
            .await
            .is_ok()
    );
    assert!(
        store
            .export_conversation(tenant, &object, limits())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn budgets_and_prefix_counts_bound_reads_without_silent_truncation() {
    let store = memory_store();
    let tenant = Uuid::now_v7();
    let first_event = event("first", "main", None, "{}");
    let mut head = store
        .append_conversation_event(tenant, None, &first_event, limits().max_object_bytes)
        .await
        .unwrap();
    let root = manifest(tenant, Some(head.clone()));
    let object = store
        .put_conversation_manifest(&root, limits())
        .await
        .unwrap();
    for restricted in [
        ConversationReadLimits {
            max_events: 0,
            ..limits()
        },
        ConversationReadLimits {
            max_object_bytes: 1,
            ..limits()
        },
        ConversationReadLimits {
            max_total_bytes: object.size_bytes - 1,
            ..limits()
        },
    ] {
        assert!(
            store
                .export_conversation(tenant, &object, restricted)
                .await
                .is_err()
        );
    }
    head.event_count += 1;
    assert!(
        store
            .put_conversation_manifest(&manifest(tenant, Some(head)), limits())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn duplicate_event_ids_and_unresolved_parents_prevent_manifest_publication() {
    let store = memory_store();
    let tenant = Uuid::now_v7();
    let first_event = event("first", "main", None, "{}");
    let prefix = store
        .append_conversation_event(tenant, None, &first_event, limits().max_object_bytes)
        .await
        .unwrap();
    for invalid_event in [
        event("first", "main", Some("first"), "{}"),
        event("second", "main", Some("missing"), "{}"),
    ] {
        let head = store
            .append_conversation_event(
                tenant,
                Some(prefix.clone()),
                &invalid_event,
                limits().max_object_bytes,
            )
            .await
            .unwrap();
        assert!(
            store
                .put_conversation_manifest(&manifest(tenant, Some(head)), limits())
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn unsupported_manifest_version_and_foreign_tenant_are_rejected() {
    let store = memory_store();
    let tenant = Uuid::now_v7();
    let mut root = manifest(tenant, None);
    root.format_version += 1;
    assert!(
        store
            .put_conversation_manifest(&root, limits())
            .await
            .is_err()
    );
    let object = store
        .put_conversation_json(tenant, &root, limits().max_object_bytes)
        .await
        .unwrap();
    assert!(
        store
            .get_conversation_manifest(tenant, &object, 65536)
            .await
            .is_err()
    );
    root.format_version = FORMAT_VERSION;
    let object = store
        .put_conversation_manifest(&root, limits())
        .await
        .unwrap();
    assert!(
        store
            .export_conversation(Uuid::now_v7(), &object, limits())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn empty_or_mislabeled_branches_cannot_be_exported_as_complete() {
    let store = memory_store();
    let tenant = Uuid::now_v7();
    let root = manifest(tenant, None);
    let object = store
        .put_conversation_manifest(&root, limits())
        .await
        .unwrap();
    assert!(
        store
            .export_conversation(tenant, &object, limits())
            .await
            .is_err()
    );
    let first_event = event("first", "main", None, "{}");
    let head = store
        .append_conversation_event(tenant, None, &first_event, limits().max_object_bytes)
        .await
        .unwrap();
    let mut root = manifest(tenant, Some(head));
    root.branch_id = "different".into();
    assert!(
        store
            .put_conversation_manifest(&root, limits())
            .await
            .is_err()
    );
    root.branch_id = "main".into();
    root.parent_branch_id = Some("missing".into());
    assert!(
        store
            .put_conversation_manifest(&root, limits())
            .await
            .is_err()
    );
    root.parent_event_id = Some("first".into());
    assert!(
        store
            .put_conversation_manifest(&root, limits())
            .await
            .is_err()
    );
}
