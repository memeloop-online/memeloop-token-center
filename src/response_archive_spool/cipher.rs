use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};

use crate::{
    db::ArchiveSpoolIdentity,
    error::AppError,
    provider::{open_private_json, seal_private_json, seal_private_json_with_nonce},
};

#[derive(Serialize, Deserialize)]
struct Envelope {
    bytes: String,
}

fn aad(identity: ArchiveSpoolIdentity, seq: i64, purpose: super::BufferedArchivePurpose) -> String {
    format!(
        "memeloop-token-center/{}-archive-spool/v1/{}/{}/{}/{}",
        purpose.as_str(),
        identity.tenant_id,
        identity.request_id,
        identity.reservation_id,
        seq
    )
}

pub(super) fn seal(
    identity: ArchiveSpoolIdentity,
    seq: i64,
    bytes: &[u8],
    pepper: &[u8],
) -> Result<String, AppError> {
    seal_for_purpose(
        identity,
        seq,
        bytes,
        pepper,
        super::BufferedArchivePurpose::Response,
    )
}

pub(super) fn seal_for_purpose(
    identity: ArchiveSpoolIdentity,
    seq: i64,
    bytes: &[u8],
    pepper: &[u8],
    purpose: super::BufferedArchivePurpose,
) -> Result<String, AppError> {
    if seq < 0 || bytes.is_empty() || bytes.len() > super::CHUNK_BYTES {
        return Err(AppError::Internal);
    }
    seal_private_json(
        &Envelope {
            bytes: URL_SAFE_NO_PAD.encode(bytes),
        },
        pepper,
        aad(identity, seq, purpose).as_bytes(),
    )
}

pub(super) fn seal_for_purpose_with_nonce(
    identity: ArchiveSpoolIdentity,
    seq: i64,
    bytes: &[u8],
    pepper: &[u8],
    purpose: super::BufferedArchivePurpose,
    nonce: [u8; 12],
) -> Result<String, AppError> {
    if seq < 0 || bytes.is_empty() || bytes.len() > super::CHUNK_BYTES {
        return Err(AppError::Internal);
    }
    let aad = aad(identity, seq, purpose);
    seal_private_json_with_nonce(
        &Envelope {
            bytes: URL_SAFE_NO_PAD.encode(bytes),
        },
        pepper,
        aad.as_bytes(),
        nonce,
    )
}

pub(super) fn sealed_len(byte_count: usize) -> Option<usize> {
    if !(1..=super::CHUNK_BYTES).contains(&byte_count) {
        return None;
    }
    // {"bytes":"<base64>"}, the Poly1305 tag, and the v2 nonce envelope.
    let json_len = 12_usize.checked_add(base64_len(byte_count)?)?;
    let encrypted_len = json_len.checked_add(16)?;
    20_usize.checked_add(base64_len(encrypted_len)?)
}

fn base64_len(bytes: usize) -> Option<usize> {
    (bytes / 3).checked_mul(4)?.checked_add(match bytes % 3 {
        0 => 0,
        1 => 2,
        2 => 3,
        _ => unreachable!(),
    })
}

#[cfg(test)]
pub(super) fn open(
    identity: ArchiveSpoolIdentity,
    seq: i64,
    ciphertext: &str,
    expected_bytes: i64,
    pepper: &[u8],
) -> Result<bytes::Bytes, AppError> {
    open_for_purpose(
        identity,
        seq,
        ciphertext,
        expected_bytes,
        pepper,
        super::BufferedArchivePurpose::Response,
    )
}

pub(super) fn open_for_purpose(
    identity: ArchiveSpoolIdentity,
    seq: i64,
    ciphertext: &str,
    expected_bytes: i64,
    pepper: &[u8],
    purpose: super::BufferedArchivePurpose,
) -> Result<bytes::Bytes, AppError> {
    // Bound input before the shared envelope decoder allocates. Persistence
    // stores only authenticated v2 envelopes, never raw payload or a key.
    if ciphertext.len() > 256 * 1024
        || !ciphertext.starts_with("v2.")
        || !(1..=super::CHUNK_BYTES as i64).contains(&expected_bytes)
    {
        return Err(AppError::Internal);
    }
    let value: Envelope =
        open_private_json(ciphertext, pepper, aad(identity, seq, purpose).as_bytes())?;
    let bytes = URL_SAFE_NO_PAD
        .decode(value.bytes)
        .map_err(|_| AppError::Internal)?;
    if i64::try_from(bytes.len()).ok() != Some(expected_bytes) {
        return Err(AppError::Internal);
    }
    Ok(bytes.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PEPPER: &[u8] = b"archive-spool-test-pepper";

    fn identity() -> ArchiveSpoolIdentity {
        ArchiveSpoolIdentity {
            request_id: uuid::Uuid::nil(),
            tenant_id: uuid::Uuid::nil(),
            reservation_id: uuid::Uuid::nil(),
        }
    }

    #[test]
    fn predicted_ciphertext_length_matches_v2_serialization() {
        for byte_count in [1, 2, 3, 65_535, 65_536] {
            let plaintext = vec![b'x'; byte_count];
            let ciphertext = seal_for_purpose_with_nonce(
                identity(),
                0,
                &plaintext,
                PEPPER,
                super::super::BufferedArchivePurpose::Response,
                [7; 12],
            )
            .unwrap();
            assert_eq!(Some(ciphertext.len()), sealed_len(byte_count));
        }
    }
}
