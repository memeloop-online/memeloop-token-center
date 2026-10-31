use super::*;

#[tokio::test]
async fn shutdown_waits_for_already_accepted_request_object_upload() {
    let directory = tempfile::tempdir().unwrap();
    let store = MemoryObjectStore::new();
    let release = store.pause_next_write();
    let admission =
        RequestSpoolAdmission::new(directory.path().to_owned()).with_memory_store(store.clone());
    let body = Bytes::from_static(b"request survives shutdown");
    let request_id = "33333333-3333-4333-8333-333333333333";
    let spool = admission
        .capture_with_memory(
            Body::from(body.clone()),
            body.len(),
            Some(body.len()),
            None,
            0,
            Some(request_id),
        )
        .await
        .unwrap();
    let mut drain = Box::pin(admission.drain_uploads());
    assert!(futures_util::poll!(&mut drain).is_pending());
    release.send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), drain)
        .await
        .unwrap();
    assert!(spool.upload_outcome().await);
    let key = request_body_object_key(request_id).unwrap();
    assert_eq!(store.objects.lock().unwrap().get(&key), Some(&body));
}
