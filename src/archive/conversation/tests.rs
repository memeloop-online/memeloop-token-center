use bytes::Bytes;
use futures_util::TryStreamExt;
use object_store::{ObjectStoreExt, PutPayload};
use serde_json::{Value, json};

use super::*;
use crate::archive::{
    path::{archive_path, tenant_cas_location},
    tests::memory_store,
};

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

// Legacy v1 wire artifacts seeded directly into storage. The JSON field names
// are intentionally independent of the current reader structs.
const TOOL: &str = r#"{ "id":"provider-1", "type":"function_call", "call_id":"call-9", "arguments":"{\"x\":1}", "unknown":18446744073709551616 }"#;
const SSE: &[u8] =
    b"event: response.output_item.done\ndata: {\"item\":{\"call_id\":\"call-9\"}}\n\n";
const PNG: &[u8] = b"\x89PNG\r\n\x1a\nsynthetic";

async fn artifact(store: &ArchiveStore, tenant: Uuid, data: Bytes) -> CasArchiveObject {
    let digest = blake3::hash(&data).to_hex().to_string();
    let location = tenant_cas_location(tenant, &digest, false).unwrap();
    let size_bytes = data.len() as u64;
    store
        .inner
        .put(
            &archive_path(&location).unwrap(),
            PutPayload::from_bytes(data),
        )
        .await
        .unwrap();
    CasArchiveObject {
        object_locator: location,
        blake3_digest: digest,
        size_bytes,
    }
}

fn legacy_cas_reference(object: &CasArchiveObject) -> Value {
    json!({
        "object_locator": object.object_locator,
        "blake3_digest": object.blake3_digest,
        "size_bytes": object.size_bytes,
    })
}

async fn json_artifact(store: &ArchiveStore, tenant: Uuid, value: Value) -> CasArchiveObject {
    artifact(store, tenant, Bytes::from(value.to_string())).await
}

async fn root_reference(store: &ArchiveStore, tenant: Uuid, request: Uuid, bytes: Bytes) {
    let path = format!("tenants/{tenant}/conversations/v1/requests/{request}.json");
    store
        .inner
        .put(&archive_path(&path).unwrap(), PutPayload::from_bytes(bytes))
        .await
        .unwrap();
}

async fn read_export(
    store: &ArchiveStore,
    tenant: Uuid,
    object: &CasArchiveObject,
) -> Result<Vec<Value>, AppError> {
    let lines: Vec<Bytes> = store
        .stream_conversation_export(tenant, object)
        .await?
        .try_collect()
        .await?;
    Ok(lines
        .iter()
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect())
}

fn legacy_manifest(tenant: Uuid, head: Value) -> Value {
    json!({
        "format_version":1,"tenant_id":tenant,"session_id":"session-1",
        "branch_id":"main","parent_branch_id":null,"parent_event_id":null,
        "head":head,"raw_captures":[],"gaps":[]
    })
}

