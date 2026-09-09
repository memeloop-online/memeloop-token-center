use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};

use crate::{
    db::ArchiveSpoolIdentity,
    error::AppError,
    provider::{open_private_json, seal_private_json},
};

#[derive(Serialize, Deserialize)]
struct Envelope {
    bytes: String,
}

fn aad(identity: ArchiveSpoolIdentity, seq: i64) -> String {
    format!(
        "memeloop-token-center/response-archive-spool/v1/{}/{}/{}/{}",
        identity.tenant_id, identity.request_id, identity.reservation_id, seq
    )
}

pub(super) fn seal(
    identity: ArchiveSpoolIdentity,
    seq: i64,
    bytes: &[u8],
    pepper: &[u8],
) -> Result<String, AppError> {
    if seq < 0 || bytes.is_empty() || bytes.len() > super::CHUNK_BYTES {
        return Err(AppError::Internal);
    }
    seal_private_json(
        &Envelope {
            bytes: URL_SAFE_NO_PAD.encode(bytes),
        },
        pepper,
        aad(identity, seq).as_bytes(),
    )
}

pub(super) fn open(
    identity: ArchiveSpoolIdentity,
    seq: i64,
    ciphertext: &str,
    expected_bytes: i64,
    pepper: &[u8],
) -> Result<bytes::Bytes, AppError> {
    // Bound input before the shared envelope decoder allocates. Persistence
    // stores only authenticated v2 envelopes, never raw payload or a key.
    if ciphertext.len() > 256 * 1024
        || !ciphertext.starts_with("v2.")
        || !(1..=super::CHUNK_BYTES as i64).contains(&expected_bytes)
    {
        return Err(AppError::Internal);
    }
    let value: Envelope = open_private_json(ciphertext, pepper, aad(identity, seq).as_bytes())?;
    let bytes = URL_SAFE_NO_PAD
        .decode(value.bytes)
        .map_err(|_| AppError::Internal)?;
    if i64::try_from(bytes.len()).ok() != Some(expected_bytes) {
        return Err(AppError::Internal);
    }
    Ok(bytes.into())
}
