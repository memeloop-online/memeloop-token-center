use super::*;
use crate::plugin::application::ApplicationRevision;

impl Database {
    pub(crate) async fn stage_application_plugin_candidate(
        &self,
        inventory_id: &str,
        identity_digest: &str,
        contract_digest: &str,
    ) -> Result<(), AppError> {
        sqlx::query("INSERT INTO application_plugin_candidates (inventory_id, identity_digest, contract_digest, created_at) VALUES ($1, $2, $3, $4) ON CONFLICT(inventory_id) DO NOTHING")
            .bind(inventory_id).bind(identity_digest).bind(contract_digest).bind(unix_millis())
            .execute(&self.pool).await?;
        let row = sqlx::query("SELECT identity_digest, contract_digest FROM application_plugin_candidates WHERE inventory_id = $1")
            .bind(inventory_id).fetch_one(&self.pool).await?;
        if row.try_get::<String, _>("identity_digest")? != identity_digest
            || row.try_get::<String, _>("contract_digest")? != contract_digest
        {
            return Err(AppError::Conflict("inventory ID is immutable".into()));
        }
        Ok(())
    }

    /// Always consult the primary database; notifications are not authority.
    pub(crate) async fn application_plugin_head(&self) -> Result<ApplicationRevision, AppError> {
        let row = sqlx::query("SELECT r.revision, r.inventory_id, r.reason, c.identity_digest, c.contract_digest FROM application_plugin_head h JOIN application_plugin_revisions r ON r.revision = h.revision JOIN application_plugin_candidates c ON c.inventory_id = r.inventory_id WHERE h.scope = 'global'")
            .fetch_optional(&self.pool).await?.ok_or(AppError::Internal)?;
        application_revision(row)
    }

    pub(crate) async fn application_plugin_revision(
        &self,
        revision: i64,
    ) -> Result<ApplicationRevision, AppError> {
        let row = sqlx::query("SELECT r.revision, r.inventory_id, r.reason, c.identity_digest, c.contract_digest FROM application_plugin_revisions r JOIN application_plugin_candidates c ON c.inventory_id = r.inventory_id WHERE r.revision = $1")
            .bind(revision).fetch_optional(&self.pool).await?.ok_or(AppError::NotFound)?;
        application_revision(row)
    }

    /// Operation claim, monotonic revision publication and CAS commit together.
    /// The caller has validated the immutable, locally preinstalled candidate.
    pub(crate) async fn publish_application_plugin(
        &self,
        inventory_id: &str,
        expected_revision: i64,
        reason: &str,
        idempotency_key: &str,
        request_hash: &str,
    ) -> Result<ApplicationRevision, AppError> {
        let next = expected_revision
            .checked_add(1)
            .filter(|n| *n > 0)
            .ok_or(AppError::Internal)?;
        let mut tx = self.pool.begin().await?;
        let claimed = sqlx::query("INSERT INTO application_plugin_operations (idempotency_key, request_hash, created_at) VALUES ($1, $2, $3) ON CONFLICT(idempotency_key) DO NOTHING")
            .bind(idempotency_key).bind(request_hash).bind(unix_millis()).execute(&mut *tx).await?;
        if claimed.rows_affected() == 0 {
            let row = sqlx::query("SELECT request_hash, result_revision FROM application_plugin_operations WHERE idempotency_key = $1")
                .bind(idempotency_key).fetch_one(&mut *tx).await?;
            if row.try_get::<String, _>("request_hash")? != request_hash {
                return Err(AppError::Conflict(
                    "idempotency key was used for another operation".into(),
                ));
            }
            let revision: Option<i64> = row.try_get("result_revision")?;
            let revision = revision.ok_or(AppError::Internal)?;
            tx.commit().await?;
            return self.application_plugin_revision(revision).await;
        }
        // Reserve the new revision only if this expected head still wins. A
        // competing transaction either sees the CAS miss or rolls back its
        // tentative insert on conflict. No partially published row survives.
        if expected_revision > 0 {
            let compatible: Option<i64> = sqlx::query_scalar("SELECT r.revision FROM application_plugin_revisions r JOIN application_plugin_candidates previous ON previous.inventory_id = r.inventory_id JOIN application_plugin_candidates candidate ON candidate.inventory_id = $1 WHERE r.revision = $2 AND previous.contract_digest = candidate.contract_digest")
                .bind(inventory_id).bind(expected_revision).fetch_optional(&mut *tx).await?;
            if compatible.is_none() {
                return Err(AppError::Forbidden);
            }
        }
        let inserted = sqlx::query("INSERT INTO application_plugin_revisions (revision, inventory_id, reason, created_at) VALUES ($1, $2, $3, $4) ON CONFLICT(revision) DO NOTHING")
            .bind(next).bind(inventory_id).bind(reason).bind(unix_millis()).execute(&mut *tx).await?;
        if inserted.rows_affected() != 1 {
            return Err(AppError::Conflict("plugin runtime revision changed".into()));
        }
        let changed = if expected_revision == 0 {
            sqlx::query("INSERT INTO application_plugin_head (scope, revision) VALUES ('global', $1) ON CONFLICT(scope) DO NOTHING")
                .bind(next).execute(&mut *tx).await?.rows_affected()
        } else {
            sqlx::query("UPDATE application_plugin_head SET revision = $1 WHERE scope = 'global' AND revision = $2")
                .bind(next).bind(expected_revision).execute(&mut *tx).await?.rows_affected()
        };
        if changed != 1 {
            return Err(AppError::Conflict("plugin runtime revision changed".into()));
        }
        sqlx::query("UPDATE application_plugin_operations SET result_revision = $1 WHERE idempotency_key = $2")
            .bind(next).bind(idempotency_key).execute(&mut *tx).await?;
        tx.commit().await?;
        self.application_plugin_revision(next).await
    }
}

fn application_revision(row: AnyRow) -> Result<ApplicationRevision, AppError> {
    Ok(ApplicationRevision {
        revision: row.try_get("revision")?,
        inventory_id: row.try_get("inventory_id")?,
        reason: row.try_get("reason")?,
        identity_digest: row.try_get("identity_digest")?,
        contract_digest: row.try_get("contract_digest")?,
    })
}
