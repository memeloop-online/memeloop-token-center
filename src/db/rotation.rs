use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::Row;
use uuid::Uuid;

use crate::{
    error::AppError,
    provider::{open_private_json, seal_private_json},
};

const CREDENTIAL_ROTATION_AAD: &str = "memeloop-token-center/credential-rotation-response/v1";
pub(super) const CREDENTIAL_ROTATION_REPLAY_TTL_MILLIS: i64 = 24 * 60 * 60 * 1_000;

pub(super) struct RotationReplay {
    pub(super) response_ciphertext: Option<String>,
    pub(super) expires_at: i64,
}

pub(super) fn credential_rotation_request_hash(resource_kind: &str, resource_id: Uuid) -> String {
    let canonical = format!(
        "memeloop-token-center/credential-rotation-request/v1\0{resource_kind}\0{resource_id}"
    );
    format!("{:x}", Sha256::digest(canonical.as_bytes()))
}

fn credential_rotation_aad(
    resource_kind: &str,
    resource_id: Uuid,
    idempotency_key: &str,
    request_hash: &str,
    expires_at: i64,
) -> Vec<u8> {
    format!(
        "{CREDENTIAL_ROTATION_AAD}\0{resource_kind}\0{resource_id}\0{idempotency_key}\0{request_hash}\0{expires_at}"
    )
    .into_bytes()
}

pub(super) async fn claim_credential_rotation(
    transaction: &mut sqlx::Transaction<'_, sqlx::Any>,
    resource_kind: &str,
    resource_id: Uuid,
    idempotency_key: &str,
    request_hash: &str,
    now: i64,
    expires_at: i64,
) -> Result<Option<RotationReplay>, AppError> {
    let claimed = sqlx::query(
        "INSERT INTO credential_rotation_replays (idempotency_key, resource_kind, resource_id, request_hash, response_ciphertext, expires_at, created_at) VALUES ($1, $2, $3, $4, NULL, $5, $6) ON CONFLICT(idempotency_key) DO NOTHING",
    )
    .bind(idempotency_key)
    .bind(resource_kind)
    .bind(resource_id.to_string())
    .bind(request_hash)
    .bind(expires_at)
    .bind(now)
    .execute(&mut **transaction)
    .await?;
    if claimed.rows_affected() == 1 {
        return Ok(None);
    }

    let row = sqlx::query(
        "SELECT resource_kind, resource_id, request_hash, response_ciphertext, expires_at FROM credential_rotation_replays WHERE idempotency_key = $1",
    )
    .bind(idempotency_key)
    .fetch_one(&mut **transaction)
    .await?;
    let existing_kind: String = row.try_get("resource_kind")?;
    let existing_id: String = row.try_get("resource_id")?;
    let existing_hash: String = row.try_get("request_hash")?;
    if existing_kind != resource_kind
        || existing_id != resource_id.to_string()
        || existing_hash != request_hash
    {
        return Err(AppError::BadRequest(
            "Idempotency-Key was already used for a different credential rotation".into(),
        ));
    }
    Ok(Some(RotationReplay {
        response_ciphertext: row.try_get("response_ciphertext")?,
        expires_at: row.try_get("expires_at")?,
    }))
}

pub(super) fn open_rotation_replay<T: for<'de> Deserialize<'de>>(
    replay: RotationReplay,
    resource_kind: &str,
    resource_id: Uuid,
    idempotency_key: &str,
    request_hash: &str,
    pepper: &[u8],
    now: i64,
) -> Result<T, AppError> {
    if replay.expires_at <= now {
        return Err(AppError::BadRequest(
            "idempotent credential rotation response is no longer available; rotate with a new Idempotency-Key"
                .into(),
        ));
    }
    let ciphertext = replay.response_ciphertext.ok_or_else(|| {
        AppError::BadRequest(
            "idempotent credential rotation response is no longer available; rotate with a new Idempotency-Key"
                .into(),
        )
    })?;
    let aad = credential_rotation_aad(
        resource_kind,
        resource_id,
        idempotency_key,
        request_hash,
        replay.expires_at,
    );
    open_private_json(&ciphertext, pepper, &aad)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn store_credential_rotation_response<T: Serialize>(
    transaction: &mut sqlx::Transaction<'_, sqlx::Any>,
    idempotency_key: &str,
    response: &T,
    resource_kind: &str,
    resource_id: Uuid,
    request_hash: &str,
    expires_at: i64,
    pepper: &[u8],
) -> Result<(), AppError> {
    let aad = credential_rotation_aad(
        resource_kind,
        resource_id,
        idempotency_key,
        request_hash,
        expires_at,
    );
    let ciphertext = seal_private_json(response, pepper, &aad)?;
    let stored = sqlx::query(
        "UPDATE credential_rotation_replays SET response_ciphertext = $1 WHERE idempotency_key = $2 AND resource_kind = $3 AND resource_id = $4 AND request_hash = $5 AND response_ciphertext IS NULL",
    )
    .bind(ciphertext)
    .bind(idempotency_key)
    .bind(resource_kind)
    .bind(resource_id.to_string())
    .bind(request_hash)
    .execute(&mut **transaction)
    .await?;
    if stored.rows_affected() != 1 {
        return Err(AppError::Internal);
    }
    Ok(())
}
