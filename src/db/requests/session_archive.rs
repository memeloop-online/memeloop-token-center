use super::super::*;

pub(super) fn archive_proof_digest(domain: &str, fields: &[&str]) -> String {
    let mut digest = Sha256::new();
    digest.update(domain.as_bytes());
    for field in fields {
        digest.update([0]);
        digest.update(field.as_bytes());
    }
    format!("{:x}", digest.finalize())
}

pub(super) fn deterministic_archive_request_id(
    tenant_external_id: &str,
    source: &str,
    external_request_id: &str,
) -> Uuid {
    let digest = Sha256::digest(
        [
            b"memeloop-session-archive-request-v1".as_slice(),
            &[0],
            tenant_external_id.as_bytes(),
            &[0],
            source.as_bytes(),
            &[0],
            external_request_id.as_bytes(),
        ]
        .concat(),
    );
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}
