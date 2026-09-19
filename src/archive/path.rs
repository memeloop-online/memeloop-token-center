use object_store::path::Path;

use crate::error::AppError;
use uuid::Uuid;

pub(super) fn content_location(hash: &str) -> String {
    format!("objects/blake3/{}/{hash}", &hash[..2])
}

pub(super) fn tenant_cas_location(
    tenant_id: Uuid,
    digest: &str,
    compressed: bool,
) -> Result<String, AppError> {
    if !is_lower_hex_digest(digest) {
        return Err(AppError::Storage(
            "archive content digest is invalid".into(),
        ));
    }
    Ok(format!(
        "tenants/{tenant_id}/cas/v1/blake3/{}/{}{}",
        &digest[..2],
        digest,
        if compressed {
            super::compressed::SUFFIX
        } else {
            ""
        }
    ))
}

pub(crate) fn is_tenant_cas_location(tenant_id: Uuid, location: &str) -> bool {
    let prefix = format!("tenants/{tenant_id}/cas/v1/blake3/");
    let Some(rest) = location.strip_prefix(&prefix) else {
        return false;
    };
    let Some((shard, object)) = rest.split_once('/') else {
        return false;
    };
    if object.contains('/') {
        return false;
    }
    let digest = object
        .strip_suffix(super::compressed::SUFFIX)
        .unwrap_or(object);
    is_lower_hex_digest(digest) && shard.len() == 2 && digest.starts_with(shard)
}

fn is_lower_hex_digest(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

pub(super) fn is_any_v1_cas_location(location: &str) -> bool {
    let mut segments = location.split('/');
    matches!(
        (
            segments.next(),
            segments.next(),
            segments.next(),
            segments.next(),
            segments.next(),
            segments.next(),
            segments.next(),
            segments.next(),
        ),
        (
            Some("tenants"),
            Some(_),
            Some("cas"),
            Some("v1"),
            Some("blake3"),
            Some(_),
            Some(_),
            None
        )
    )
}

pub(super) fn archive_path(location: &str) -> Result<Path, AppError> {
    // Object locations are internal identifiers, not filesystem paths or URLs. Keeping
    // their alphabet deliberately small gives every backend (especially the local test
    // backend) the same traversal and separator semantics.
    let has_only_safe_bytes = location
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-'));
    if location.is_empty()
        || location.starts_with('/')
        || location.ends_with('/')
        || !has_only_safe_bytes
    {
        return Err(AppError::BadRequest(
            "invalid archive object location".to_owned(),
        ));
    }

    Path::parse(location)
        .map_err(|_| AppError::BadRequest("invalid archive object location".to_owned()))
}
