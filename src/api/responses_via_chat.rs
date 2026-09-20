//! Provider-neutral Responses-to-Chat compatibility.
//!
//! The implementation is shared with the native Kimi dialect adapter, but
//! callers outside that adapter use this module so future plugin transports do
//! not depend on Kimi-named routing state or types.
use super::AppError;
use serde_json::{Value, json};

pub(in crate::api) use super::kimi_transport::responses::{
    Context, ResponsesViaChatDialect, Stream, buffered,
};

pub(in crate::api) fn prepare_with_dialect(
    model: &str,
    request: &mut Value,
    dialect: ResponsesViaChatDialect,
    // Upstream opt-in: translate Responses compaction requests into direct
    // chat requests (compaction_trigger becomes a summarization
    // instruction, and the upstream text answer is returned as an opaque
    // compaction checkpoint). Without the opt-in a compaction_trigger
    // item is rejected as unsupported.
    translate_compaction: bool,
) -> Result<Context, AppError> {
    let context = Context::with_dialect(request, dialect);
    *request =
        super::kimi_transport::responses_request::convert_with_dialect(
            request,
            dialect,
            translate_compaction,
        )?;
    let object = request
        .as_object_mut()
        .ok_or_else(|| AppError::BadRequest("request body must be an object".into()))?;
    let model = match dialect {
        ResponsesViaChatDialect::OpenAiChatV1 => model.to_owned(),
        ResponsesViaChatDialect::KimiV1 => super::kimi_transport::normalize_model(model),
    };
    object.insert("model".into(), Value::String(model));
    if object.get("stream").and_then(Value::as_bool) == Some(true) {
        let options = object.entry("stream_options").or_insert_with(|| json!({}));
        let options = options
            .as_object_mut()
            .ok_or_else(|| AppError::BadRequest("stream_options must be an object".into()))?;
        options.insert("include_usage".into(), Value::Bool(true));
    }
    Ok(context)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_model_normalization_is_dialect_driven() {
        let mut kimi = json!({"input":"hello"});
        prepare_with_dialect(
            "kimi-k3-256k",
            &mut kimi,
            ResponsesViaChatDialect::KimiV1,
            false,
        )
        .unwrap();
        assert_eq!(kimi["model"], "k3-256k");

        let mut generic = json!({"input":"hello"});
        prepare_with_dialect(
            "kimi-k3-256k",
            &mut generic,
            ResponsesViaChatDialect::OpenAiChatV1,
            false,
        )
        .unwrap();
        assert_eq!(generic["model"], "kimi-k3-256k");
    }
}
