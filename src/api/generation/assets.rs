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
    match asset.source {
        crate::model::GenerationAssetSource::Archive { object_locator } => {
            archived_generation_asset_response(state, headers, asset.view, &object_locator).await
        }
        crate::model::GenerationAssetSource::Provider {
            owner,
            url,
            expires_at,
        } => {
            provider_generation_asset_response(state, headers, asset.view, owner, &url, expires_at)
                .await
        }
    }
}

async fn archived_generation_asset_response(
    state: &AppState,
    headers: &HeaderMap,
    view: crate::model::GenerationAssetView,
    object_locator: &str,
) -> Result<Response, AppError> {
    let declared_size = u64::try_from(view.size_bytes).map_err(|_| AppError::Internal)?;
    let range_header = match single_byte_range_header(headers) {
        Ok(value) => value,
        Err(()) => return Ok(range_not_satisfiable(declared_size)),
    };
    let requested_range = match parse_byte_range(range_header, declared_size) {
        Ok(range) => range,
        Err(()) => return Ok(range_not_satisfiable(declared_size)),
    };
    let actual_size = state.archive.head_size(object_locator).await?;
    if actual_size != declared_size {
        tracing::error!(
            asset_id = %view.asset_id,
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
        .open_stream(object_locator, requested_range.clone())
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
        .header(header::CONTENT_TYPE, safe_download_mime(&view.mime_type))
        .header(header::CONTENT_LENGTH, content_length)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CACHE_CONTROL, "private, no-store")
        .header(
            header::CONTENT_DISPOSITION,
            format!(
                "attachment; filename=\"{}\"",
                safe_download_filename(&view.filename, view.index)
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

async fn provider_generation_asset_response(
    state: &AppState,
    headers: &HeaderMap,
    view: crate::model::GenerationAssetView,
    owner: crate::model::GenerationAssetProviderOwner,
    url: &str,
    expires_at: Option<i64>,
) -> Result<Response, AppError> {
    if expires_at.is_some_and(|expires_at| expires_at <= unix_millis()) {
        return Ok(provider_asset_unavailable());
    }
    let route = match owner {
        crate::model::GenerationAssetProviderOwner::Job(job_id) => {
            state
                .db
                .load_generation_asset_upstream(job_id, state.config.key_pepper.as_bytes())
                .await?
        }
        crate::model::GenerationAssetProviderOwner::Request(request_id) => {
            state
                .db
                .load_synchronous_asset_upstream(request_id, state.config.key_pepper.as_bytes())
                .await?
        }
    }
    .ok_or_else(|| AppError::Upstream("generation asset upstream is unavailable".into()))?;
    crate::generation::ensure_asset_origin(&route, url)?;
    let client = crate::generation::route_http(state, &route, url).await?;
    let mut request = client.get(url);
    let asset_url = url::Url::parse(url)
        .map_err(|_| AppError::Upstream("generation asset URL is invalid".into()))?;
    let base_url = url::Url::parse(&route.base_url).map_err(|_| AppError::Internal)?;
    if asset_url.origin() == base_url.origin() {
        request = route.credential.apply(request, unix_millis())?;
    }
    let range = match single_provider_range_header(headers) {
        Ok(range) => range,
        Err(()) => return Ok(range_not_satisfiable(0)),
    };
    if let Some(range) = range {
        request = request.header(header::RANGE, range);
    }
    let _upstream_activity = state
        .metrics
        .active_upstream(&route.driver, "generation_asset_proxy");
    let started = std::time::Instant::now();
    let response_result = request.send().await;
    state.metrics.observe_upstream(
        &route.driver,
        "generation_asset_proxy",
        response_result.as_ref().ok().map(reqwest::Response::status),
        started.elapsed(),
    );
    let upstream =
        response_result.map_err(|_| AppError::Upstream("generation asset fetch failed".into()))?;
    if matches!(upstream.status(), StatusCode::NOT_FOUND | StatusCode::GONE) {
        return Ok(provider_asset_unavailable());
    }
    if !matches!(
        upstream.status(),
        StatusCode::OK | StatusCode::PARTIAL_CONTENT
    ) {
        return Err(AppError::Upstream(format!(
            "generation asset fetch returned HTTP {}",
            upstream.status().as_u16()
        )));
    }
    let status = upstream.status();
    if range.is_some() && status != StatusCode::PARTIAL_CONTENT {
        return Err(AppError::Upstream(
            "generation asset provider ignored the requested byte range".into(),
        ));
    }
    let content_type = upstream
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(safe_download_mime)
        .unwrap_or_else(|| safe_download_mime(&view.mime_type));
    let content_length = upstream.headers().get(header::CONTENT_LENGTH).cloned();
    if content_length
        .as_ref()
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        == Some(0)
    {
        return Ok(provider_asset_unavailable());
    }
    let content_range = upstream.headers().get(header::CONTENT_RANGE).cloned();
    // The authenticated proxy accepts and forwards a single byte range even
    // when a provider omits the advisory header on its full-body response.
    // Preserve an explicit upstream value such as `none` when one is present.
    let accept_ranges = upstream
        .headers()
        .get(header::ACCEPT_RANGES)
        .cloned()
        .unwrap_or_else(|| HeaderValue::from_static("bytes"));
    let mut response = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "private, no-store")
        .header(
            header::CONTENT_DISPOSITION,
            format!(
                "attachment; filename=\"{}\"",
                safe_download_filename(&view.filename, view.index)
            ),
        );
    for (name, value) in [
        (header::CONTENT_LENGTH, content_length),
        (header::CONTENT_RANGE, content_range),
        (header::ACCEPT_RANGES, Some(accept_ranges)),
    ] {
        if let Some(value) = value {
            response = response.header(name, value);
        }
    }
    response
        .body(Body::from_stream(upstream.bytes_stream()))
        .map_err(|_| AppError::Internal)
}

fn single_provider_range_header(headers: &HeaderMap) -> Result<Option<&str>, ()> {
    let mut values = headers.get_all(header::RANGE).iter();
    let first = values.next();
    if values.next().is_some() {
        return Err(());
    }
    let Some(value) = first else {
        return Ok(None);
    };
    let value = value.to_str().map_err(|_| ())?;
    if value.len() > 200 || value.contains(',') {
        return Err(());
    }
    let range = value.strip_prefix("bytes=").ok_or(())?;
    let (start, end) = range.split_once('-').ok_or(())?;
    match (start.is_empty(), end.is_empty()) {
        (true, true) => return Err(()),
        (true, false) => {
            if end.parse::<u64>().map_err(|_| ())? == 0 {
                return Err(());
            }
        }
        (false, true) => {
            start.parse::<u64>().map_err(|_| ())?;
        }
        (false, false) => {
            let start = start.parse::<u64>().map_err(|_| ())?;
            let end = end.parse::<u64>().map_err(|_| ())?;
            if end < start {
                return Err(());
            }
        }
    }
    Ok(Some(value))
}

fn provider_asset_unavailable() -> Response {
    let body = Bytes::from_static(
        br#"{"error":{"code":"generation_asset_unavailable","message":"The upstream generation asset has expired or is no longer available."}}"#,
    );
    Response::builder()
        .status(StatusCode::GONE)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CONTENT_LENGTH, body.len())
        .header(header::CACHE_CONTROL, "private, no-store")
        .body(Body::from(body))
        .expect("static provider asset response is valid")
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
        .header(header::CACHE_CONTROL, "private, no-store")
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

    #[test]
    fn provider_range_header_accepts_only_one_well_formed_byte_range() {
        for value in ["bytes=1-2", "bytes=1-", "bytes=-2"] {
            let mut headers = HeaderMap::new();
            headers.insert(header::RANGE, HeaderValue::from_static(value));
            assert_eq!(single_provider_range_header(&headers), Ok(Some(value)));
        }
        for value in [
            "items=1-2",
            "bytes=",
            "bytes=-0",
            "bytes=2-1",
            "bytes=a-2",
            "bytes=1-b",
            "bytes=1-2,4-5",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::RANGE,
                HeaderValue::from_str(value).expect("ASCII range fixture"),
            );
            assert_eq!(single_provider_range_header(&headers), Err(()));
        }
    }
}
