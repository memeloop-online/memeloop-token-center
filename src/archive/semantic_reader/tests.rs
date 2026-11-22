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
