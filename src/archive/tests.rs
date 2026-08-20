use std::sync::atomic::{AtomicUsize, Ordering};

use futures_util::FutureExt;
use object_store::{Extensions, MultipartUpload, PutResult, UploadPart};

use super::*;

#[derive(Debug)]
struct TestMultipartUpload {
    aborts: Arc<AtomicUsize>,
    fail_parts: bool,
}

#[async_trait::async_trait]
impl MultipartUpload for TestMultipartUpload {
    fn put_part(&mut self, _data: PutPayload) -> UploadPart {
        let fail = self.fail_parts;
        async move {
            if fail {
                Err(object_store::Error::Generic {
                    store: "archive-test",
                    source: Box::new(std::io::Error::other("injected part failure")),
                })
            } else {
                Ok(())
            }
        }
        .boxed()
    }

    async fn complete(&mut self) -> object_store::Result<PutResult> {
        Ok(PutResult {
            e_tag: None,
            version: None,
            extensions: Extensions::new(),
        })
    }

    async fn abort(&mut self) -> object_store::Result<()> {
        self.aborts.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn memory_store() -> ArchiveStore {
    ArchiveStore {
        inner: Arc::new(InMemory::new()),
        readiness: Arc::new(tokio::sync::Mutex::new(ReadinessCache::default())),
        readiness_path: path::archive_path("readiness/archive-tests.bin").expect("readiness path"),
    }
}

fn test_writer(aborts: Arc<AtomicUsize>, fail_parts: bool) -> ArchiveWriter {
    let upload = TestMultipartUpload { aborts, fail_parts };
    ArchiveWriter {
        store: Arc::new(InMemory::new()),
        staging: Path::from("staging/test/response.bin"),
        inner: Some(object_store::WriteMultipart::new_with_chunk_size(
            Box::new(upload),
            1,
        )),
        multipart_part_bytes: 1,
        hasher: blake3::Hasher::new(),
        size_bytes: 0,
    }
}

fn test_download_stream(
    chunks: Vec<object_store::Result<Bytes>>,
    expected_bytes: u64,
) -> Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>> {
    verified_download_stream(Box::pin(stream::iter(chunks)), expected_bytes)
}

#[tokio::test]
async fn repeated_content_reuses_one_stable_object_location() {
    let store = memory_store();
    let body = Bytes::from_static(b"same archived body");

    let first = store.put_content(body.clone()).await.expect("first put");
    let second = store.put_content(body.clone()).await.expect("second put");

    assert_eq!(first, second);
    assert!(first.starts_with("objects/blake3/"));
    assert_eq!(
        store
            .get_bounded(&first, body.len())
            .await
            .expect("stored content"),
        body
    );
}

#[tokio::test]
async fn streamed_content_moves_from_staging_to_its_digest_location() {
    let store = memory_store();
    let staging = "staging/request-id/response.bin";
    let mut writer = store.start_writer(staging).await.expect("writer");
    writer
        .write(Bytes::from_static(b"stream "))
        .await
        .expect("first chunk");
    writer
        .write(Bytes::from_static(b"body"))
        .await
        .expect("second chunk");

    let location = writer.finish().await.expect("finish");

    assert!(location.starts_with("objects/blake3/"));
    assert_eq!(
        store
            .get_bounded(&location, b"stream body".len())
            .await
            .expect("stored stream"),
        Bytes::from_static(b"stream body")
    );
    assert!(store.inner.head(&Path::from(staging)).await.is_err());
}

#[tokio::test]
async fn archive_store_allows_twelve_writers_to_make_progress() {
    let store = memory_store();
    let mut writers = Vec::new();
    for index in 0..12 {
        writers.push(
            tokio::time::timeout(
                Duration::from_secs(1),
                store.start_writer(&format!("staging/writer-limit/{index}")),
            )
            .await
            .expect("creating a writer must not wait on another writer")
            .expect("writer"),
        );
    }
    for writer in &mut writers {
        tokio::time::timeout(
            Duration::from_secs(1),
            writer.write(Bytes::from_static(b"x")),
        )
        .await
        .expect("all twelve writers must make progress independently")
        .expect("writer progress");
    }
    for writer in writers {
        writer.abort().await.expect("cleanup writer");
    }
}

#[tokio::test]
async fn staged_streams_are_durable_isolated_and_exactly_cleanable() {
    let store = memory_store();
    let mut first = store
        .start_writer("staging/synchronous/request-a/asset-0")
        .await
        .expect("first staged writer");
    first
        .write(Bytes::from_static(b"first image"))
        .await
        .expect("first staged write");
    let first = first.finish_staged().await.expect("first staged finish");
    let mut second = store
        .start_writer("staging/synchronous/request-b/asset-0")
        .await
        .expect("second staged writer");
    second
        .write(Bytes::from_static(b"second image"))
        .await
        .expect("second staged write");
    let second = second.finish_staged().await.expect("second staged finish");

    assert_eq!(first.size_bytes, b"first image".len() as u64);
    assert_eq!(
        first.blake3_digest,
        blake3::hash(b"first image").to_hex().to_string()
    );
    assert_eq!(
        store
            .get(&first.object_locator)
            .await
            .expect("first object"),
        Bytes::from_static(b"first image")
    );
    assert_eq!(
        store
            .get(&second.object_locator)
            .await
            .expect("second object"),
        Bytes::from_static(b"second image")
    );
    assert!(
        store
            .head_size(&content_location(&first.blake3_digest))
            .await
            .is_err(),
        "finish_staged must not promote into the shared CAS namespace"
    );

    store
        .delete_prefix("staging/synchronous/request-a")
        .await
        .expect("delete exact first staging prefix");
    assert!(store.get(&first.object_locator).await.is_err());
    assert_eq!(
        store
            .get(&second.object_locator)
            .await
            .expect("isolated second object"),
        Bytes::from_static(b"second image")
    );
}

#[tokio::test]
async fn delete_prefix_is_segment_exact_and_rejects_non_canonical_input() {
    let temporary = tempfile::tempdir().expect("temporary archive root");
    let mut filesystem_config = Config::for_test("sqlite::memory:".to_owned());
    filesystem_config.archive_backend = ArchiveBackend::Filesystem;
    filesystem_config.archive_path = Some(temporary.path().to_string_lossy().into_owned());
    let filesystem = ArchiveStore::from_config(&filesystem_config)
        .await
        .expect("filesystem archive store");

    for store in [memory_store(), filesystem] {
        store
            .put("staging/exact-object", Bytes::from_static(b"exact"))
            .await
            .expect("put exact-prefix object");
        store
            .put("staging/x/object", Bytes::from_static(b"x"))
            .await
            .expect("put target descendant");
        store
            .put("staging/x2/object", Bytes::from_static(b"x2"))
            .await
            .expect("put lexical neighbour");

        store
            .delete_prefix("staging/exact-object")
            .await
            .expect("delete object exactly equal to prefix");
        assert!(store.get("staging/exact-object").await.is_err());
        store
            .delete_prefix("staging/x")
            .await
            .expect("delete exact path segment");
        assert!(store.get("staging/x/object").await.is_err());
        assert_eq!(
            store
                .get("staging/x2/object")
                .await
                .expect("lexical neighbour survives"),
            Bytes::from_static(b"x2")
        );

        for unsafe_prefix in [
            "",
            "/",
            ".",
            "..",
            "staging/./x",
            "staging/../x",
            "staging//x",
            "staging/x/",
            "staging\\x",
            "staging/%2e%2e/x",
        ] {
            assert!(
                matches!(
                    store.delete_prefix(unsafe_prefix).await,
                    Err(AppError::BadRequest(_))
                ),
                "unsafe prefix {unsafe_prefix:?} must fail locally"
            );
        }
    }
}

#[tokio::test]
async fn bounded_read_accepts_an_object_exactly_at_the_limit() {
    let store = memory_store();
    let body = Bytes::from_static(b"exactly-at-limit");
    let location = store.put_content(body.clone()).await.expect("put content");

    assert_eq!(
        store
            .get_bounded(&location, body.len())
            .await
            .expect("read at exact limit"),
        body
    );
}

#[tokio::test]
async fn bounded_read_rejects_an_object_over_the_limit() {
    let store = memory_store();
    let body = Bytes::from_static(b"one-byte-too-large");
    let location = store.put_content(body.clone()).await.expect("put content");

    let error = store
        .get_bounded(&location, body.len() - 1)
        .await
        .expect_err("object over limit must be rejected");
    assert!(error.to_string().contains("read limit"));
}

#[tokio::test]
async fn stream_read_returns_the_exact_requested_range_without_collecting_in_store() {
    let store = memory_store();
    let location = store
        .put_content(Bytes::from_static(b"0123456789"))
        .await
        .expect("put ranged fixture");
    let download = store
        .open_stream(&location, Some(2..7))
        .await
        .expect("open bounded stream");
    assert_eq!(download.object_size, 10);
    assert_eq!(download.range, 2..7);
    let chunks = download
        .stream
        .try_collect::<Vec<_>>()
        .await
        .expect("read bounded stream");
    assert_eq!(chunks.concat(), b"23456");
}

#[tokio::test]
async fn download_metadata_rejects_wrong_or_out_of_bounds_ranges() {
    assert_eq!(
        validate_download_range(10, Some(&(2..7)), &(2..7)).expect("exact range"),
        5
    );
    assert!(validate_download_range(10, Some(&(2..7)), &(2..8)).is_err());
    assert!(validate_download_range(10, Some(&(2..7)), &(1..7)).is_err());
    assert!(validate_download_range(10, Some(&(2..11)), &(2..11)).is_err());
    assert!(validate_download_range(10, None, &(0..9)).is_err());
}

#[tokio::test]
async fn download_stream_fails_closed_on_short_or_extra_bodies() {
    let short = test_download_stream(vec![Ok(Bytes::from_static(b"12"))], 3)
        .try_collect::<Vec<_>>()
        .await
        .expect_err("short response must fail");
    assert_eq!(short.kind(), std::io::ErrorKind::UnexpectedEof);

    let oversized = test_download_stream(vec![Ok(Bytes::from_static(b"1234"))], 3)
        .try_collect::<Vec<_>>()
        .await
        .expect_err("oversized chunk must fail before release");
    assert_eq!(oversized.kind(), std::io::ErrorKind::Other);

    let trailing = test_download_stream(
        vec![
            Ok(Bytes::from_static(b"12")),
            Ok(Bytes::from_static(b"3")),
            Ok(Bytes::from_static(b"4")),
        ],
        3,
    )
    .try_collect::<Vec<_>>()
    .await
    .expect_err("trailing bytes must fail before the final chunk is released");
    assert_eq!(trailing.kind(), std::io::ErrorKind::Other);

    let exact = test_download_stream(
        vec![
            Ok(Bytes::from_static(b"12")),
            Ok(Bytes::new()),
            Ok(Bytes::from_static(b"3")),
        ],
        3,
    )
    .try_collect::<Vec<_>>()
    .await
    .expect("exact response");
    assert_eq!(exact.concat(), b"123");

    let empty = test_download_stream(Vec::new(), 0)
        .try_collect::<Vec<_>>()
        .await
        .expect("empty object response");
    assert!(empty.is_empty());
    test_download_stream(vec![Ok(Bytes::from_static(b"unexpected"))], 0)
        .try_collect::<Vec<_>>()
        .await
        .expect_err("a zero-length range cannot return bytes");
}

#[tokio::test]
async fn multipart_part_failure_is_aborted_before_write_returns() {
    let aborts = Arc::new(AtomicUsize::new(0));
    let mut writer = test_writer(aborts.clone(), true);
    writer
        .write(Bytes::from_static(b"p"))
        .await
        .expect_err("part failure must fail the write that queued it");
    assert_eq!(aborts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn multipart_large_write_applies_backpressure_between_parts() {
    let aborts = Arc::new(AtomicUsize::new(0));
    let mut writer = test_writer(aborts.clone(), true);
    writer
        .write(Bytes::from_static(b"part"))
        .await
        .expect_err("one input buffer must be backpressured between multipart chunks");
    assert_eq!(aborts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn dropped_multipart_writer_schedules_best_effort_abort() {
    let aborts = Arc::new(AtomicUsize::new(0));
    let mut writer = test_writer(aborts.clone(), false);
    writer
        .write(Bytes::from_static(b"part"))
        .await
        .expect("queue part");
    drop(writer);

    tokio::time::timeout(Duration::from_secs(1), async {
        while aborts.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("drop abort task completes");
    assert_eq!(aborts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn readiness_canary_is_cleaned_and_success_is_cached() {
    let store = memory_store();
    store.readiness_check().await.expect("first canary");
    store.readiness_check().await.expect("cached canary");
    let prefix = Path::from("readiness");
    let remaining = store
        .inner
        .list(Some(&prefix))
        .try_collect::<Vec<_>>()
        .await
        .expect("list readiness prefix");
    assert!(remaining.is_empty(), "readiness canary must be deleted");
}

#[tokio::test]
async fn object_locations_reject_traversal_and_ambiguous_separators() {
    let store = memory_store();

    for location in [
        "../escape",
        "tenant/../escape",
        "tenant/./object",
        "/absolute/object",
        "tenant//object",
        "tenant/object/",
        "tenant\\object",
        "tenant/%2e%2e/object",
    ] {
        let error = store
            .put(location, Bytes::from_static(b"must not be stored"))
            .await
            .expect_err("unsafe location must be rejected");
        assert!(matches!(error, AppError::BadRequest(_)), "{location}");
        assert!(!error.to_string().contains(location));
    }

    store
        .put(
            "tenants/test-tenant/objects/request_01.bin",
            Bytes::from_static(b"safe"),
        )
        .await
        .expect("safe tenant object location");
}
