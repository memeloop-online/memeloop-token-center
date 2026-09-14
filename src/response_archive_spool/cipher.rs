use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};

use crate::{
    db::ArchiveSpoolIdentity,
    error::AppError,
    provider::{open_private_json, seal_private_json, seal_private_json_with_nonce},
};

const COMPRESSED_PREFIX: &str = "zstd1.";
const ZSTD_LEVEL: i32 = 1;
const MAX_CIPHERTEXT_BYTES: usize = 256 * 1024;

#[derive(Serialize, Deserialize)]
struct Envelope {
    bytes: String,
}

fn aad(
    identity: ArchiveSpoolIdentity,
    seq: i64,
    purpose: super::BufferedArchivePurpose,
    format_version: u8,
) -> String {
    format!(
        "memeloop-token-center/{}-archive-spool/v{format_version}/{}/{}/{}/{}",
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
    seal_with_compression(identity, seq, bytes, pepper, false)
}

pub(super) fn seal_with_compression(
    identity: ArchiveSpoolIdentity,
    seq: i64,
    bytes: &[u8],
    pepper: &[u8],
    compression_enabled: bool,
) -> Result<String, AppError> {
    seal_for_purpose_with_compression(
        identity,
        seq,
        bytes,
        pepper,
        super::BufferedArchivePurpose::Response,
        compression_enabled,
    )
}

pub(super) fn seal_for_purpose(
    identity: ArchiveSpoolIdentity,
    seq: i64,
    bytes: &[u8],
    pepper: &[u8],
    purpose: super::BufferedArchivePurpose,
) -> Result<String, AppError> {
    seal_for_purpose_with_compression(identity, seq, bytes, pepper, purpose, false)
}

pub(super) fn seal_for_purpose_with_compression(
    identity: ArchiveSpoolIdentity,
    seq: i64,
    bytes: &[u8],
    pepper: &[u8],
    purpose: super::BufferedArchivePurpose,
    compression_enabled: bool,
) -> Result<String, AppError> {
    if seq < 0 || bytes.is_empty() || bytes.len() > super::CHUNK_BYTES {
        return Err(AppError::Internal);
    }
    let (envelope, compressed) = prepare_envelope(bytes, compression_enabled)?;
    let aad = aad(identity, seq, purpose, if compressed { 2 } else { 1 });
    let ciphertext = seal_private_json(&envelope, pepper, aad.as_bytes())?;
    Ok(if compressed {
        format!("{COMPRESSED_PREFIX}{ciphertext}")
    } else {
        ciphertext
    })
}

pub(super) fn seal_for_purpose_with_nonce(
    identity: ArchiveSpoolIdentity,
    seq: i64,
    bytes: &[u8],
    pepper: &[u8],
    purpose: super::BufferedArchivePurpose,
    nonce: [u8; 12],
) -> Result<String, AppError> {
    seal_for_purpose_with_nonce_and_compression(identity, seq, bytes, pepper, purpose, nonce, false)
}

pub(super) fn seal_for_purpose_with_nonce_and_compression(
    identity: ArchiveSpoolIdentity,
    seq: i64,
    bytes: &[u8],
    pepper: &[u8],
    purpose: super::BufferedArchivePurpose,
    nonce: [u8; 12],
    compression_enabled: bool,
) -> Result<String, AppError> {
    if seq < 0 || bytes.is_empty() || bytes.len() > super::CHUNK_BYTES {
        return Err(AppError::Internal);
    }
    let (envelope, compressed) = prepare_envelope(bytes, compression_enabled)?;
    let aad = aad(identity, seq, purpose, if compressed { 2 } else { 1 });
    let ciphertext = seal_private_json_with_nonce(&envelope, pepper, aad.as_bytes(), nonce)?;
    Ok(if compressed {
        format!("{COMPRESSED_PREFIX}{ciphertext}")
    } else {
        ciphertext
    })
}

fn prepare_envelope(bytes: &[u8], compression_enabled: bool) -> Result<(Envelope, bool), AppError> {
    if compression_enabled {
        let compressed = zstd::bulk::compress(bytes, ZSTD_LEVEL).map_err(|_| AppError::Internal)?;
        let compressed_len = sealed_envelope_len(compressed.len(), COMPRESSED_PREFIX.len())
            .ok_or(AppError::Internal)?;
        let raw_len = sealed_len(bytes.len()).ok_or(AppError::Internal)?;
        if compressed_len < raw_len {
            return Ok((
                Envelope {
                    bytes: URL_SAFE_NO_PAD.encode(compressed),
                },
                true,
            ));
        }
    }
    Ok((
        Envelope {
            bytes: URL_SAFE_NO_PAD.encode(bytes),
        },
        false,
    ))
}

pub(super) fn sealed_len(byte_count: usize) -> Option<usize> {
    if !(1..=super::CHUNK_BYTES).contains(&byte_count) {
        return None;
    }
    sealed_envelope_len(byte_count, 0)
}

fn sealed_envelope_len(byte_count: usize, external_prefix_len: usize) -> Option<usize> {
    // {"bytes":"<base64>"}, the Poly1305 tag, and the v2 nonce envelope.
    let json_len = 12_usize.checked_add(base64_len(byte_count)?)?;
    let encrypted_len = json_len.checked_add(16)?;
    external_prefix_len
        .checked_add(20)?
        .checked_add(base64_len(encrypted_len)?)
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
    if ciphertext.len() > MAX_CIPHERTEXT_BYTES
        || !(1..=super::CHUNK_BYTES as i64).contains(&expected_bytes)
    {
        return Err(AppError::Internal);
    }
    let (sealed, compressed, format_version) = match ciphertext.strip_prefix(COMPRESSED_PREFIX) {
        Some(sealed) => (sealed, true, 2),
        None => (ciphertext, false, 1),
    };
    if !sealed.starts_with("v2.") {
        return Err(AppError::Internal);
    }
    let value: Envelope = open_private_json(
        sealed,
        pepper,
        aad(identity, seq, purpose, format_version).as_bytes(),
    )?;
    let maximum_encoded = base64_len(super::CHUNK_BYTES).ok_or(AppError::Internal)?;
    if value.bytes.len() > maximum_encoded {
        return Err(AppError::Internal);
    }
    let payload = URL_SAFE_NO_PAD
        .decode(value.bytes)
        .map_err(|_| AppError::Internal)?;
    let bytes = if compressed {
        zstd::bulk::decompress(
            &payload,
            usize::try_from(expected_bytes).map_err(|_| AppError::Internal)?,
        )
        .map_err(|_| AppError::Internal)?
    } else {
        payload
    };
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

    #[test]
    fn compressed_chunks_are_dual_read_and_smaller_only() {
        let sample = serde_json::to_vec(&vec![
            serde_json::json!({
                "id": "synthetic-response",
                "role": "assistant",
                "content": "ordinary repeated JSON content for deterministic CI coverage",
                "finish_reason": null,
            });
            320
        ])
        .unwrap();
        assert!(sample.len() <= super::super::CHUNK_BYTES);
        let compressed = seal_for_purpose_with_nonce_and_compression(
            identity(),
            0,
            &sample,
            PEPPER,
            super::super::BufferedArchivePurpose::Response,
            [7; 12],
            true,
        )
        .unwrap();
        let legacy = seal_for_purpose_with_nonce(
            identity(),
            0,
            &sample,
            PEPPER,
            super::super::BufferedArchivePurpose::Response,
            [7; 12],
        )
        .unwrap();
        assert!(compressed.starts_with(COMPRESSED_PREFIX));
        assert!(compressed.len() < legacy.len());
        assert_eq!(
            open_for_purpose(
                identity(),
                0,
                &compressed,
                sample.len() as i64,
                PEPPER,
                super::super::BufferedArchivePurpose::Response,
            )
            .unwrap()
            .as_ref(),
            sample.as_slice()
        );
        assert_eq!(
            open_for_purpose(
                identity(),
                0,
                &legacy,
                sample.len() as i64,
                PEPPER,
                super::super::BufferedArchivePurpose::Response,
            )
            .unwrap()
            .as_ref(),
            sample.as_slice()
        );
        assert!(
            open_for_purpose(
                identity(),
                0,
                &compressed,
                sample.len() as i64 - 1,
                PEPPER,
                super::super::BufferedArchivePurpose::Response,
            )
            .is_err(),
            "the database byte_count remains the exact decompression bound"
        );
    }

    #[test]
    fn compression_falls_back_to_the_exact_legacy_format_when_not_shorter() {
        let bytes = b"x";
        let enabled = seal_for_purpose_with_nonce_and_compression(
            identity(),
            0,
            bytes,
            PEPPER,
            super::super::BufferedArchivePurpose::Response,
            [7; 12],
            true,
        )
        .unwrap();
        let legacy = seal_for_purpose_with_nonce(
            identity(),
            0,
            bytes,
            PEPPER,
            super::super::BufferedArchivePurpose::Response,
            [7; 12],
        )
        .unwrap();
        assert_eq!(enabled, legacy);
        assert!(!enabled.starts_with(COMPRESSED_PREFIX));
    }

    #[test]
    fn compressed_fixed_nonce_replay_is_byte_identical_and_format_bound() {
        let bytes = vec![b'a'; super::super::CHUNK_BYTES];
        let seal = || {
            seal_for_purpose_with_nonce_and_compression(
                identity(),
                9,
                &bytes,
                PEPPER,
                super::super::BufferedArchivePurpose::Request,
                [11; 12],
                true,
            )
            .unwrap()
        };
        let first = seal();
        assert_eq!(first, seal());
        let inner = first.strip_prefix(COMPRESSED_PREFIX).unwrap();
        assert!(
            open_private_json::<Envelope>(
                inner,
                PEPPER,
                aad(
                    identity(),
                    9,
                    super::super::BufferedArchivePurpose::Request,
                    1,
                )
                .as_bytes(),
            )
            .is_err(),
            "the compressed marker must select an independently authenticated AAD"
        );
    }

    #[test]
    fn compressed_open_rejects_output_over_the_original_chunk_bound() {
        let expanded = vec![b'b'; super::super::CHUNK_BYTES + 1];
        let compressed = zstd::bulk::compress(&expanded, ZSTD_LEVEL).unwrap();
        let sealed = seal_private_json_with_nonce(
            &Envelope {
                bytes: URL_SAFE_NO_PAD.encode(compressed),
            },
            PEPPER,
            aad(
                identity(),
                0,
                super::super::BufferedArchivePurpose::Response,
                2,
            )
            .as_bytes(),
            [13; 12],
        )
        .unwrap();
        let ciphertext = format!("{COMPRESSED_PREFIX}{sealed}");
        assert!(
            open_for_purpose(
                identity(),
                0,
                &ciphertext,
                super::super::CHUNK_BYTES as i64,
                PEPPER,
                super::super::BufferedArchivePurpose::Response,
            )
            .is_err()
        );
        assert!(
            open_for_purpose(
                identity(),
                0,
                &ciphertext,
                super::super::CHUNK_BYTES as i64 + 1,
                PEPPER,
                super::super::BufferedArchivePurpose::Response,
            )
            .is_err()
        );
    }
}
