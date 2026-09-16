use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, StatusCode, header},
    response::Response,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{AppState, error::AppError};

pub(super) const MAX_ARCHIVE_DETAIL_BODY: usize = 1024 * 1024;
const MAX_ARCHIVE_DETAIL_JSON_DEPTH: usize = 64;
const MAX_ARCHIVE_DETAIL_JSON_NODES: usize = 16 * 1024;
const MAX_ARCHIVE_DETAIL_JSON_STRING_BYTES: usize = 256 * 1024;
const MAX_ARCHIVE_DETAIL_JSON_ARRAY_ITEMS: usize = 8 * 1024;
const MAX_ARCHIVE_DETAIL_JSON_OBJECT_FIELDS: usize = 4 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(in crate::api) enum RequestArchiveSide {
    Request,
    Response,
}

enum ArchiveContentSource<'a> {
    Inline(Bytes),
    Object(&'a str),
}

pub(super) async fn request_detail(
    state: &AppState,
    refs: crate::model::RequestArchiveRefs,
) -> crate::model::RequestDetail {
    let request = if archive_is_pending(refs.request_archive_state) {
        ArchiveValue::pending()
    } else {
        archive_value(state, &refs.request_object).await
    };
    let response = if archive_is_pending(refs.response_archive_state) {
        ArchiveValue::pending()
    } else {
        match refs.response_object.as_deref() {
            Some(location) => archive_value(state, location).await,
            None => match refs.response_json {
                Some(value) if json_value_structure_is_bounded(&value) => ArchiveValue {
                    value,
                    complete: true,
                    reason: None,
                },
                Some(_) => ArchiveValue::gap("archive_payload_invalid"),
                None if matches!(
                    refs.response_archive_state,
                    crate::model::RequestArchiveState::Bound
                        | crate::model::RequestArchiveState::MetadataOnly
                ) =>
                {
                    ArchiveValue {
                        value: Value::Null,
                        complete: true,
                        reason: None,
                    }
                }
                None => ArchiveValue {
                    value: Value::Null,
                    complete: false,
                    reason: refs.response_archive_reason.clone(),
                },
            },
        }
    };
    crate::model::RequestDetail {
        view: refs.view,
        request_body: request.value,
        response_body: response.value,
        archive_complete: request.complete && response.complete,
        archive: crate::model::RequestArchiveCompletenessView {
            request: crate::model::RequestArchiveSideView {
                state: refs.request_archive_state,
                complete: request.complete,
                reason: refs.request_archive_reason.or(request.reason),
            },
            response: crate::model::RequestArchiveSideView {
                state: refs.response_archive_state,
                complete: response.complete,
                reason: refs.response_archive_reason.or(response.reason),
            },
        },
        provenance: refs.provenance,
    }
}

/// Stream the exact archived bytes independently from the bounded request-detail
/// JSON projection. Object-backed bodies stay incremental even when the client
/// requests the entire representation; callers can use a single byte range for
/// paged viewing without imposing a total archive-size limit.
pub(in crate::api) async fn request_archive_content_response(
    state: &AppState,
    headers: &HeaderMap,
    refs: &crate::model::RequestArchiveRefs,
    side: RequestArchiveSide,
) -> Result<Response, AppError> {
    let source = match archive_content_source(refs, side) {
        Ok(source) => source,
        Err(reason) => return archive_content_unavailable(reason),
    };
    match source {
        ArchiveContentSource::Inline(bytes) => inline_archive_content_response(headers, bytes),
        ArchiveContentSource::Object(location) => {
            object_archive_content_response(state, headers, location).await
        }
    }
}

fn archive_content_source(
    refs: &crate::model::RequestArchiveRefs,
    side: RequestArchiveSide,
) -> Result<ArchiveContentSource<'_>, &'static str> {
    let (state, reason, location) = match side {
        RequestArchiveSide::Request => (
            refs.request_archive_state,
            refs.request_archive_reason.as_deref(),
            Some(refs.request_object.as_str()),
        ),
        RequestArchiveSide::Response => (
            refs.response_archive_state,
            refs.response_archive_reason.as_deref(),
            refs.response_object.as_deref(),
        ),
    };
    if archive_is_pending(state) {
        return Err("archive_pending");
    }
    if state == crate::model::RequestArchiveState::MetadataOnly {
        return Err("media_body_not_archived_by_policy");
    }
    if let Some(location) = location {
        if let Some(value) = location.strip_prefix("inline-json:") {
            return Ok(ArchiveContentSource::Inline(Bytes::copy_from_slice(
                value.as_bytes(),
            )));
        }
        if location.starts_with("metadata-only-json:")
            || location.starts_with("provider-reference-json:")
        {
            return Err("media_body_not_archived_by_policy");
        }
        if location.starts_with("gap://") {
            return Err("archive_object_unavailable");
        }
        return Ok(ArchiveContentSource::Object(location));
    }
    if side == RequestArchiveSide::Response {
        if let Some(value) = refs.response_json.as_ref() {
            let bytes = serde_json::to_vec(value).map_err(|_| "archive_payload_invalid")?;
            return Ok(ArchiveContentSource::Inline(Bytes::from(bytes)));
        }
        if state == crate::model::RequestArchiveState::Bound {
            return Ok(ArchiveContentSource::Inline(Bytes::from_static(b"null")));
        }
    }
    Err(reason
        .and_then(public_archive_unavailable_reason)
        .unwrap_or("archive_object_unavailable"))
}

