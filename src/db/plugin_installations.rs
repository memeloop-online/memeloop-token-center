use super::*;
use crate::plugin::application::installation::{
    InstallationRecord, PackageCheckpoint, PluginAuditEntry,
};

impl Database {
    pub(crate) async fn replay_plugin_installation(
        &self,
        key: &str,
        hash: &str,
        actor: &str,
    ) -> Result<Option<InstallationRecord>, AppError> {
        let row =
            sqlx::query("SELECT * FROM application_plugin_installations WHERE idempotency_hash=$1")
                .bind(key)
                .fetch_optional(&self.pool)
                .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        if row.try_get::<String, _>("request_hash")? != hash
            || row.try_get::<String, _>("actor")? != actor
        {
            return Err(AppError::Conflict(
                "installation idempotency key was used for another operation".into(),
            ));
        }
        let record = installation_record(row)?;
        if matches!(
            record.status.as_str(),
            "review" | "registered" | "installing"
        ) {
            Ok(Some(record))
        } else {
            Ok(None)
        }
    }
    pub(crate) async fn plugin_installation(
        &self,
        id: &str,
    ) -> Result<InstallationRecord, AppError> {
        let row = sqlx::query("SELECT * FROM application_plugin_installations WHERE id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(AppError::NotFound)?;
        installation_record(row)
    }

    pub(crate) async fn plugin_installations(&self) -> Result<Vec<InstallationRecord>, AppError> {
        sqlx::query("SELECT id,idempotency_hash,request_hash,inventory_id,packages_json,actor,attempt_id,status,review_digest,NULL AS review_json,checkpoints_json,failure_category,lease_until,created_at,updated_at FROM application_plugin_installations ORDER BY created_at DESC, id DESC LIMIT 100")
            .fetch_all(&self.pool).await?.into_iter().map(installation_record).collect()
    }

    pub(crate) async fn begin_plugin_installation(
        &self,
        inventory_id: &str,
        packages: &serde_json::Value,
        request_hash: &str,
        idempotency_hash: &str,
        actor: &str,
        lease_until: i64,
    ) -> Result<(InstallationRecord, bool), AppError> {
        let now = unix_millis();
        let id = Uuid::now_v7().to_string();
        let mut tx = self.pool.begin().await?;
        let inserted = sqlx::query("INSERT INTO application_plugin_installations (id, idempotency_hash, request_hash, inventory_id, packages_json, actor, attempt_id, status, lease_until, created_at, updated_at) VALUES ($1,$2,$3,$4,$5,$6,$1,'installing',$7,$8,$8) ON CONFLICT DO NOTHING")
            .bind(&id).bind(idempotency_hash).bind(request_hash).bind(inventory_id).bind(packages.to_string()).bind(actor).bind(lease_until).bind(now)
            .execute(&mut *tx).await?.rows_affected();
        let row = sqlx::query(
            "SELECT * FROM application_plugin_installations WHERE idempotency_hash = $1",
        )
        .bind(idempotency_hash)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| {
            AppError::Conflict("inventory ID already belongs to another installation".into())
        })?;
        if row.try_get::<String, _>("request_hash")? != request_hash
            || row.try_get::<String, _>("actor")? != actor
        {
            return Err(AppError::Conflict(
                "installation idempotency key was used for another operation".into(),
            ));
        }
        let record = installation_record(row)?;
        if inserted == 0
            && (record.status == "review"
                || record.status == "registered"
                || record.lease_until > now)
        {
            tx.commit().await?;
            return Ok((record, false));
        }
        sqlx::query("INSERT INTO application_plugin_install_lock (scope, operation_id, lease_until) VALUES ('global',$1,$2) ON CONFLICT(scope) DO NOTHING")
            .bind(&id).bind(lease_until).execute(&mut *tx).await?;
        let locked = sqlx::query("UPDATE application_plugin_install_lock SET operation_id=$1, lease_until=$2 WHERE scope='global' AND (lease_until <= $3 OR operation_id=$1)")
            .bind(&id).bind(lease_until).bind(now).execute(&mut *tx).await?.rows_affected();
        if locked != 1 {
            return Err(AppError::Overloaded);
        }
        // Each attempt publishes into a distinct physical inventory root. Old
        // checkpoints belong to the previous root and must never be reused.
        sqlx::query("UPDATE application_plugin_installations SET status='installing', review_digest=NULL, review_json=NULL, checkpoints_json='{}', failure_category=NULL, lease_until=$1, updated_at=$2, attempt_id=$4 WHERE id=$3")
            .bind(lease_until).bind(now).bind(&record.id).bind(&id).execute(&mut *tx).await?;
        append_audit(
            &mut tx,
            actor,
            "install",
            Some(inventory_id),
            None,
            "started",
            &format!("install-start:{id}"),
        )
        .await?;
        tx.commit().await?;
        Ok((self.plugin_installation(&record.id).await?, true))
    }

