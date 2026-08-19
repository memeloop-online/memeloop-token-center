use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

pub struct IssuedCredential {
    pub credential_id: Uuid,
    pub key_id: Uuid,
    pub secret: String,
    pub fingerprint: String,
    pub secret_hash: Vec<u8>,
}

pub struct ParsedCredential {
    pub key_id: Uuid,
}

pub fn issue_credential(key_id: Uuid, pepper: &[u8]) -> IssuedCredential {
    issue_credential_with_prefix(key_id, pepper, "mtc")
}

pub fn issue_service_credential(service_id: Uuid, pepper: &[u8]) -> IssuedCredential {
    issue_credential_with_prefix(service_id, pepper, "mts")
}

fn issue_credential_with_prefix(key_id: Uuid, pepper: &[u8], prefix: &str) -> IssuedCredential {
    let credential_id = Uuid::now_v7();
    let mut random = [0_u8; 32];
    getrandom::fill(&mut random).expect("operating system random source");
    let material = URL_SAFE_NO_PAD.encode(random);
    let secret = format!("{prefix}_{}_{}", key_id.simple(), material);
    let secret_hash = keyed_hash(pepper, secret.as_bytes());
    let fingerprint = hex_prefix(&secret_hash, 8);

    IssuedCredential {
        credential_id,
        key_id,
        secret,
        fingerprint,
        secret_hash,
    }
}

pub fn parse_credential(value: &str) -> Option<ParsedCredential> {
    parse_credential_with_prefix(value, "mtc")
}

pub fn parse_service_credential(value: &str) -> Option<ParsedCredential> {
    parse_credential_with_prefix(value, "mts")
}

fn parse_credential_with_prefix(value: &str, prefix: &str) -> Option<ParsedCredential> {
    let raw = value.strip_prefix(prefix)?.strip_prefix('_')?;
    let (key_id, secret_material) = raw.split_once('_')?;
    if secret_material.len() < 32 {
        return None;
    }

    Some(ParsedCredential {
        key_id: Uuid::parse_str(key_id).ok()?,
    })
}

pub fn verify_credential(value: &str, pepper: &[u8], expected: &[u8]) -> bool {
    // Delegate tag verification (including the constant-time comparison and
    // wrong-length rejection) to RustCrypto's `Mac` implementation.  This
    // module defines only the Token Center credential format; it does not
    // implement a cryptographic primitive.
    let mut mac = HmacSha256::new_from_slice(pepper).expect("HMAC accepts any key length");
    mac.update(value.as_bytes());
    mac.verify_slice(expected).is_ok()
}

pub fn hash_credential(value: &str, pepper: &[u8]) -> (Vec<u8>, String) {
    let secret_hash = keyed_hash(pepper, value.as_bytes());
    let fingerprint = hex_prefix(&secret_hash, 8);
    (secret_hash, fingerprint)
}

pub fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    left.ct_eq(right).into()
}

/// Signs an HTTP webhook payload using the documented MemeLoop Cloud v1
/// envelope.  RustCrypto performs both HMAC construction and verification;
/// this function only defines the wire-format framing.
pub fn sign_webhook_payload(secret: &[u8], timestamp: &str, payload: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(timestamp.as_bytes());
    mac.update(b".");
    mac.update(payload);
    format!("v1={}", URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes()))
}

pub fn verify_webhook_payload(
    secret: &[u8],
    timestamp: &str,
    payload: &[u8],
    signature: &str,
) -> bool {
    let Some(encoded) = signature.strip_prefix("v1=") else {
        return false;
    };
    let Ok(tag) = URL_SAFE_NO_PAD.decode(encoded) else {
        return false;
    };
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(timestamp.as_bytes());
    mac.update(b".");
    mac.update(payload);
    mac.verify_slice(&tag).is_ok()
}

fn keyed_hash(pepper: &[u8], value: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(pepper).expect("HMAC accepts any key length");
    mac.update(value);
    mac.finalize().into_bytes().to_vec()
}

fn hex_prefix(bytes: &[u8], count: usize) -> String {
    bytes
        .iter()
        .take(count)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issued_key_carries_stable_public_id_and_verifies() {
        let key_id = Uuid::now_v7();
        let issued = issue_credential(key_id, b"a pepper that is long enough for this test");
        let parsed = parse_credential(&issued.secret).expect("valid key");

        assert_eq!(parsed.key_id, key_id);
        assert!(verify_credential(
            &issued.secret,
            b"a pepper that is long enough for this test",
            &issued.secret_hash
        ));
        assert!(!verify_credential(
            "mtc_wrong_material",
            b"a pepper that is long enough for this test",
            &issued.secret_hash
        ));
        assert!(!verify_credential(
            &issued.secret,
            b"a pepper that is long enough for this test",
            &issued.secret_hash[..31]
        ));
    }

    #[test]
    fn webhook_signature_is_framed_and_verified_by_rustcrypto() {
        let secret = b"webhook secret long enough for a real integration";
        let signature = sign_webhook_payload(secret, "1700000000", br#"{"event":"updated"}"#);
        assert!(verify_webhook_payload(
            secret,
            "1700000000",
            br#"{"event":"updated"}"#,
            &signature,
        ));
        assert!(!verify_webhook_payload(
            secret,
            "1700000001",
            br#"{"event":"updated"}"#,
            &signature,
        ));
        assert!(!verify_webhook_payload(
            secret,
            "1700000000",
            br#"{"event":"cancelled"}"#,
            &signature,
        ));
        assert!(!verify_webhook_payload(
            secret,
            "1700000000",
            br#"{"event":"updated"}"#,
            "v1=not-base64!",
        ));
    }
}
