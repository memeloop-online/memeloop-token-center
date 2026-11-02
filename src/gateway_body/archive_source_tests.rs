use super::*;
use bytes::Bytes;

#[tokio::test]
async fn chat_messages_and_responses_share_the_captured_source() {
    let directory = tempfile::tempdir().unwrap();
    let archive = crate::archive::ArchiveStore::from_config(&crate::config::Config::for_test(
        "sqlite::memory:".into(),
    ))
    .await
    .unwrap();
    let admission = request_spool::RequestSpoolAdmission::new(directory.path().to_owned())
        .with_archive(archive);
    let budget = memory::ProxyMemoryBudget::new(32 * 1024 * 1024);
    for path in [
        "/v1/chat/completions",
        "/v1/messages",
        "/v1/responses",
        "/v1/responses/compact",
    ] {
        let request_id = uuid::Uuid::now_v7();
        let body = Bytes::from_static(
            br#"{"model":"test","messages":[{"role":"user","content":"original bytes"}]}"#,
        );
        let mut request = Request::post(path).body(Body::from(body.clone())).unwrap();
        request
            .extensions_mut()
            .insert(request_spool::RequestBodyObjectKey(request_id.to_string()));
        let admitted = admit_gateway_request_body_with_memory(
            request,
            Arc::new(tokio::sync::Semaphore::new(0)),
            Arc::new(tokio::sync::Semaphore::new(0)),
            1024 * 1024,
            1024 * 1024,
            Some(&budget),
            &admission,
        )
        .await
        .unwrap();
        let source = admitted
            .extensions()
            .get::<Arc<request_spool::RequestSpool>>()
            .unwrap()
            .clone();
        assert!(Arc::ptr_eq(&source, &admission.source(request_id).unwrap()));
        assert_eq!(source.read_all().await.unwrap(), body);
        let (parts, delivered) = admitted.into_parts();
        drop(parts);
        if path.starts_with("/v1/responses") {
            assert!(to_bytes(delivered, usize::MAX).await.unwrap().is_empty());
        } else {
            let delivered = to_bytes(delivered, usize::MAX).await.unwrap();
            assert_eq!(delivered, body);
            drop(source);
            assert!(admission.source(request_id).is_some());
            drop(delivered);
            assert!(admission.source(request_id).is_none());
            continue;
        }
        drop(source);
    }
}
