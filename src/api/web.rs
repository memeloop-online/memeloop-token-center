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

pub(super) async fn web_asset(State(state): State<AppState>, Path(path): Path<String>) -> Response {
    if let Some((plugin_id, version, sha256, entry)) = plugin_ui_asset_path(&path) {
        let state = match state.pin_application_plugins().await {
            Ok(state) => state,
            Err(error) => return error.into_response(),
        };
        return match state
            .plugins
            .operator_ui_module(plugin_id, version, sha256, entry)
        {
            Some(body) => (
                [
                    (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
                    (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
                ],
                Bytes::from_owner(body),
            )
                .into_response(),
            None => StatusCode::NOT_FOUND.into_response(),
        };
    }
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

fn plugin_ui_asset_path(path: &str) -> Option<(&str, &str, &str, &str)> {
    let mut parts = path.splitn(5, '/');
    if parts.next()? != "plugins" {
        return None;
    }
    let plugin_id = parts.next()?;
    let version = parts.next()?;
    let sha256 = parts.next()?;
    let entry = parts.next()?;
    if !crate::plugin::safe_plugin_token(plugin_id, 64)
        || version.is_empty()
        || version.len() > 128
        || semver::Version::parse(version).is_err()
        || !sha256.strip_prefix("sha256:").is_some_and(|value| {
            value.len() == 64
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
        || !crate::plugin::safe_operator_ui_module_entry(entry)
    {
        return None;
    }
    Some((plugin_id, version, sha256, entry))
}

#[cfg(test)]
mod tests {
    use super::plugin_ui_asset_path;

    #[test]
    fn plugin_ui_asset_path_rejects_unbounded_or_normalized_identifiers() {
        let digest = "a".repeat(64);
        let valid = format!("plugins/health/1.2.3/sha256:{digest}/assets/operator-ui.mjs");
        assert_eq!(
            plugin_ui_asset_path(&valid),
            Some((
                "health",
                "1.2.3",
                valid.split('/').nth(3).unwrap(),
                "assets/operator-ui.mjs"
            ))
        );
        for path in [
            format!("plugins/INVALID/1.2.3/sha256:{digest}/assets/operator-ui.mjs"),
            format!("plugins/health/latest/sha256:{digest}/assets/operator-ui.mjs"),
            format!("plugins/health/1.2.3/sha256:{digest}/assets//operator-ui.mjs"),
            format!("plugins/health/1.2.3/sha256:{digest}/assets/../operator-ui.mjs"),
        ] {
            assert!(plugin_ui_asset_path(&path).is_none(), "accepted {path}");
        }
    }
}
