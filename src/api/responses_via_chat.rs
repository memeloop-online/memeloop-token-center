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

pub(in crate::api) fn prepare(model: &str, request: &mut Value) -> Result<Context, AppError> {
    prepare_with_dialect(model, request, ResponsesViaChatDialect::OpenAiChatV1)
}

pub(in crate::api) fn prepare_with_dialect(
    model: &str,
    request: &mut Value,
    dialect: ResponsesViaChatDialect,
) -> Result<Context, AppError> {
    let context = Context::with_dialect(request, dialect);
    *request = super::kimi_transport::responses_request::convert_with_dialect(request, dialect)?;
    let object = request
        .as_object_mut()
        .ok_or_else(|| AppError::BadRequest("request body must be an object".into()))?;
    object.insert("model".into(), Value::String(model.to_owned()));
    if object.get("stream").and_then(Value::as_bool) == Some(true) {
        let options = object.entry("stream_options").or_insert_with(|| json!({}));
        let options = options
            .as_object_mut()
            .ok_or_else(|| AppError::BadRequest("stream_options must be an object".into()))?;
        options.insert("include_usage".into(), Value::Bool(true));
    }
    Ok(context)
}
