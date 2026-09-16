use bytes::Bytes;
use futures_util::TryStreamExt;
use object_store::{ObjectStoreExt, PutPayload};

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
