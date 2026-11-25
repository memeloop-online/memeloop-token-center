use super::*;
use serde_json::json;
use std::io::Cursor;

fn scan(value: &[u8], selection: Selection) -> Projection {
    scanner::scan(Cursor::new(value), selection).unwrap()
}

#[test]
fn long_text_and_json_reassemble_on_utf8_byte_boundaries() {
    let text = format!(
        "{}工具😀{}",
        "x".repeat(CHUNK - 1),
        "汉😀\n\\\"".repeat(40000)
    );
    let arguments = json!({"input":text,"nested":[true,null,42]});
    let body = serde_json::to_vec(&json!({"role":"assistant","parts":[
        {"part_index":7,"kind":"tool_call","text":text,"arguments":arguments}
    ]}))
    .unwrap();
    for (field, expected) in [
        ("text", text.clone()),
        ("arguments", serde_json::to_string(&arguments).unwrap()),
    ] {
        let mut output = String::new();
        loop {
            let offset = output.len() as u64;
            let projection = scan(
                &body,
                Selection::Content {
                    part: 0,
                    field: field.into(),
                    offset,
                },
            );
            assert!(projection.text.len() <= CHUNK);
            assert!(projection.complete || !projection.text.is_empty());
            output.push_str(&projection.text);
            if projection.complete {
                break;
            }
        }
        assert_eq!(output, expected);
    }
}

#[test]
fn parts_pages_keep_sparse_part_identity_and_skip_large_values() {
    let parts = (0..67)
        .map(|index| json!({"part_index":index*3,"kind":"text","text":"大".repeat(50000)}))
        .collect::<Vec<_>>();
    let body = serde_json::to_vec(&json!({"role":"user","parts":parts})).unwrap();
    let first = scan(&body, Selection::Parts { start: 0 });
    assert_eq!(first.parts.len(), 32);
    assert_eq!(first.parts[1].part_index, 3);
    assert!(first.more);
    let middle = scan(&body, Selection::Parts { start: 32 });
    assert_eq!(middle.parts[0].part_index, 96);
    assert!(middle.more);
    let last = scan(&body, Selection::Parts { start: 64 });
    assert_eq!(last.parts.len(), 3);
    assert!(!last.more);
    assert!(last.text.is_empty());
}

#[test]
fn escaped_surrogate_pairs_and_malformed_tail_are_checked() {
    let body =
        br#"{"role":"user","parts":[{"part_index":0,"kind":"text","text":"\uD83D\uDE00\n"}]}"#;
    assert_eq!(
        scan(
            body,
            Selection::Content {
                part: 0,
                field: "text".into(),
                offset: 0
            }
        )
        .text,
        "😀\n"
    );
    let mut invalid = body.to_vec();
    invalid.extend_from_slice(b"invalid tail");
    assert!(scanner::scan(Cursor::new(invalid), Selection::Parts { start: 0 }).is_err());
    assert!(
        scanner::scan(
            Cursor::new(body),
            Selection::Content {
                part: 0,
                field: "missing".into(),
                offset: 0
            }
        )
        .is_err()
    );
}

#[tokio::test]
async fn verified_stream_errors_never_return_a_projection() {
    let bytes = Bytes::from_static(br#"{"role":"user","parts":[]}"#);
    let length = bytes.len() as u64;
    let stream =
        futures_util::stream::iter(vec![Ok(bytes), Err(std::io::Error::other("bad digest"))]);
    let download = ArchiveDownload {
        object_size: length,
        range: 0..length,
        stream: Box::pin(stream),
    };
    assert!(
        project(download, Selection::Parts { start: 0 })
            .await
            .is_err()
    );
}

#[tokio::test]
async fn streamed_field_reads_source_once_and_finishes_after_verified_eof() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let text = "文😀".repeat(2_400_000);
    let body = serde_json::to_vec(
        &json!({"role":"assistant","parts":[{"part_index":0,"kind":"reasoning","text":text}]}),
    )
    .unwrap();
    let size = body.len();
    let count = Arc::new(AtomicUsize::new(0));
    let observed = count.clone();
    let chunks = body
        .chunks(8192)
        .map(Bytes::copy_from_slice)
        .collect::<Vec<_>>();
    let source = futures_util::stream::iter(chunks).map(move |bytes| {
        observed.fetch_add(bytes.len(), Ordering::SeqCst);
        Ok(bytes)
    });
    let download = ArchiveDownload {
        object_size: size as u64,
        range: 0..size as u64,
        stream: Box::pin(source),
    };
    let mut output = content_stream(
        download,
        Selection::Content {
            part: 0,
            field: "text".into(),
            offset: 0,
        },
        None,
    );
    let mut combined = String::new();
    let mut complete = false;
    while let Some(piece) = output.next().await {
        let piece = piece.unwrap();
        assert!(!complete);
        assert_eq!(piece.offset, combined.len() as u64);
        assert!(piece.text.len() <= CHUNK);
        combined.push_str(&piece.text);
        complete = piece.complete;
        if complete {
            assert_eq!(count.load(Ordering::SeqCst), size);
        }
    }
    assert!(complete);
    assert_eq!(combined, text);
    assert_eq!(count.load(Ordering::SeqCst), size);
}

