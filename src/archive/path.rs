use object_store::path::Path;

use crate::error::AppError;

pub(super) fn content_location(hash: &str) -> String {
    format!("objects/blake3/{}/{hash}", &hash[..2])
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
