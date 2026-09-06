use sqlx::Row;
use uuid::Uuid;

use super::{AppError, Database, TenantManagementView, unix_millis};

const DEFAULT_TENANT: &str = "default";

// Deletion is the sole destructive lifecycle operation. Renaming and
// archiving preserve a tenant's stable UUID and all tenant-owned history.
// The final DELETE is also protected by database foreign keys against a
// concurrent or newly-added dependency.
const TENANT_HAS_DEPENDENCIES: &str = r#"
    SELECT EXISTS(
        SELECT 1 FROM key_records WHERE tenant_id = $1
        UNION ALL SELECT 1 FROM principals WHERE tenant_id = $1
        UNION ALL SELECT 1 FROM credit_accounts WHERE tenant_id = $1
        UNION ALL SELECT 1 FROM upstream_accounts WHERE tenant_id = $1
        UNION ALL SELECT 1 FROM model_routes WHERE tenant_id = $1
        UNION ALL SELECT 1 FROM request_records WHERE tenant_id = $1
        UNION ALL SELECT 1 FROM generation_jobs WHERE tenant_id = $1
        UNION ALL SELECT 1 FROM plugin_configurations WHERE tenant_id = $1
        UNION ALL SELECT 1 FROM routing_relation_write_locks WHERE tenant_id = $1
        UNION ALL SELECT 1 FROM session_archive_import_checkpoints WHERE tenant_id = $1
    ) AS has_dependencies
"#;

impl Database {
    pub async fn list_tenant_management(&self) -> Result<Vec<TenantManagementView>, AppError> {
        let rows = sqlx::query(
            "SELECT external_id, status, created_at, CASE WHEN updated_at = 0 THEN created_at ELSE updated_at END AS updated_at FROM tenants ORDER BY external_id ASC LIMIT 100",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(tenant_management_view).collect()
    }

    pub async fn create_tenant(
        &self,
        external_id: &str,
        actor_service_id: Option<Uuid>,
    ) -> Result<TenantManagementView, AppError> {
        let external_id = normalize_tenant_external_id(external_id)?;
        let tenant_id = Uuid::now_v7();
        let now = unix_millis();
        let mut transaction = self.begin_write_transaction().await?;
        let inserted = sqlx::query(
            "INSERT INTO tenants (id, external_id, created_at, updated_at) VALUES ($1, $2, $3, $4) ON CONFLICT(external_id) DO NOTHING",
        )
        .bind(tenant_id.to_string())
        .bind(&external_id)
        .bind(now)
        .bind(now)
        .execute(&mut *transaction)
        .await?;
        if inserted.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "a tenant with this identifier already exists".into(),
            ));
        }
        record_lifecycle_audit(
            &mut transaction,
            tenant_id,
            &external_id,
            "created",
            actor_service_id,
            now,
        )
        .await?;
        transaction.commit().await?;
        Ok(TenantManagementView {
            external_id,
            status: "active".into(),
            created_at: now,
            updated_at: now,
        })
    }

    pub async fn rename_tenant(
        &self,
        current_external_id: &str,
        next_external_id: &str,
        actor_service_id: Option<Uuid>,
    ) -> Result<TenantManagementView, AppError> {
        let current_external_id = normalize_tenant_external_id(current_external_id)?;
        let next_external_id = normalize_tenant_external_id(next_external_id)?;
        reject_default_tenant_change(&current_external_id)?;
        let now = unix_millis();
        let mut transaction = self.begin_write_transaction().await?;
        let tenant = tenant_for_lifecycle(&mut transaction, &current_external_id).await?;
        let changed = sqlx::query(
            "UPDATE tenants SET external_id = $1, updated_at = $2 WHERE id = $3 AND external_id = $4",
        )
        .bind(&next_external_id)
        .bind(now)
        .bind(tenant.0.to_string())
        .bind(&current_external_id)
        .execute(&mut *transaction)
        .await
        .map_err(lifecycle_write_error)?;
        if changed.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "reload the tenant before renaming it".into(),
            ));
        }
        // Tenant UUIDs remain the source of truth for product relations. The
        // two external-ID columns below deliberately exist for management
        // credential scoping and an in-flight OAuth handoff, so keep them in
        // the same transaction as the durable tenant rename.
        sqlx::query(
            "UPDATE service_credentials SET tenant_external_id = $1 WHERE tenant_external_id = $2",
        )
        .bind(&next_external_id)
        .bind(&current_external_id)
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            "UPDATE oauth_login_sessions SET tenant_external_id = $1 WHERE tenant_external_id = $2",
        )
        .bind(&next_external_id)
        .bind(&current_external_id)
        .execute(&mut *transaction)
        .await?;
        // Archive import checkpoints intentionally need no textual rewrite:
        // they are keyed by tenant_id and therefore follow this immutable ID.
        record_lifecycle_audit(
            &mut transaction,
            tenant.0,
            &next_external_id,
            "renamed",
            actor_service_id,
            now,
        )
        .await?;
        transaction.commit().await?;
        Ok(TenantManagementView {
            external_id: next_external_id,
            status: tenant.1,
            created_at: tenant.2,
            updated_at: now,
        })
    }

    pub async fn set_tenant_archived(
        &self,
        external_id: &str,
        archived: bool,
        actor_service_id: Option<Uuid>,
    ) -> Result<TenantManagementView, AppError> {
        let external_id = normalize_tenant_external_id(external_id)?;
        reject_default_tenant_change(&external_id)?;
        let status = if archived { "archived" } else { "active" };
        let now = unix_millis();
        let mut transaction = self.begin_write_transaction().await?;
        let tenant = tenant_for_lifecycle(&mut transaction, &external_id).await?;
        if tenant.1 == status {
            return Err(AppError::Conflict(
                "tenant already has this lifecycle state".into(),
            ));
        }
        let changed =
            sqlx::query("UPDATE tenants SET status = $1, updated_at = $2 WHERE id = $3")
                .bind(status)
                .bind(now)
                .bind(tenant.0.to_string())
                .execute(&mut *transaction)
                .await?;
        if changed.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "reload the tenant before changing its lifecycle state".into(),
            ));
        }
        record_lifecycle_audit(
            &mut transaction,
            tenant.0,
            &external_id,
            if archived { "archived" } else { "restored" },
            actor_service_id,
            now,
        )
        .await?;
        transaction.commit().await?;
        Ok(TenantManagementView {
            external_id,
            status: status.into(),
            created_at: tenant.2,
            updated_at: now,
        })
    }

    pub async fn delete_archived_tenant(
        &self,
        external_id: &str,
        actor_service_id: Option<Uuid>,
    ) -> Result<(), AppError> {
        let external_id = normalize_tenant_external_id(external_id)?;
        reject_default_tenant_change(&external_id)?;
        let now = unix_millis();
        let mut transaction = self.begin_write_transaction().await?;
        let tenant = tenant_for_lifecycle(&mut transaction, &external_id).await?;
        if tenant.1 != "archived" {
            return Err(AppError::Conflict(
                "archive the tenant before deleting it".into(),
            ));
        }
        require_empty_tenant(&mut transaction, &tenant.0).await?;
        record_lifecycle_audit(
            &mut transaction,
            tenant.0,
            &external_id,
            "deleted",
            actor_service_id,
            now,
        )
        .await?;
        let deleted = sqlx::query("DELETE FROM tenants WHERE id = $1 AND status = 'archived'")
            .bind(tenant.0.to_string())
            .execute(&mut *transaction)
            .await
            .map_err(lifecycle_write_error)?;
        if deleted.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "reload the tenant before deleting it".into(),
            ));
        }
        transaction.commit().await?;
        Ok(())
    }
}

