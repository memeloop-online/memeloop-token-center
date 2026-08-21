use sqlx::{Any, AnyPool, Row, Transaction};
use uuid::Uuid;

use crate::error::AppError;

const CREDENTIAL_SUBJECT: &str = "credential";
const ROUTE_SUBJECT: &str = "route";

/// Serializes routing-grant replacement transactions for one tenant.
///
/// Both the credential-facing and route-facing editors replace complete edge
/// sets.  Taking the same short-lived tenant row lock before discovering old
/// reverse edges keeps those sets stable on PostgreSQL and SQLite alike.
pub(crate) async fn lock_routing_relation_writes(
    tx: &mut Transaction<'_, Any>,
    tenant_id: &str,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO routing_relation_write_locks (tenant_id, generation) \
         SELECT id, 0 FROM tenants WHERE id = $1 \
         ON CONFLICT (tenant_id) DO NOTHING",
    )
    .bind(tenant_id)
    .execute(&mut **tx)
    .await?;
    let locked = sqlx::query(
        "UPDATE routing_relation_write_locks \
         SET generation = CASE WHEN generation = 9223372036854775807 THEN 0 ELSE generation + 1 END \
         WHERE tenant_id = $1",
    )
    .bind(tenant_id)
    .execute(&mut **tx)
    .await?;
    if locked.rows_affected() != 1 {
        return Err(AppError::NotFound);
    }
    Ok(())
}

pub(crate) async fn credential_grant_revision(
    pool: &AnyPool,
    tenant_id: &str,
    key_id: Uuid,
) -> Result<i64, AppError> {
    relation_revision(pool, tenant_id, CREDENTIAL_SUBJECT, key_id).await
}

pub(crate) async fn route_grant_revision(
    pool: &AnyPool,
    tenant_id: &str,
    route_id: Uuid,
) -> Result<i64, AppError> {
    relation_revision(pool, tenant_id, ROUTE_SUBJECT, route_id).await
}

pub(crate) async fn compare_and_bump_credential_grant_revision(
    tx: &mut Transaction<'_, Any>,
    tenant_id: &str,
    key_id: Uuid,
    expected_revision: i64,
    should_bump: bool,
) -> Result<i64, AppError> {
    compare_and_bump_relation_revision(
        tx,
        tenant_id,
        CREDENTIAL_SUBJECT,
        key_id,
        expected_revision,
        should_bump,
    )
    .await
}

pub(crate) async fn compare_and_bump_route_grant_revision(
    tx: &mut Transaction<'_, Any>,
    tenant_id: &str,
    route_id: Uuid,
    expected_revision: i64,
    should_bump: bool,
) -> Result<i64, AppError> {
    compare_and_bump_relation_revision(
        tx,
        tenant_id,
        ROUTE_SUBJECT,
        route_id,
        expected_revision,
        should_bump,
    )
    .await
}

pub(crate) async fn bump_credential_grant_revisions(
    tx: &mut Transaction<'_, Any>,
    tenant_id: &str,
    key_ids: &[Uuid],
) -> Result<(), AppError> {
    bump_relation_revisions(tx, tenant_id, CREDENTIAL_SUBJECT, key_ids).await
}

pub(crate) async fn bump_route_grant_revisions(
    tx: &mut Transaction<'_, Any>,
    tenant_id: &str,
    route_ids: &[Uuid],
) -> Result<(), AppError> {
    bump_relation_revisions(tx, tenant_id, ROUTE_SUBJECT, route_ids).await
}

async fn relation_revision(
    pool: &AnyPool,
    tenant_id: &str,
    subject_kind: &str,
    subject_id: Uuid,
) -> Result<i64, AppError> {
    let revision = sqlx::query(
        "SELECT revision FROM routing_grant_relation_revisions \
         WHERE tenant_id = $1 AND subject_kind = $2 AND subject_id = $3",
    )
    .bind(tenant_id)
    .bind(subject_kind)
    .bind(subject_id.to_string())
    .fetch_optional(pool)
    .await?
    .map(|row| row.try_get("revision"))
    .transpose()?
    .unwrap_or(0);
    Ok(revision)
}

async fn compare_and_bump_relation_revision(
    tx: &mut Transaction<'_, Any>,
    tenant_id: &str,
    subject_kind: &str,
    subject_id: Uuid,
    expected_revision: i64,
    should_bump: bool,
) -> Result<i64, AppError> {
    if expected_revision < 0 {
        return Err(AppError::BadRequest(
            "expected_grant_revision must be non-negative".into(),
        ));
    }
    ensure_relation_revision(tx, tenant_id, subject_kind, subject_id).await?;
    if !should_bump {
        let current: i64 = sqlx::query(
            "SELECT revision FROM routing_grant_relation_revisions \
             WHERE tenant_id = $1 AND subject_kind = $2 AND subject_id = $3",
        )
        .bind(tenant_id)
        .bind(subject_kind)
        .bind(subject_id.to_string())
        .fetch_one(&mut **tx)
        .await?
        .try_get("revision")?;
        if current != expected_revision {
            return Err(AppError::Conflict(
                "routing grants changed; reload before saving".into(),
            ));
        }
        return Ok(current);
    }
    let updated = sqlx::query(
        "UPDATE routing_grant_relation_revisions SET revision = revision + 1 \
         WHERE tenant_id = $1 AND subject_kind = $2 AND subject_id = $3 \
           AND revision = $4 AND revision < 9223372036854775807",
    )
    .bind(tenant_id)
    .bind(subject_kind)
    .bind(subject_id.to_string())
    .bind(expected_revision)
    .execute(&mut **tx)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(AppError::Conflict(
            "routing grants changed; reload before saving".into(),
        ));
    }
    Ok(expected_revision + 1)
}

async fn bump_relation_revisions(
    tx: &mut Transaction<'_, Any>,
    tenant_id: &str,
    subject_kind: &str,
    subject_ids: &[Uuid],
) -> Result<(), AppError> {
    for subject_id in subject_ids {
        ensure_relation_revision(tx, tenant_id, subject_kind, *subject_id).await?;
        let updated = sqlx::query(
            "UPDATE routing_grant_relation_revisions SET revision = revision + 1 \
             WHERE tenant_id = $1 AND subject_kind = $2 AND subject_id = $3 \
               AND revision < 9223372036854775807",
        )
        .bind(tenant_id)
        .bind(subject_kind)
        .bind(subject_id.to_string())
        .execute(&mut **tx)
        .await?;
        if updated.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "routing grant revision is exhausted".into(),
            ));
        }
    }
    Ok(())
}

async fn ensure_relation_revision(
    tx: &mut Transaction<'_, Any>,
    tenant_id: &str,
    subject_kind: &str,
    subject_id: Uuid,
) -> Result<(), AppError> {
    let (key_id, route_id) = if subject_kind == CREDENTIAL_SUBJECT {
        (Some(subject_id.to_string()), None)
    } else if subject_kind == ROUTE_SUBJECT {
        (None, Some(subject_id.to_string()))
    } else {
        return Err(AppError::Internal);
    };
    sqlx::query(
        "INSERT INTO routing_grant_relation_revisions \
         (tenant_id, subject_kind, subject_id, key_id, model_route_id, revision) \
         VALUES ($1, $2, $3, $4, $5, 0) \
         ON CONFLICT (tenant_id, subject_kind, subject_id) DO NOTHING",
    )
    .bind(tenant_id)
    .bind(subject_kind)
    .bind(subject_id.to_string())
    .bind(key_id)
    .bind(route_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}