#[tokio::test]
async fn persisted_legacy_conversation_preserves_branch_tool_media_and_raw_bytes() {
    let store = memory_store();
    let tenant = Uuid::from_u128(1);
    let request = Uuid::from_u128(2);
    let tool_bytes = Bytes::from(format!(
        r#"{{"event_id":"first","request_id":"request-1","branch_id":"main","parent_event_id":null,"agent_id":null,"parent_agent_id":null,"origin":"response","role":"assistant","kind":"provider_item","media":[],"gaps":[],"document":{TOOL}}}"#
    ));
    let event = artifact(&store, tenant, tool_bytes.clone()).await;
    let prefix = json!({"event_count":1,"object":json_artifact(&store, tenant,
        json!({"format_version":1,"parent":null,"event":legacy_cas_reference(&event)})).await});
    let media = artifact(&store, tenant, Bytes::from_static(PNG)).await;
    let raw = artifact(&store, tenant, Bytes::from_static(SSE)).await;
    for branch in ["main", "edited"] {
        let result = json_artifact(
            &store,
            tenant,
            json!({
                "event_id":"result","branch_id":branch,"parent_event_id":"first","gaps":[],
                "document":{"role":"tool","tool_call_id":"call-9","source":{"media_ref":"image-1"}},
                "media":[{"json_pointer":"/source","media_type":"image/png","object":legacy_cas_reference(&media)}]
            }),
        )
        .await;
        let node = json_artifact(
            &store,
            tenant,
            json!({"format_version":1,"parent":prefix,"event":legacy_cas_reference(&result)}),
        )
        .await;
        let mut manifest = legacy_manifest(
            tenant,
            json!({"event_count":2,"object":legacy_cas_reference(&node)}),
        );
        manifest["branch_id"] = json!(branch);
        if branch == "edited" {
            manifest["parent_branch_id"] = json!("main");
            manifest["parent_event_id"] = json!("first");
        }
        manifest["raw_captures"] = json!([{"request_id":"request-1","side":"response","format":"sse","object":legacy_cas_reference(&raw)}]);
        let object = json_artifact(&store, tenant, manifest.clone()).await;
        let reference = Bytes::from(legacy_cas_reference(&object).to_string());
        root_reference(&store, tenant, request, reference.clone()).await;
        let restored = store
            .request_conversation_reference(tenant, request)
            .await
            .unwrap();
        assert_eq!(restored, object);
        let lines = read_export(&store, tenant, &restored).await.unwrap();
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0]["manifest"], manifest);
        assert_eq!(lines[1]["ordinal"], 1);
        assert_eq!(lines[1]["parent_event_id"], "first");
        assert_eq!(lines[1]["media"][0]["object"], legacy_cas_reference(&media));
        assert_eq!(lines[2]["ordinal"], 0);
        assert_eq!(lines[2]["object"], legacy_cas_reference(&event));
        assert_eq!(lines[3], json!({"record":"end","events":2,"verified":true}));
        assert_eq!(
            store
                .get_tenant_content(tenant, &event, 4096)
                .await
                .unwrap(),
            tool_bytes
        );
        assert_eq!(
            store.get_tenant_content(tenant, &raw, 4096).await.unwrap(),
            SSE
        );
        let range = store
            .open_stream(&raw.object_locator, Some(7..15))
            .await
            .unwrap();
        assert_eq!(range.range, 7..15);
        let selected: Vec<Bytes> = range.stream.try_collect().await.unwrap();
        assert_eq!(selected.concat().as_slice(), &SSE[7..15]);

        let foreign = Uuid::from_u128(3);
        root_reference(&store, foreign, request, reference).await;
        assert!(
            store
                .request_conversation_reference(foreign, request)
                .await
                .is_err()
        );
        assert!(read_export(&store, foreign, &object).await.is_err());
    }

    // Existing lookups and content reads fail closed on missing and corrupt objects.
    let reference = store
        .request_conversation_reference(tenant, request)
        .await
        .unwrap();
    store
        .inner
        .delete(&archive_path(&media.object_locator).unwrap())
        .await
        .unwrap();
    assert!(read_export(&store, tenant, &reference).await.is_err());
    store
        .inner
        .put(
            &archive_path(&raw.object_locator).unwrap(),
            PutPayload::from_bytes(Bytes::from(vec![b'x'; SSE.len()])),
        )
        .await
        .unwrap();
    assert!(
        store
            .stream_conversation_export(tenant, &reference)
            .await
            .is_err()
    );
    root_reference(&store, tenant, request, Bytes::from(vec![b' '; 4097])).await;
    assert!(
        store
            .request_conversation_reference(tenant, request)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn legacy_stream_accepts_large_opaque_events_and_rejects_incomplete_references() {
    let store = memory_store();
    let tenant = Uuid::from_u128(1);
    let event = json_artifact(
        &store,
        tenant,
        json!({
            "event_id":"large","branch_id":"main","parent_event_id":null,"media":[],"gaps":[],
            "document":{"content":"工具".repeat(350_000)}
        }),
    )
    .await;
    let prefix = json_artifact(
        &store,
        tenant,
        json!({
            "format_version":1,"event":legacy_cas_reference(&event),"parent":null
        }),
    )
    .await;
    let mut manifest = legacy_manifest(
        tenant,
        json!({"event_count":1,"object":legacy_cas_reference(&prefix)}),
    );
    manifest["session_id"] = json!("s".repeat(1024 * 1024 + 1));
    let object = json_artifact(&store, tenant, manifest.clone()).await;
    let lines = read_export(&store, tenant, &object).await.unwrap();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0]["manifest"]["session_id"], manifest["session_id"]);
    assert_eq!(lines[1]["object"], legacy_cas_reference(&event));
    assert!(event.size_bytes > 2 * 1024 * 1024);
    assert!(lines[1].to_string().len() < 65536);

    for (field, invalid) in [
        ("format_version", json!(2)),
        ("head", json!(null)),
        (
            "head",
            json!({"event_count":2,"object":legacy_cas_reference(&prefix)}),
        ),
        ("branch_id", json!("wrong")),
        ("gaps", json!(["upstream_disconnected"])),
    ] {
        let mut broken = manifest.clone();
        broken["session_id"] = json!("session-1");
        broken[field] = invalid;
        let object = json_artifact(&store, tenant, broken).await;
        assert!(
            read_export(&store, tenant, &object).await.is_err(),
            "{field}"
        );
    }
}