#[tokio::test]
async fn streamed_integrity_failure_never_marks_complete() {
    let body = serde_json::to_vec(
        &json!({"role":"user","parts":[{"part_index":0,"kind":"text","text":"x".repeat(CHUNK*3)}]}),
    )
    .unwrap();
    let size = body.len() as u64;
    let source = futures_util::stream::iter(vec![
        Ok(Bytes::from(body)),
        Err(std::io::Error::other("bad CAS tail")),
    ]);
    let mut output = content_stream(
        ArchiveDownload {
            object_size: size,
            range: 0..size,
            stream: Box::pin(source),
        },
        Selection::Content {
            part: 0,
            field: "text".into(),
            offset: 0,
        },
        None,
    );
    let mut error = false;
    while let Some(piece) = output.next().await {
        match piece {
            Ok(piece) => assert!(!piece.complete),
            Err(_) => error = true,
        }
    }
    assert!(error);
}

#[tokio::test]
async fn dropping_content_receiver_releases_the_source() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    struct SourceGuard(Arc<AtomicBool>);
    impl Drop for SourceGuard {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let released = Arc::new(AtomicBool::new(false));
    let guard = SourceGuard(released.clone());
    let body = serde_json::to_vec(&json!({"role":"user","parts":[{"part_index":0,"kind":"text","text":"x".repeat(CHUNK*100)}]})).unwrap();
    let size = body.len() as u64;
    let chunks = body
        .chunks(8192)
        .map(Bytes::copy_from_slice)
        .collect::<std::collections::VecDeque<_>>();
    let source = futures_util::stream::unfold((chunks, guard), |(mut chunks, guard)| async move {
        chunks.pop_front().map(|chunk| (Ok(chunk), (chunks, guard)))
    });
    let mut frames = content_stream(
        ArchiveDownload {
            object_size: size,
            range: 0..size,
            stream: Box::pin(source),
        },
        Selection::Content {
            part: 0,
            field: "text".into(),
            offset: 0,
        },
        None,
    );
    assert!(!frames.next().await.unwrap().unwrap().complete);
    drop(frames);
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while !released.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[test]
fn deep_tool_json_and_extensions_have_no_new_total_size_limits() {
    let nested = format!(
        "{}{}{}",
        "[".repeat(2048),
        "9".repeat(1024),
        "]".repeat(2048)
    );
    let extra = (0..200)
        .map(|i| format!("\"extension{i}\":true,"))
        .collect::<String>();
    let body = format!(
        "{{\"role\":\"assistant\",\"parts\":[{{{extra}\"kind\":\"tool_result\",\"part_index\":0,\"result_content\":{nested}}}]}}"
    );
    let result = scan(
        body.as_bytes(),
        Selection::Content {
            part: 0,
            field: "result_content".into(),
            offset: 0,
        },
    );
    assert_eq!(result.text, nested);
    assert!(result.complete);
}

#[test]
fn long_metadata_has_a_preview_and_full_signed_field_route() {
    let name = "工具😀".repeat(30000);
    let body = serde_json::to_vec(&json!({"role":"assistant","parts":[{"kind":"tool_call","part_index":7,"tool_name":name,"arguments":{}}]})).unwrap();
    let page = scan(&body, Selection::Parts { start: 0 });
    assert!(page.parts[0].metadata_deferred);
    let preview = page.parts[0].tool_name.as_ref().unwrap();
    assert!(name.starts_with(preview));
    assert!(preview.len() <= 16384);
    assert!(
        page.parts[0]
            .fields
            .iter()
            .any(|f| f.name == "tool_name" && f.format == "text")
    );
    let mut output = String::new();
    loop {
        let piece = scan(
            &body,
            Selection::Content {
                part: 0,
                field: "tool_name".into(),
                offset: output.len() as u64,
            },
        );
        output.push_str(&piece.text);
        if piece.complete {
            break;
        }
    }
    assert_eq!(output, name);
}

#[tokio::test]
async fn mismatched_part_index_cannot_complete_a_stream() {
    let body = serde_json::to_vec(
        &json!({"role":"user","parts":[{"kind":"text","part_index":7,"text":"read me"}]}),
    )
    .unwrap();
    let size = body.len() as u64;
    let source = futures_util::stream::once(async move { Ok(Bytes::from(body)) });
    let mut frames = content_stream(
        ArchiveDownload {
            object_size: size,
            range: 0..size,
            stream: Box::pin(source),
        },
        Selection::Content {
            part: 0,
            field: "text".into(),
            offset: 0,
        },
        Some(0),
    );
    assert!(frames.next().await.unwrap().is_err());
}

#[test]
fn parts_previews_share_one_page_budget_and_preserve_field_boundaries() {
    let body = serde_json::to_vec(&json!({"role":"assistant","parts":[
        {"part_index":0,"kind":"text","text":"short"},
        {"part_index":1,"kind":"tool_call","arguments":{"input":"文😀".repeat(CHUNK)}},
        {"part_index":2,"kind":"reasoning","text":"remaining"}
    ]}))
    .unwrap();
    let projection = scan(&body, Selection::Parts { start: 0 });
    let previews = projection
        .parts
        .iter()
        .flat_map(|part| &part.fields)
        .filter_map(|field| field.preview.as_ref())
        .collect::<Vec<_>>();
    assert!(
        previews
            .iter()
            .map(|preview| preview.text.len())
            .sum::<usize>()
            <= CHUNK
    );
    assert_eq!(previews[0].text, "short");
    assert!(previews[0].complete);
    assert!(!previews[1].complete);
    let field = &projection.parts[1].fields[0];
    let preview = field.preview.as_ref().unwrap();
    let continuation = scan(
        &body,
        Selection::Content {
            part: 1,
            field: "arguments".into(),
            offset: preview.text.len() as u64,
        },
    );
    let expected = serde_json::to_string(&json!({"input":"文😀".repeat(CHUNK)})).unwrap();
    assert!(expected.starts_with(&format!("{}{}", preview.text, continuation.text)));
}
