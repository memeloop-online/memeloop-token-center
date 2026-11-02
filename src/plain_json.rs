//! Plain (unencrypted) storage helpers.
//!
//! The gateway persists operator state so a restart never loses an in-flight
//! login or an idempotent response. That state used to be sealed into an
//! authenticated envelope, which made incident response harder than the
//! problem it solved: the values are either short-lived handles or operator
//! configuration, and both are far easier to operate when they can be read
//! directly. Nothing here encrypts, decrypts, signs, or authenticates; the
//! two helpers only turn a Rust value into text and back.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Serialize, de::DeserializeOwned};

use crate::error::AppError;

/// Marks an opaque client handle so a stale sealed value from an older build
/// fails with a clear error instead of a confusing JSON parse failure.
const HANDLE_PREFIX: &str = "j1.";

/// Serialises a value for a `TEXT` column as plain JSON.
pub(crate) fn encode_json<T: Serialize>(value: &T) -> Result<String, AppError> {
    serde_json::to_string(value).map_err(|_| AppError::internal("plain_json_encode_1"))
}

/// Reads a value written by [`encode_json`].
pub(crate) fn decode_json<T: DeserializeOwned>(stored: &str) -> Result<T, AppError> {
    serde_json::from_str(stored).map_err(|_| AppError::internal("plain_json_decode_1"))
}

/// Encodes an opaque handle handed to a client. These travel through URLs (the
/// OAuth `state` parameter, device-flow session tokens), so the payload is
/// base64url rather than raw JSON. This is encoding, not protection: the
/// handle is readable by whoever holds it.
pub(crate) fn encode_handle<T: Serialize>(value: &T) -> Result<String, AppError> {
    let bytes =
        serde_json::to_vec(value).map_err(|_| AppError::internal("plain_json_encode_handle_1"))?;
    Ok(format!("{HANDLE_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes)))
}

/// Reads a handle written by [`encode_handle`].
pub(crate) fn decode_handle<T: DeserializeOwned>(handle: &str) -> Result<T, AppError> {
    let encoded = handle
        .strip_prefix(HANDLE_PREFIX)
        .ok_or_else(|| AppError::internal("plain_json_decode_handle_1"))?;
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| AppError::internal("plain_json_decode_handle_2"))?;
    serde_json::from_slice(&bytes).map_err(|_| AppError::internal("plain_json_decode_handle_3"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Deserialize, serde::Serialize, PartialEq, Debug)]
    struct Sample {
        name: String,
        count: u32,
    }

    fn sample() -> Sample {
        Sample {
            name: "proxy".into(),
            count: 2,
        }
    }

    #[test]
    fn stored_values_are_readable_json() {
        let encoded = encode_json(&sample()).unwrap();
        assert_eq!(encoded, r#"{"name":"proxy","count":2}"#);
        assert_eq!(decode_json::<Sample>(&encoded).unwrap(), sample());
    }

    #[test]
    fn handles_are_url_safe_and_round_trip() {
        let handle = encode_handle(&sample()).unwrap();
        assert!(handle.starts_with("j1."));
        assert!(!handle.contains('"') && !handle.contains('{'));
        assert_eq!(decode_handle::<Sample>(&handle).unwrap(), sample());
    }

    #[test]
    fn legacy_sealed_handle_is_rejected() {
        assert!(decode_handle::<Sample>("v2.abcd.efgh").is_err());
    }
}
