use bytes::Bytes;
use futures_util::TryStreamExt;
use object_store::{ObjectStoreExt, PutPayload};
use uuid::Uuid;

use super::{compressed, path::archive_path, tests::memory_store};

#[tokio::test]
async fn versioned_text_is_smaller_lossless_and_ranges_remain_plaintext() {
    let store = memory_store();
    let body = Bytes::from("data: {\"delta\":\"hello\"}\r\n\r\n".repeat(10_000));
    let mut writer = store
        .start_compressed_writer("staging/test/text")
        .await
        .unwrap();
    // Irregular boundaries are not part of the reconstructed byte contract.
    for chunk in body.chunks(17_003) {
        writer.write(Bytes::copy_from_slice(chunk)).await.unwrap();
    }
    let stored = writer.finish_staged().await.unwrap();
    assert!(stored.object_locator.ends_with(compressed::SUFFIX));
    assert_eq!(stored.size_bytes, body.len() as u64);
    assert_eq!(
        stored.blake3_digest,
        blake3::hash(&body).to_hex().to_string()
    );
    let wire_size = store
        .inner
        .head(&archive_path(&stored.object_locator).unwrap())
        .await
        .unwrap()
        .size;
    assert!(wire_size < body.len() as u64 / 4);
    assert_eq!(
        store.head_size(&stored.object_locator).await.unwrap(),
        body.len() as u64
    );
    assert_eq!(
        store
            .get_bounded(&stored.object_locator, body.len())
            .await
            .unwrap(),
        body
    );
    assert!(
        store
            .get_bounded(&stored.object_locator, body.len() - 1)
            .await
            .is_err()
    );
    for range in [
        0..0,
        7..70_000,
        65_530..65_540,
        body.len() as u64 - 3..body.len() as u64,
    ] {
        let download = store
            .open_stream(&stored.object_locator, Some(range.clone()))
            .await
            .unwrap();
        assert_eq!(download.range, range);
        let output = download
            .stream
            .try_collect::<Vec<_>>()
            .await
            .unwrap()
            .concat();
        assert_eq!(output, body[range.start as usize..range.end as usize]);
    }
    assert!(
        store
            .open_stream(&stored.object_locator, Some(0..body.len() as u64 + 1))
            .await
            .is_err()
    );
    // Old objects are never magic-sniffed, including arbitrary binary data.
    let old = Bytes::from_static(b"MTCZSTD1\0legacy-not-an-envelope");
    store.put("staging/test/legacy", old.clone()).await.unwrap();
    assert_eq!(store.get("staging/test/legacy").await.unwrap(), old);
}