fn normalize_tenant_external_id(value: &str) -> Result<String, AppError> {
    let value = value.trim();
    if value.is_empty() || value.len() > 200 || value.chars().any(char::is_control) {
        return Err(AppError::BadRequest(
            "tenant identifier must contain 1 to 200 non-control characters".into(),
        ));
    }
    Ok(value.to_owned())
}

fn reject_default_tenant_change(external_id: &str) -> Result<(), AppError> {
    if external_id == DEFAULT_TENANT {
        return Err(AppError::Conflict(
            "the production default tenant cannot be renamed, archived, or deleted".into(),
        ));
    }
    Ok(())
}

async fn tenant_for_lifecycle(
    transaction: &mut sqlx::Transaction<'_, sqlx::Any>,
    external_id: &str,
) -> Result<(Uuid, String, i64), AppError> {
    let row = sqlx::query("SELECT id, status, created_at FROM tenants WHERE external_id = $1")
        .bind(external_id)
        .fetch_optional(&mut **transaction)
        .await?
        .ok_or(AppError::NotFound)?;
    let id = row.try_get::<String, _>("id")?;
    Ok((
        Uuid::parse_str(&id).map_err(|_| AppError::Internal)?,
        row.try_get("status")?,
        row.try_get("created_at")?,
    ))
}

async fn require_empty_tenant(
    transaction: &mut sqlx::Transaction<'_, sqlx::Any>,
    tenant_id: &Uuid,
) -> Result<(), AppError> {
    let has_dependencies: bool = sqlx::query_scalar(TENANT_HAS_DEPENDENCIES)
        .bind(tenant_id.to_string())
        .fetch_one(&mut **transaction)
        .await?;
    if has_dependencies {
        return Err(AppError::Conflict(
            "remove tenant-owned resources before deleting this tenant".into(),
        ));
    }
    Ok(())
}

async fn record_lifecycle_audit(
    transaction: &mut sqlx::Transaction<'_, sqlx::Any>,
    tenant_id: Uuid,
    external_id: &str,
    action: &str,
    actor_service_id: Option<Uuid>,
    now: i64,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO tenant_lifecycle_audit (id, tenant_id, external_id, action, actor_service_id, created_at) VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(tenant_id.to_string())
    .bind(external_id)
    .bind(action)
    .bind(actor_service_id.map(|value| value.to_string()))
    .bind(now)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

fn lifecycle_write_error(error: sqlx::Error) -> AppError {
    match error {
        sqlx::Error::Database(_) => AppError::Conflict(
            "tenant lifecycle change conflicts with current data; refresh and try again".into(),
        ),
        other => other.into(),
    }
}

fn tenant_management_view(row: sqlx::any::AnyRow) -> Result<TenantManagementView, AppError> {
    Ok(TenantManagementView {
        external_id: row.try_get("external_id")?,
        status: row.try_get("status")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}
