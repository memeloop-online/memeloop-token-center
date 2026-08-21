use super::*;

pub(super) async fn operator_index() -> Response {
    web_index().await
}

pub(super) async fn portal_index() -> Response {
    web_index().await
}

fn web_root() -> PathBuf {
    std::env::var_os("MTC_WEB_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/usr/share/memeloop-token-center/web"))
}

async fn web_index() -> Response {
    let mut response = match tokio::fs::read(web_root().join("index.html")).await {
        Ok(body) => ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], body).into_response(),
        Err(error) => {
            tracing::error!(%error, "built web application is unavailable");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "web assets are not installed",
            )
                .into_response()
        }
    };
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; script-src 'self' 'sha256-XON9Vo1xKu4g0Ro9kQujwC0clU/XLRu/4dTJ6h2ZH0c='; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; connect-src 'self'; font-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'; object-src 'none'",
        ),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

pub(super) async fn web_asset(Path(path): Path<String>) -> Response {
    let relative = std::path::Path::new(&path);
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    let content_type = mime_guess::from_path(&path).first_or_octet_stream();
    match tokio::fs::read(web_root().join(path)).await {
        Ok(body) => (
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_str(content_type.as_ref())
                    .unwrap_or(HeaderValue::from_static("application/octet-stream")),
            )],
            body,
        )
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}
