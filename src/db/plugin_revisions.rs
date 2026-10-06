use super::*;
use crate::plugin::application::ApplicationRevision;

impl Database {
    pub(crate) async fn begin_empty_plugin_registration(
        &self,
        inventory_id: &str,
        event_key: &str,
        actor: &str,
    ) -> Result<Transaction<'_, Any>, AppError> {
        let mut transaction = self.begin_write_transaction().await?;
        let now = unix_millis();
        sqlx::query("INSERT INTO application_plugin_install_lock (scope,operation_id,lease_until) VALUES ('global',$1,0) ON CONFLICT(scope) DO NOTHING")
            .bind(event_key).execute(&mut *transaction).await?;
        let locked = sqlx::query("UPDATE application_plugin_install_lock SET operation_id=$1,lease_until=$2 WHERE scope='global' AND lease_until <= $3")
            .bind(event_key).bind(now + 90_000).bind(now).execute(&mut *transaction).await?.rows_affected();
        if locked != 1 {
            return Err(AppError::Overloaded);
        }
        let installed =
            sqlx::query("SELECT id FROM application_plugin_installations WHERE inventory_id=$1")
                .bind(inventory_id)
                .fetch_optional(&mut *transaction)
                .await?;
        if installed.is_some() {
            return Err(AppError::Conflict(
                "inventory ID belongs to an installation".into(),
            ));
        }
        sqlx::query("INSERT INTO application_plugin_audit (id,event_key,actor,action,inventory_id,outcome,created_at) VALUES ($1,$2,$3,'register_empty',$4,'staged',$5) ON CONFLICT(event_key) DO NOTHING")
            .bind(Uuid::now_v7().to_string()).bind(event_key).bind(actor).bind(inventory_id).bind(now)
            .execute(&mut *transaction).await?;
        let receipt = sqlx::query(
            "SELECT actor,action,inventory_id FROM application_plugin_audit WHERE event_key=$1",
        )
        .bind(event_key)
        .fetch_one(&mut *transaction)
        .await?;
        if receipt.try_get::<String, _>("actor")? != actor
            || receipt.try_get::<String, _>("action")? != "register_empty"
            || receipt.try_get::<String, _>("inventory_id")? != inventory_id
        {
            return Err(AppError::Conflict(
                "idempotency key was used for another operation".into(),
            ));
        }
        Ok(transaction)
    }

    pub(crate) async fn finish_empty_plugin_registration(
        &self,
        mut transaction: Transaction<'_, Any>,
        inventory_id: &str,
        event_key: &str,
        identity_digest: &str,
        contract_digest: &str,
    ) -> Result<(), AppError> {
        sqlx::query("INSERT INTO application_plugin_candidates (inventory_id,identity_digest,contract_digest,created_at) VALUES ($1,$2,$3,$4) ON CONFLICT(inventory_id) DO NOTHING")
            .bind(inventory_id).bind(identity_digest).bind(contract_digest).bind(unix_millis())
            .execute(&mut *transaction).await?;
        let candidate = sqlx::query("SELECT identity_digest,contract_digest FROM application_plugin_candidates WHERE inventory_id=$1")
            .bind(inventory_id).fetch_one(&mut *transaction).await?;
        if candidate.try_get::<String, _>("identity_digest")? != identity_digest
            || candidate.try_get::<String, _>("contract_digest")? != contract_digest
        {
            return Err(AppError::Conflict("inventory ID is immutable".into()));
        }
        sqlx::query("UPDATE application_plugin_install_lock SET lease_until=0 WHERE scope='global' AND operation_id=$1")
            .bind(event_key).execute(&mut *transaction).await?;
        transaction.commit().await?;
        Ok(())
    }

    pub(crate) async fn application_plugin_history(
        &self,
        before: Option<i64>,
    ) -> Result<Vec<serde_json::Value>, AppError> {
        sqlx::query("SELECT revision,inventory_id,reason,created_at FROM application_plugin_revisions WHERE ($1 IS NULL OR revision < $1) ORDER BY revision DESC LIMIT 100")
            .bind(before).fetch_all(&self.pool).await?.into_iter().map(|row| Ok(serde_json::json!({
                "revision":row.try_get::<i64,_>("revision")?,"inventory_id":row.try_get::<String,_>("inventory_id")?,
                "reason":row.try_get::<String,_>("reason")?,"created_at":row.try_get::<i64,_>("created_at")?,
            }))).collect()
    }
    pub(crate) async fn replay_application_plugin_operation(
        &self,
        key: &str,
        hash: &str,
    ) -> Result<Option<ApplicationRevision>, AppError> {
        let row = sqlx::query("SELECT request_hash, result_revision FROM application_plugin_operations WHERE idempotency_key = $1")
            .bind(key).fetch_optional(&self.pool).await?;
        let Some(row) = row else { return Ok(None) };
        if row.try_get::<String, _>("request_hash")? != hash {
            return Err(AppError::Conflict(
                "idempotency key was used for another operation".into(),
            ));
        }
        let revision: Option<i64> = row.try_get("result_revision")?;
        let revision = revision.ok_or(AppError::Internal)?;
        self.application_plugin_revision(revision).await.map(Some)
    }

    pub(crate) async fn staged_application_plugin_ids(&self) -> Result<Vec<String>, AppError> {
        Ok(sqlx::query_scalar(
            "SELECT inventory_id FROM application_plugin_candidates ORDER BY inventory_id",
        )
        .fetch_all(&self.pool)
        .await?)
    }

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
        self.optional_application_plugin_head()
            .await?
            .ok_or(AppError::Internal)
    }

    pub(crate) async fn optional_application_plugin_head(
        &self,
    ) -> Result<Option<ApplicationRevision>, AppError> {
        let row = sqlx::query("SELECT r.revision, r.inventory_id, r.reason, c.identity_digest, c.contract_digest FROM application_plugin_head h JOIN application_plugin_revisions r ON r.revision = h.revision JOIN application_plugin_candidates c ON c.inventory_id = r.inventory_id WHERE h.scope = 'global'")
            .fetch_optional(&self.pool).await?;
        row.map(application_revision).transpose()
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
        actor: &str,
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
        // Contract changes are admitted by host grants during staging. The
        // immutable candidate FK and head CAS remain the publication authority;
        // requiring equality here would forbid installing any new provider.
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
        sqlx::query("INSERT INTO application_plugin_audit (id,event_key,actor,action,inventory_id,revision,outcome,created_at) VALUES ($1,$2,$3,$4,$5,$6,'published',$7) ON CONFLICT(event_key) DO NOTHING")
            .bind(Uuid::now_v7().to_string()).bind(format!("revision:{next}")).bind(actor)
            .bind(if reason=="rollback" {"rollback"}else{"publish"}).bind(inventory_id).bind(next).bind(unix_millis())
            .execute(&mut *tx).await?;
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