fn public_archive_unavailable_reason(reason: &str) -> Option<&'static str> {
    match reason {
        "archive_pending" => Some("archive_pending"),
        "archive_object_unavailable" => Some("archive_object_unavailable"),
        "archive_payload_invalid" => Some("archive_payload_invalid"),
        "media_body_not_archived_by_policy" => Some("media_body_not_archived_by_policy"),
        _ => None,
    }
}

fn inline_archive_content_response(
    headers: &HeaderMap,
    bytes: Bytes,
) -> Result<Response, AppError> {
    let size = u64::try_from(bytes.len()).map_err(|_| AppError::Internal)?;
    let etag = format!("\"{}\"", blake3::hash(&bytes).to_hex());
    if !if_match_satisfied(headers, &etag) {
        return precondition_failed(&etag);
    }
    let requested_range = match requested_byte_range(headers, size) {
        Ok(range) => range,
        Err(()) => return range_not_satisfiable(size, Some(&etag)),
    };
    let range = requested_range.clone().unwrap_or(0..size);
    let start = usize::try_from(range.start).map_err(|_| AppError::Internal)?;
    let end = usize::try_from(range.end).map_err(|_| AppError::Internal)?;
    archive_content_success(
        requested_range.is_some(),
        size,
        range,
        &etag,
        Body::from(bytes.slice(start..end)),
    )
}

async fn object_archive_content_response(
    state: &AppState,
    headers: &HeaderMap,
    location: &str,
) -> Result<Response, AppError> {
    let size = match state.archive.head_size(location).await {
        Ok(size) => size,
        Err(_) => {
            tracing::warn!(
                error_code = "archive_object_unavailable",
                "archived request content is unavailable"
            );
            return archive_content_unavailable("archive_object_unavailable");
        }
    };
    let etag = archive_object_etag(location, size);
    if !if_match_satisfied(headers, &etag) {
        return precondition_failed(&etag);
    }
    let requested_range = match requested_byte_range(headers, size) {
        Ok(range) => range,
        Err(()) => return range_not_satisfiable(size, Some(&etag)),
    };
    let download = match state
        .archive
        .open_stream(location, requested_range.clone())
        .await
    {
        Ok(download) => download,
        Err(_) => {
            tracing::warn!(
                error_code = "archive_object_unavailable",
                "archived request content could not be opened"
            );
            return archive_content_unavailable("archive_object_unavailable");
        }
    };
    if download.object_size != size {
        return Err(AppError::Storage(
            "request archive changed during download".to_owned(),
        ));
    }
    let expected_range = requested_range.clone().unwrap_or(0..size);
    if download.range != expected_range {
        return Err(AppError::Storage(
            "request archive returned an unexpected range".to_owned(),
        ));
    }
    archive_content_success(
        requested_range.is_some(),
        size,
        download.range,
        &etag,
        Body::from_stream(download.stream),
    )
}

fn archive_object_etag(location: &str, size: u64) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"mtc-request-archive-content-etag-v1\0");
    hasher.update(location.as_bytes());
    hasher.update(b"\0");
    hasher.update(&size.to_be_bytes());
    format!("\"{}\"", hasher.finalize().to_hex())
}

fn requested_byte_range(
    headers: &HeaderMap,
    size: u64,
) -> Result<Option<std::ops::Range<u64>>, ()> {
    let mut values = headers.get_all(header::RANGE).iter();
    let first = values.next();
    if values.next().is_some() {
        return Err(());
    }
    let value = first
        .map(axum::http::HeaderValue::to_str)
        .transpose()
        .map_err(|_| ())?;
    crate::api::generation::parse_byte_range(value, size)
}

fn if_match_satisfied(headers: &HeaderMap, etag: &str) -> bool {
    let values = headers.get_all(header::IF_MATCH);
    if values.iter().next().is_none() {
        return true;
    }
    values.iter().any(|value| {
        value.to_str().ok().is_some_and(|value| {
            value
                .split(',')
                .map(str::trim)
                .any(|candidate| candidate == "*" || candidate == etag)
        })
    })
}

