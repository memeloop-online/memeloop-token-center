use super::*;

#[derive(Clone, Default)]
struct MetadataLogCapture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

struct MetadataLogWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for MetadataLogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for MetadataLogCapture {
    type Writer = MetadataLogWriter;

    fn make_writer(&'writer self) -> Self::Writer {
        MetadataLogWriter(self.0.clone())
    }
}

#[test]
fn warn_only_preparation_keeps_typed_ids_and_safe_rejection_metadata() {
    let capture = MetadataLogCapture::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::WARN)
        .with_current_span(true)
        .with_span_list(false)
        .without_time()
        .with_writer(capture.clone())
        .finish();
    let request_id = uuid::Uuid::from_u128(1);
    let account_id = uuid::Uuid::from_u128(2);
    tracing::subscriber::with_default(subscriber, || {
        for kind in ["compaction_trigger", "private-type-canary"] {
            let mut request = json!({"model":"kimi-k3","input":[
                {"role":"user","content":"private-history-canary"},
                {"type":kind,"encrypted_content":"private-ciphertext-canary",
                    "arguments":"private-arguments-canary","output":"private-output-canary",
                    "private-key-canary":"private-value-canary"}
            ]});
            let original = request.clone();
            let result = with_input_rejection_diagnostics(request_id, account_id, 7, || {
                crate::api::responses_via_chat::prepare_with_dialect(
                    "kimi-k3",
                    &mut request,
                    ResponsesViaChatDialect::KimiV1,
                    false,
                )
            });
            assert!(result.is_err());
            assert_eq!(request, original);
        }
    });
    let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert!(!logs.contains("private-"));
    let events: Vec<Value> = logs
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(events.len(), 2);
    for (event, expected_kind) in events.iter().zip(["compaction_trigger", "unknown_type"]) {
        assert_eq!(event["level"], "WARN");
        assert_eq!(event["span"]["request_id"], request_id.to_string());
        assert_eq!(event["span"]["upstream_account_id"], account_id.to_string());
        assert_eq!(event["span"]["credential_generation"], 7);
        assert_eq!(event["fields"]["phase"], "responses_chat_input_rejected");
        assert_eq!(event["fields"]["input_index"], 1);
        assert_eq!(event["fields"]["input_kind"], expected_kind);
        assert_eq!(event["fields"]["dialect"], "kimi_v1");
        assert_eq!(event["fields"]["compaction_translation_enabled"], false);
    }
}

#[test]
fn actual_conversion_rejection_logs_kind_index_and_flag_without_payload() {
    let capture = MetadataLogCapture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(capture.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let request = json!({"model":"kimi-k3","input":[
            {"role":"user","content":"private-history-canary"},
            {"type":"compaction_trigger","encrypted_content":"private-ciphertext-canary"}
        ]});
        assert!(convert_with_dialect(&request, ResponsesViaChatDialect::KimiV1, false).is_err());
    });
    let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("responses_chat_input_rejected"));
    assert!(logs.contains("input_index=1"));
    assert!(logs.contains("input_kind=\"compaction_trigger\""));
    assert!(logs.contains("dialect=\"kimi_v1\""));
    assert!(logs.contains("compaction_translation_enabled=false"));
    assert!(!logs.contains("private-"));
}

#[test]
fn compact_rejection_metadata_identifies_only_code_owned_fields() {
    let metadata = rejected_input_metadata(
        3,
        &json!({"type":"compaction_trigger","private_payload":"not-for-logs"}),
        ResponsesViaChatDialect::KimiV1,
        false,
    );
    assert_eq!(
        metadata,
        RejectedInputMetadata {
            input_index: 3,
            input_kind: "compaction_trigger",
            dialect: "kimi_v1",
            compaction_translation_enabled: false,
        }
    );
    assert!(!format!("{metadata:?}").contains("not-for-logs"));
}

#[test]
fn unknown_input_discriminants_and_values_are_not_echoed() {
    for item in [
        json!({"type":"private-type-canary","encrypted_content":"private-ciphertext-canary"}),
        json!({"type":{"private":"private-type-canary"},"output":"private-tool-output-canary"}),
    ] {
        let metadata =
            rejected_input_metadata(7, &item, ResponsesViaChatDialect::OpenAiChatV1, true);
        assert_eq!(metadata.input_index, 7);
        assert_eq!(metadata.dialect, "openai_chat_v1");
        assert!(metadata.compaction_translation_enabled);
        assert!(matches!(
            metadata.input_kind,
            "unknown_type" | "missing_or_invalid_type"
        ));
        assert!(!format!("{metadata:?}").contains("private-"));
    }
}

#[test]
fn unsupported_items_keep_existing_rejection_and_opt_in_boundary() {
    for kind in ["compaction_trigger", "private-type-canary"] {
        let request = json!({"model":"kimi-k3","input":[
            {"role":"user","content":"private-history-canary"},
            {"type":kind,"output":"private-tool-output-canary"}
        ]});
        let original = request.clone();
        let error =
            convert_with_dialect(&request, ResponsesViaChatDialect::KimiV1, false).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unsupported Responses input item for Responses-via-Chat")
        );
        assert!(!error.to_string().contains("private-"));
        assert_eq!(request, original);
    }
    let request = json!({"model":"kimi-k3","input":[
        {"role":"user","content":"history"},
        {"type":"compaction_trigger"}
    ]});
    let converted = convert_with_dialect(&request, ResponsesViaChatDialect::KimiV1, true).unwrap();
    let messages = converted["messages"].as_array().unwrap();
    assert_eq!(messages.last().unwrap()["content"], COMPACTION_INSTRUCTION);
}
