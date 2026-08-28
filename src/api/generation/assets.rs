use super::super::*;

pub(in crate::api) async fn self_generations(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<RequestsQuery>,
) -> Result<impl IntoResponse, AppError> {
    let key = authenticate_downstream(&headers, &state).await?;
    Ok(Json(
        state
            .db
            .list_generation_jobs(key.key_id, query.limit)
            .await?,
    ))
}

pub(in crate::api) async fn self_generation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(job_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let key = authenticate_downstream(&headers, &state).await?;
    Ok(Json(state.db.generation_job(key.key_id, job_id).await?))
}

pub(in crate::api) async fn self_generation_asset(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((job_id, asset_id)): Path<(Uuid, Uuid)>,
) -> Result<Response, AppError> {
    let key = authenticate_downstream(&headers, &state).await?;
    let asset = state
        .db
        .generation_asset_for_key(key.key_id, job_id, asset_id)
        .await?;
    generation_asset_response(&state, &headers, asset).await
}

pub(in crate::api) async fn self_request_asset(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((request_id, asset_id)): Path<(Uuid, Uuid)>,
) -> Result<Response, AppError> {
    let key = authenticate_downstream(&headers, &state).await?;
    let asset = state
        .db
        .synchronous_generation_asset_for_key(key.key_id, request_id, asset_id)
        .await?;
    generation_asset_response(&state, &headers, asset).await
}

pub(in crate::api) async fn generation_asset_response(
    state: &AppState,
    headers: &HeaderMap,
    asset: crate::model::GenerationAssetDownload,
) -> Result<Response, AppError> {
    let declared_size = u64::try_from(asset.view.size_bytes).map_err(|_| AppError::Internal)?;
    let range_header = match single_byte_range_header(headers) {
        Ok(value) => value,
        Err(()) => return Ok(range_not_satisfiable(declared_size)),
    };
    let requested_range = match parse_byte_range(range_header, declared_size) {
        Ok(range) => range,
        Err(()) => return Ok(range_not_satisfiable(declared_size)),
    };
    let actual_size = state.archive.head_size(&asset.object_locator).await?;
    if actual_size != declared_size {
        tracing::error!(
            asset_id = %asset.view.asset_id,
            declared_size,
            actual_size,
            "generation asset archive size mismatch"
        );
        return Err(AppError::Storage(
            "generation asset archive size mismatch".to_owned(),
        ));
    }
    let download = state
        .archive
        .open_stream(&asset.object_locator, requested_range.clone())
        .await?;
    if download.object_size != actual_size {
        return Err(AppError::Storage(
            "generation asset archive changed during download".to_owned(),
        ));
    }
    let expected_range = requested_range.clone().unwrap_or(0..declared_size);
    if download.range != expected_range {
        return Err(AppError::Storage(
            "generation asset archive returned an unexpected range".to_owned(),
        ));
    }
    let content_length = download.range.end.saturating_sub(download.range.start);
    let mut response = Response::builder()
        .status(if requested_range.is_some() {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        })
        .header(
            header::CONTENT_TYPE,
            safe_download_mime(&asset.view.mime_type),
        )
        .header(header::CONTENT_LENGTH, content_length)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CACHE_CONTROL, "private, no-store")
        .header(
            header::CONTENT_DISPOSITION,
            format!(
                "attachment; filename=\"{}\"",
                safe_download_filename(&asset.view.filename, asset.view.index)
            ),
        );
    if requested_range.is_some() {
        response = response.header(
            header::CONTENT_RANGE,
            format!(
                "bytes {}-{}/{}",
                download.range.start,
                download.range.end.saturating_sub(1),
                actual_size
            ),
        );
    }
    response
        .body(Body::from_stream(download.stream))
        .map_err(|_| AppError::Internal)
}

pub(in crate::api) fn parse_byte_range(
    value: Option<&str>,
    size: u64,
) -> Result<Option<std::ops::Range<u64>>, ()> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.strip_prefix("bytes=").ok_or(())?;
    if value.contains(',') || value.is_empty() || size == 0 {
        return Err(());
    }
    let (start, end) = value.split_once('-').ok_or(())?;
    if start.is_empty() {
        let suffix = end.parse::<u64>().map_err(|_| ())?;
        if suffix == 0 {
            return Err(());
        }
        let length = suffix.min(size);
        return Ok(Some(size - length..size));
    }
    let start = start.parse::<u64>().map_err(|_| ())?;
    if start >= size {
        return Err(());
    }
    let end = if end.is_empty() {
        size - 1
    } else {
        end.parse::<u64>().map_err(|_| ())?.min(size - 1)
    };
    if end < start {
        return Err(());
    }
    Ok(Some(start..end.checked_add(1).ok_or(())?))
}

fn single_byte_range_header(headers: &HeaderMap) -> Result<Option<&str>, ()> {
    let mut values = headers.get_all(header::RANGE).iter();
    let first = values.next();
    if values.next().is_some() {
        return Err(());
    }
    first
        .map(header::HeaderValue::to_str)
        .transpose()
        .map_err(|_| ())
}

fn range_not_satisfiable(size: u64) -> Response {
    Response::builder()
        .status(StatusCode::RANGE_NOT_SATISFIABLE)
        .header(header::CONTENT_RANGE, format!("bytes */{size}"))
        .header(header::CONTENT_LENGTH, 0)
        .header(header::ACCEPT_RANGES, "bytes")
        .body(Body::empty())
        .expect("static range response is valid")
}

fn safe_download_mime(value: &str) -> &'static str {
    match value {
        "image/png" => "image/png",
        "image/jpeg" => "image/jpeg",
        "image/webp" => "image/webp",
        "image/gif" => "image/gif",
        "video/mp4" => "video/mp4",
        "video/webm" => "video/webm",
        "video/quicktime" => "video/quicktime",
        _ => "application/octet-stream",
    }
}

fn safe_download_filename(value: &str, index: i64) -> String {
    let safe = value
        .chars()
        .take(120)
        .map(|value| {
            if value.is_ascii_alphanumeric() || matches!(value, '.' | '-' | '_') {
                value
            } else {
                '_'
            }
        })
        .collect::<String>();
    if safe.is_empty() || safe == "." || safe == ".." {
        format!("asset-{}.bin", index.max(0))
    } else {
        safe
    }
}

pub(in crate::api) async fn cancel_self_generation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(job_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let key = authenticate_downstream(&headers, &state).await?;
    let current = state.db.generation_job(key.key_id, job_id).await?;
    if current.driver == "http-json"
        && current.upstream_job_id.is_some()
        && !matches!(
            current.status.as_str(),
            "succeeded" | "failed" | "cancelled"
        )
    {
        return Err(AppError::BadRequest(
            "this video provider does not expose confirmed upstream cancellation".into(),
        ));
    }
    Ok(Json(
        state.db.cancel_generation_job(key.key_id, job_id).await?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_header_is_single_and_valid_ascii_or_fails_closed() {
        let mut headers = HeaderMap::new();
        assert_eq!(single_byte_range_header(&headers), Ok(None));

        headers.insert(header::RANGE, HeaderValue::from_static("bytes=1-2"));
        assert_eq!(single_byte_range_header(&headers), Ok(Some("bytes=1-2")));

        headers.append(header::RANGE, HeaderValue::from_static("bytes=3-4"));
        assert_eq!(single_byte_range_header(&headers), Err(()));

        let mut headers = HeaderMap::new();
        headers.insert(
            header::RANGE,
            HeaderValue::from_bytes(b"bytes=\xff").expect("opaque header value"),
        );
        assert_eq!(single_byte_range_header(&headers), Err(()));
    }
}