fn archive_content_success(
    partial: bool,
    size: u64,
    range: std::ops::Range<u64>,
    etag: &str,
    body: Body,
) -> Result<Response, AppError> {
    let mut response = Response::builder()
        .status(if partial {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        })
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(
            header::CONTENT_LENGTH,
            range.end.saturating_sub(range.start),
        )
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CACHE_CONTROL, "private, no-store")
        .header(header::ETAG, etag)
        .header("x-content-type-options", "nosniff");
    if partial {
        response = response.header(
            header::CONTENT_RANGE,
            format!(
                "bytes {}-{}/{}",
                range.start,
                range.end.saturating_sub(1),
                size
            ),
        );
    }
    response.body(body).map_err(|_| AppError::Internal)
}

fn archive_content_unavailable(reason: &str) -> Result<Response, AppError> {
    let body = serde_json::to_vec(&json!({
        "error": {
            "code": "archive_content_unavailable",
            "message": "archived request content is unavailable",
            "reason": reason,
        }
    }))
    .map_err(|_| AppError::Internal)?;
    Response::builder()
        .status(StatusCode::CONFLICT)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CONTENT_LENGTH, body.len())
        .header(header::CACHE_CONTROL, "private, no-store")
        .body(Body::from(body))
        .map_err(|_| AppError::Internal)
}

fn precondition_failed(etag: &str) -> Result<Response, AppError> {
    Response::builder()
        .status(StatusCode::PRECONDITION_FAILED)
        .header(header::CONTENT_LENGTH, 0)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CACHE_CONTROL, "private, no-store")
        .header(header::ETAG, etag)
        .body(Body::empty())
        .map_err(|_| AppError::Internal)
}

fn range_not_satisfiable(size: u64, etag: Option<&str>) -> Result<Response, AppError> {
    let mut response = Response::builder()
        .status(StatusCode::RANGE_NOT_SATISFIABLE)
        .header(header::CONTENT_RANGE, format!("bytes */{size}"))
        .header(header::CONTENT_LENGTH, 0)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CACHE_CONTROL, "private, no-store");
    if let Some(etag) = etag {
        response = response.header(header::ETAG, etag);
    }
    response.body(Body::empty()).map_err(|_| AppError::Internal)
}

struct ArchiveValue {
    value: Value,
    complete: bool,
    reason: Option<String>,
}

fn archive_is_pending(state: crate::model::RequestArchiveState) -> bool {
    matches!(
        state,
        crate::model::RequestArchiveState::Capturing
            | crate::model::RequestArchiveState::Pending
            | crate::model::RequestArchiveState::Uploading
    )
}

impl ArchiveValue {
    fn pending() -> Self {
        Self {
            value: Value::Null,
            complete: false,
            reason: None,
        }
    }

    fn gap(reason: &str) -> Self {
        Self {
            value: Value::Null,
            complete: false,
            reason: Some(reason.to_owned()),
        }
    }
}

async fn archive_value(state: &AppState, location: &str) -> ArchiveValue {
    if location.starts_with("provider-reference-json:") {
        return ArchiveValue {
            value: serde_json::json!({"media_archived": false}),
            complete: true,
            reason: Some("media_body_not_archived_by_policy".to_owned()),
        };
    }
    if let Some(value) = location.strip_prefix("metadata-only-json:") {
        let mut metadata = decode_archive_value(value.as_bytes());
        metadata.complete = true;
        metadata.reason = Some("media_body_not_archived_by_policy".to_owned());
        return metadata;
    }
    if let Some(value) = location.strip_prefix("inline-json:") {
        return decode_archive_value(value.as_bytes());
    }
    if location.starts_with("gap://") {
        return ArchiveValue::gap("archive_object_unavailable");
    }
    match state
        .archive
        .get_bounded(location, MAX_ARCHIVE_DETAIL_BODY)
        .await
    {
        Ok(bytes) => decode_archive_value(&bytes),
        Err(_) => {
            // Imported locators and object-store errors can contain provider
            // paths, signed query values, or storage identity metadata.
            tracing::warn!(
                error_code = "archive_object_unavailable",
                "archived request object is unavailable"
            );
            ArchiveValue::gap("archive_object_unavailable")
        }
    }
}

fn decode_archive_value(bytes: &[u8]) -> ArchiveValue {
    if bytes.len() > MAX_ARCHIVE_DETAIL_BODY || !json_bytes_structure_is_bounded(bytes) {
        return ArchiveValue::gap("archive_payload_invalid");
    }
    match serde_json::from_slice(bytes) {
        Ok(value) if json_value_structure_is_bounded(&value) => ArchiveValue {
            value,
            complete: true,
            reason: None,
        },
        Ok(_) => ArchiveValue::gap("archive_payload_invalid"),
        Err(_) => ArchiveValue {
            value: Value::String(String::from_utf8_lossy(bytes).into_owned()),
            complete: true,
            reason: None,
        },
    }
}