    pub(crate) async fn renew_plugin_installation(
        &self,
        id: &str,
        attempt: &str,
    ) -> Result<(), AppError> {
        let now = unix_millis();
        let until = now + 270_000;
        let mut tx = self.pool.begin().await?;
        let locked = sqlx::query("UPDATE application_plugin_install_lock SET lease_until=$1 WHERE scope='global' AND operation_id=$2 AND lease_until > $3")
            .bind(until).bind(attempt).bind(now).execute(&mut *tx).await?.rows_affected();
        let changed = sqlx::query("UPDATE application_plugin_installations SET lease_until=$1 WHERE id=$2 AND attempt_id=$3 AND status='installing' AND lease_until > $4")
            .bind(until).bind(id).bind(attempt).bind(now).execute(&mut *tx).await?.rows_affected();
        if locked != 1 || changed != 1 {
            return Err(AppError::Conflict("installation lease changed".into()));
        }
        tx.commit().await?;
        Ok(())
    }

    pub(crate) async fn checkpoint_plugin_installation(
        &self,
        id: &str,
        attempt: &str,
        checkpoints: &std::collections::BTreeMap<String, PackageCheckpoint>,
    ) -> Result<(), AppError> {
        let encoded = serde_json::to_string(checkpoints).map_err(|_| AppError::Internal)?;
        if checkpoints.len() > 16 || encoded.len() > 65536 {
            return Err(AppError::Forbidden);
        }
        let now = unix_millis();
        let changed = sqlx::query("UPDATE application_plugin_installations SET checkpoints_json=$1, updated_at=$2 WHERE id=$3 AND attempt_id=$4 AND status='installing' AND lease_until > $2 AND EXISTS (SELECT 1 FROM application_plugin_install_lock WHERE scope='global' AND operation_id=$4 AND lease_until > $2)")
            .bind(encoded).bind(now).bind(id).bind(attempt).execute(&self.pool).await?.rows_affected();
        if changed != 1 {
            return Err(AppError::Conflict("installation attempt changed".into()));
        }
        Ok(())
    }