#[tokio::test]
async fn corrupt_frames_lengths_digests_and_trailing_data_fail_closed() {
    let store = memory_store();
    let mut writer = store
        .start_compressed_writer("staging/test/corrupt")
        .await
        .unwrap();
    writer
        .write(Bytes::from(vec![b'a'; compressed::BLOCK * 2]))
        .await
        .unwrap();
    let stored = writer.finish_staged().await.unwrap();
    let path = archive_path(&stored.object_locator).unwrap();
    let original = store.inner.get(&path).await.unwrap().bytes().await.unwrap();
    let mut cases = Vec::new();
    let mut changed = original.to_vec();
    changed[0] ^= 1;
    cases.push(changed);
    let mut changed = original.to_vec();
    changed[16] ^= 1;
    cases.push(changed); // frame digest
    let mut changed = original.to_vec();
    changed[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
    cases.push(changed);
    let mut changed = original.to_vec();
    changed[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
    cases.push(changed);
    let mut changed = original.to_vec();
    let len = changed.len();
    changed[len - 1] ^= 1;
    cases.push(changed);
    let mut changed = original.to_vec();
    let len = changed.len();
    changed[len - 40..len - 32].copy_from_slice(&u64::MAX.to_le_bytes());
    cases.push(changed);
    cases.push(original[..original.len() - 1].to_vec());
    let mut changed = original.to_vec();
    changed.push(0);
    cases.push(changed);
    for changed in cases {
        store
            .inner
            .put(&path, PutPayload::from_bytes(Bytes::from(changed)))
            .await
            .unwrap();
        assert!(
            store
                .get_bounded(&stored.object_locator, compressed::MAX_PLAIN as usize)
                .await
                .is_err()
        );
    }
    // A range cannot silently pass a corrupt final object checksum.
    let mut changed = original.to_vec();
    let len = changed.len();
    changed[len - 1] ^= 1;
    store
        .inner
        .put(&path, PutPayload::from_bytes(Bytes::from(changed)))
        .await
        .unwrap();
    let range = store
        .open_stream(&stored.object_locator, Some(0..1))
        .await
        .unwrap();
    assert!(range.stream.try_collect::<Vec<_>>().await.is_err());
}

#[tokio::test]
async fn empty_compressed_staged_object_round_trips() {
    let store = memory_store();
    let writer = store
        .start_compressed_writer("staging/test/empty")
        .await
        .unwrap();
    let object = writer.finish_staged().await.unwrap();
    assert_eq!(object.size_bytes, 0);
    assert!(store.get(&object.object_locator).await.unwrap().is_empty());
}

#[tokio::test]
async fn compressed_writer_rejects_double_suffix_and_shared_cas_finish() {
    let store = memory_store();
    assert!(matches!(
        store
            .start_compressed_writer("staging/test/content.mtcz1")
            .await,
        Err(crate::error::AppError::BadRequest(_))
    ));
    assert!(
        store
            .inner
            .list(None)
            .try_collect::<Vec<_>>()
            .await
            .unwrap()
            .is_empty()
    );
    let mut writer = store
        .start_compressed_writer("staging/test/content")
        .await
        .unwrap();
    writer.write(Bytes::from_static(b"content")).await.unwrap();
    assert!(matches!(
        writer.finish().await,
        Err(crate::error::AppError::Storage(_))
    ));
    // No staged object was committed and no CAS copy was published. This
    // asserts completed-object state, not timing of the detached abort task.
    assert!(
        store
            .inner
            .list(None)
            .try_collect::<Vec<_>>()
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .inner
            .head(&archive_path("staging/test/content.mtcz1").unwrap())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn tenant_cas_is_concurrent_replayable_and_plaintext_ranged() {
    let store = memory_store();
    let tenant_id = Uuid::now_v7();
    let body = Bytes::from("exact tenant text ".repeat(8_000));
    let mut first = store
        .start_compressed_writer("staging/test/cas-first")
        .await
        .unwrap();
    first.write(body.clone()).await.unwrap();
    let first = first.finish_staged().await.unwrap();
    let mut second = store
        .start_compressed_writer("staging/test/cas-second")
        .await
        .unwrap();
    second.write(body.clone()).await.unwrap();
    let second = second.finish_staged().await.unwrap();

    let (left, right) = tokio::join!(
        store.promote_staged_text_to_cas(tenant_id, &first),
        store.promote_staged_text_to_cas(tenant_id, &second),
    );
    let left = left.unwrap();
    let right = right.unwrap();
    assert_eq!(left, right);
    let other_tenant = store
        .promote_staged_text_to_cas(Uuid::now_v7(), &first)
        .await
        .unwrap();
    assert_ne!(other_tenant.object_locator, left.object_locator);
    assert!(left.object_locator.ends_with(compressed::SUFFIX));
    assert!(
        left.object_locator
            .contains(&format!("tenants/{tenant_id}/cas/v1/"))
    );
    assert_eq!(store.get(&left.object_locator).await.unwrap(), body);
    let selected = 65_530..65_550;
    let ranged = store
        .open_stream(&left.object_locator, Some(selected.clone()))
        .await
        .unwrap()
        .stream
        .try_collect::<Vec<_>>()
        .await
        .unwrap()
        .concat();
    assert_eq!(ranged, body[selected.start as usize..selected.end as usize]);
    assert_eq!(store.get(&first.object_locator).await.unwrap(), body);
    assert_eq!(store.get(&second.object_locator).await.unwrap(), body);
    assert!(store.delete(&left.object_locator).await.is_err());
    store
        .delete_prefix(&format!("tenants/{tenant_id}"))
        .await
        .unwrap();
    assert_eq!(store.get(&left.object_locator).await.unwrap(), body);
}

#[tokio::test]
async fn cas_recovery_after_publish_before_database_commit_reuses_existing_object() {
    let store = memory_store();
    let tenant_id = Uuid::now_v7();
    let body = Bytes::from_static(b"replay after the CAS create crash window");
    let mut first = store
        .start_compressed_writer("staging/test/crash-first")
        .await
        .unwrap();
    first.write(body.clone()).await.unwrap();
    let first = first.finish_staged().await.unwrap();
    let published = store
        .promote_staged_text_to_cas(tenant_id, &first)
        .await
        .unwrap();

    // Simulate a process loss before the relational transaction published the
    // locator. A new attempt has a new staging key but reaches the same CAS.
    let mut retry = store
        .start_compressed_writer("staging/test/crash-retry")
        .await
        .unwrap();
    retry.write(body.clone()).await.unwrap();
    let retry = retry.finish_staged().await.unwrap();
    let recovered = store
        .promote_staged_text_to_cas(tenant_id, &retry)
        .await
        .unwrap();
    assert_eq!(recovered, published);
    assert_eq!(store.get(&recovered.object_locator).await.unwrap(), body);
    assert!(store.get(&first.object_locator).await.is_ok());
    assert!(store.get(&retry.object_locator).await.is_ok());
}

#[tokio::test]
async fn already_existing_cas_must_match_length_and_digest() {
    let store = memory_store();
    let mut writer = store
        .start_compressed_writer("staging/test/expected")
        .await
        .unwrap();
    writer.write(Bytes::from_static(b"expected")).await.unwrap();
    let staged = writer.finish_staged().await.unwrap();
    let mut wrong_size = store
        .start_compressed_writer("staging/test/wrong-size")
        .await
        .unwrap();
    wrong_size
        .write(Bytes::from_static(b"different"))
        .await
        .unwrap();
    let wrong_size = wrong_size.finish_staged().await.unwrap();
    let size_tenant = Uuid::now_v7();
    let size_locator =
        super::path::tenant_cas_location(size_tenant, &staged.blake3_digest, true).unwrap();
    store
        .inner
        .copy(
            &archive_path(&wrong_size.object_locator).unwrap(),
            &archive_path(&size_locator).unwrap(),
        )
        .await
        .unwrap();
    assert!(
        store
            .promote_staged_text_to_cas(size_tenant, &staged)
            .await
            .is_err()
    );

    let mut wrong_digest = store
        .start_compressed_writer("staging/test/wrong-digest")
        .await
        .unwrap();
    wrong_digest
        .write(Bytes::from_static(b"wrong!!!"))
        .await
        .unwrap();
    let wrong_digest = wrong_digest.finish_staged().await.unwrap();
    let digest_tenant = Uuid::now_v7();
    let digest_locator =
        super::path::tenant_cas_location(digest_tenant, &staged.blake3_digest, true).unwrap();
    store
        .inner
        .copy(
            &archive_path(&wrong_digest.object_locator).unwrap(),
            &archive_path(&digest_locator).unwrap(),
        )
        .await
        .unwrap();
    assert!(
        store
            .promote_staged_text_to_cas(digest_tenant, &staged)
            .await
            .is_err()
    );
    assert!(store.get(&staged.object_locator).await.is_ok());
}

#[tokio::test]
async fn legacy_raw_versioned_text_and_tenant_cas_remain_readable_together() {
    let store = memory_store();
    let legacy = Bytes::from_static(b"legacy raw archive");
    store
        .put("objects/legacy/raw", legacy.clone())
        .await
        .unwrap();
    let versioned = Bytes::from_static(b"existing versioned archive");
    let mut old_writer = store
        .start_compressed_writer("staging/test/versioned-old")
        .await
        .unwrap();
    old_writer.write(versioned.clone()).await.unwrap();
    let old = old_writer.finish_staged().await.unwrap();
    let cas_body = Bytes::from_static(b"new tenant exact CAS archive");
    let mut cas_writer = store
        .start_compressed_writer("staging/test/versioned-cas")
        .await
        .unwrap();
    cas_writer.write(cas_body.clone()).await.unwrap();
    let staged = cas_writer.finish_staged().await.unwrap();
    let cas = store
        .promote_staged_text_to_cas(Uuid::now_v7(), &staged)
        .await
        .unwrap();

    assert_eq!(store.get("objects/legacy/raw").await.unwrap(), legacy);
    assert_eq!(store.get(&old.object_locator).await.unwrap(), versioned);
    assert_eq!(store.get(&cas.object_locator).await.unwrap(), cas_body);
}