/// Reject highly expanded JSON before serde allocates a `Value`. This is a
/// conservative structural scan; serde_json remains the source of truth for
/// syntax after the depth/node budget is proven bounded.
fn json_bytes_structure_is_bounded(bytes: &[u8]) -> bool {
    let mut depth = 0_usize;
    let mut nodes = 1_usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut string_bytes = 0_usize;
    let mut containers = Vec::<(u8, usize)>::new();
    for &byte in bytes {
        if in_string {
            string_bytes = string_bytes.saturating_add(1);
            if string_bytes > MAX_ARCHIVE_DETAIL_JSON_STRING_BYTES {
                return false;
            }
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => {
                in_string = true;
                string_bytes = 0;
            }
            b'[' | b'{' => {
                depth = depth.saturating_add(1);
                nodes = nodes.saturating_add(1);
                containers.push((byte, 0));
                if depth > MAX_ARCHIVE_DETAIL_JSON_DEPTH || nodes > MAX_ARCHIVE_DETAIL_JSON_NODES {
                    return false;
                }
            }
            b']' | b'}' => {
                depth = depth.saturating_sub(1);
                containers.pop();
            }
            b',' => {
                nodes = nodes.saturating_add(1);
                if nodes > MAX_ARCHIVE_DETAIL_JSON_NODES {
                    return false;
                }
                if let Some((kind, commas)) = containers.last_mut() {
                    *commas = commas.saturating_add(1);
                    let limit = if *kind == b'[' {
                        MAX_ARCHIVE_DETAIL_JSON_ARRAY_ITEMS
                    } else {
                        MAX_ARCHIVE_DETAIL_JSON_OBJECT_FIELDS
                    };
                    if *commas >= limit {
                        return false;
                    }
                }
            }
            _ => {}
        }
    }
    true
}

pub(super) fn json_value_structure_is_bounded(root: &Value) -> bool {
    let mut stack = vec![(root, 0_usize)];
    let mut nodes = 0_usize;
    while let Some((value, depth)) = stack.pop() {
        nodes = nodes.saturating_add(1);
        if nodes > MAX_ARCHIVE_DETAIL_JSON_NODES || depth > MAX_ARCHIVE_DETAIL_JSON_DEPTH {
            return false;
        }
        match value {
            Value::Array(values) => {
                if values.len() > MAX_ARCHIVE_DETAIL_JSON_ARRAY_ITEMS {
                    return false;
                }
                stack.extend(values.iter().map(|value| (value, depth.saturating_add(1))));
            }
            Value::Object(values) => {
                if values.len() > MAX_ARCHIVE_DETAIL_JSON_OBJECT_FIELDS
                    || values.keys().any(|key| {
                        key.len() > MAX_ARCHIVE_DETAIL_JSON_STRING_BYTES
                            || key.chars().any(char::is_control)
                    })
                {
                    return false;
                }
                stack.extend(
                    values
                        .values()
                        .map(|value| (value, depth.saturating_add(1))),
                );
            }
            Value::String(value) if value.len() > MAX_ARCHIVE_DETAIL_JSON_STRING_BYTES => {
                return false;
            }
            _ => {}
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_detail_json_is_rejected_before_structural_expansion() {
        let flat_array = format!(
            "[{}]",
            std::iter::repeat_n("0", MAX_ARCHIVE_DETAIL_JSON_ARRAY_ITEMS + 1)
                .collect::<Vec<_>>()
                .join(",")
        );
        assert!(!json_bytes_structure_is_bounded(flat_array.as_bytes()));
        let rejected = decode_archive_value(flat_array.as_bytes());
        assert_eq!(rejected.value, Value::Null);
        assert!(!rejected.complete);
        assert_eq!(rejected.reason.as_deref(), Some("archive_payload_invalid"));

        let deep = format!(
            "{}0{}",
            "[".repeat(MAX_ARCHIVE_DETAIL_JSON_DEPTH + 1),
            "]".repeat(MAX_ARCHIVE_DETAIL_JSON_DEPTH + 1)
        );
        assert!(!json_bytes_structure_is_bounded(deep.as_bytes()));

        let large_string = format!(
            "\"{}\"",
            "x".repeat(MAX_ARCHIVE_DETAIL_JSON_STRING_BYTES + 1)
        );
        assert!(!json_bytes_structure_is_bounded(large_string.as_bytes()));

        let safe = br#"{"items":[1,2,3],"text":"brackets [inside] a string"}"#;
        assert!(json_bytes_structure_is_bounded(safe));
        assert!(decode_archive_value(safe).complete);
    }
}