    pub(crate) async fn finish_plugin_installation(
        &self,
        id: &str,
        attempt_id: &str,
        review: Option<(&str, &serde_json::Value)>,
    ) -> Result<(), AppError> {
        let now = unix_millis();
        let (status, digest, value, failure) = match review {
            Some((digest, value)) => ("review", Some(digest), Some(value.to_string()), None),
            None => ("failed", None, None, Some("installation_failed")),
        };
        let mut tx = self.pool.begin().await?;
        let changed=sqlx::query("UPDATE application_plugin_installations SET status=$1, review_digest=$2, review_json=$3, failure_category=$4, lease_until=0, updated_at=$5 WHERE id=$6 AND status='installing' AND attempt_id=$7")
            .bind(status).bind(digest).bind(value).bind(failure).bind(now).bind(id).bind(attempt_id).execute(&mut *tx).await?.rows_affected();
        if changed != 1 {
            return Err(AppError::Conflict("installation attempt changed".into()));
        }
        sqlx::query(
            "DELETE FROM application_plugin_install_lock WHERE scope='global' AND operation_id=$1",
        )
        .bind(attempt_id)
        .execute(&mut *tx)
        .await?;
        let row = sqlx::query(
            "SELECT actor,inventory_id FROM application_plugin_installations WHERE id=$1",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        append_audit(
            &mut tx,
            &row.try_get::<String, _>("actor")?,
            "install",
            Some(&row.try_get::<String, _>("inventory_id")?),
            None,
            status,
            &format!("install-finish:{attempt_id}"),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub(crate) async fn register_plugin_installation(
        &self,
        id: &str,
        digest: &str,
        actor: &str,
    ) -> Result<(), AppError> {
        let mut tx = self.pool.begin().await?;
        let changed = sqlx::query("UPDATE application_plugin_installations SET status='registered', updated_at=$1 WHERE id=$2 AND review_digest=$3 AND status IN ('review','registered')")
            .bind(unix_millis()).bind(id).bind(digest).execute(&mut *tx).await?.rows_affected();
        if changed != 1 {
            return Err(AppError::Conflict("installation review changed".into()));
        }
        let inventory: String = sqlx::query_scalar(
            "SELECT inventory_id FROM application_plugin_installations WHERE id=$1",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        append_audit(
            &mut tx,
            actor,
            "approve",
            Some(&inventory),
            None,
            "registered",
            &format!("approve:{id}:{digest}"),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub(crate) async fn append_plugin_audit(
        &self,
        actor: &str,
        action: &str,
        inventory_id: Option<&str>,
        revision: Option<i64>,
        outcome: &str,
        event_key: &str,
    ) -> Result<(), AppError> {
        let mut tx = self.pool.begin().await?;
        append_audit(
            &mut tx,
            actor,
            action,
            inventory_id,
            revision,
            outcome,
            event_key,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub(crate) async fn plugin_audit(
        &self,
        before: Option<&str>,
    ) -> Result<Vec<PluginAuditEntry>, AppError> {
        sqlx::query("SELECT id,actor,action,inventory_id,revision,outcome,created_at FROM application_plugin_audit WHERE ($1 IS NULL OR id < $1) ORDER BY id DESC LIMIT 100")
            .bind(before).fetch_all(&self.pool).await?.into_iter().map(|row| Ok(PluginAuditEntry {
                id:row.try_get("id")?,
                actor:row.try_get("actor")?,action:row.try_get("action")?,inventory_id:row.try_get("inventory_id")?,
                revision:row.try_get("revision")?,outcome:row.try_get("outcome")?,created_at:row.try_get("created_at")?,
            })).collect()
    }
}

async fn append_audit(
    tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    actor: &str,
    action: &str,
    inventory: Option<&str>,
    revision: Option<i64>,
    outcome: &str,
    key: &str,
) -> Result<(), AppError> {
    sqlx::query("INSERT INTO application_plugin_audit (id,event_key,actor,action,inventory_id,revision,outcome,created_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT(event_key) DO NOTHING")
        .bind(Uuid::now_v7().to_string()).bind(key).bind(actor).bind(action).bind(inventory).bind(revision).bind(outcome).bind(unix_millis())
        .execute(&mut **tx).await?;
    Ok(())
}

fn installation_record(row: AnyRow) -> Result<InstallationRecord, AppError> {
    let mut status: String = row.try_get("status")?;
    let lease_until: i64 = row.try_get("lease_until")?;
    if status == "installing" && lease_until <= unix_millis() {
        status = "interrupted".into();
    }
    let checkpoints: std::collections::BTreeMap<String, PackageCheckpoint> =
        serde_json::from_str(&row.try_get::<String, _>("checkpoints_json")?)
            .map_err(|_| AppError::Internal)?;
    Ok(InstallationRecord {
        completed_packages: checkpoints.len(),
        checkpoints,
        id: row.try_get("id")?,
        inventory_id: row.try_get("inventory_id")?,
        actor: row.try_get("actor")?,
        attempt_id: row.try_get("attempt_id")?,
        idempotency_hash: row.try_get("idempotency_hash")?,
        status,
        packages: serde_json::from_str(&row.try_get::<String, _>("packages_json")?)
            .map_err(|_| AppError::Internal)?,
        review_digest: row.try_get("review_digest")?,
        review: row
            .try_get::<Option<String>, _>("review_json")?
            .map(|value| serde_json::from_str(&value))
            .transpose()
            .map_err(|_| AppError::Internal)?,
        failure_category: row.try_get("failure_category")?,
        lease_until,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}
