use super::super::*;

#[cfg(test)]
mod tests;

pub(crate) const REQUEST_OWNER_LEASE_MILLIS: i64 =
    crate::archive_staging::ARCHIVE_STAGING_WRITE_LEASE_MILLIS;
pub(crate) const REQUEST_OWNER_HEARTBEAT_MILLIS: i64 =
    crate::archive_staging::ARCHIVE_STAGING_WRITE_HEARTBEAT_MILLIS;

#[derive(Clone)]
pub(crate) struct ProxyRequestOwner {
    pub(crate) request_id: Uuid,
    pub(crate) tenant_id: Uuid,
    pub(crate) key_id: Uuid,
    pub(crate) reservation_id: Uuid,
    pub(crate) token: Uuid,
}

impl Database {
    pub(crate) async fn proxy_request_owner_finished(
        &self,
        owner: &ProxyRequestOwner,
    ) -> Result<bool, AppError> {
        Ok(sqlx::query("SELECT 1 FROM request_records WHERE id = $1 AND tenant_id = $2 AND key_id = $3 AND reservation_id = $4 AND completed_at IS NOT NULL")
            .bind(owner.request_id.to_string()).bind(owner.tenant_id.to_string())
            .bind(owner.key_id.to_string()).bind(owner.reservation_id.to_string())
            .fetch_optional(&self.pool).await?.is_some())
    }

    pub(super) async fn request_owner_now(
        &self,
        connection: &mut AnyConnection,
    ) -> Result<i64, AppError> {
        let query = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT CAST(EXTRACT(EPOCH FROM clock_timestamp()) * 1000 AS BIGINT)"
            }
            DatabaseBackend::Sqlite => {
                "SELECT CAST((julianday('now') - 2440587.5) * 86400000 AS BIGINT)"
            }
        };
        Ok(sqlx::query_scalar(query).fetch_one(connection).await?)
    }

    pub(crate) async fn acquire_proxy_request_owner(
        &self,
        owner: &ProxyRequestOwner,
    ) -> Result<bool, AppError> {
        self.write_proxy_request_owner(owner, true).await
    }

    pub(crate) async fn renew_proxy_request_owner(
        &self,
        owner: &ProxyRequestOwner,
    ) -> Result<bool, AppError> {
        self.write_proxy_request_owner(owner, false).await
    }

    async fn write_proxy_request_owner(
        &self,
        owner: &ProxyRequestOwner,
        acquire: bool,
    ) -> Result<bool, AppError> {
        let mut transaction = self.begin_write_transaction().await?;
        let locked = sqlx::query("UPDATE request_records SET proxy_owner_token = $5 WHERE id = $1 AND tenant_id = $2 AND key_id = $3 AND reservation_id = $4 AND completed_at IS NULL AND submission_started_at IS NULL AND (proxy_owner_token = $5 OR ($6 = 1 AND proxy_owner_token IS NULL))")
            .bind(owner.request_id.to_string())
            .bind(owner.tenant_id.to_string())
            .bind(owner.key_id.to_string())
            .bind(owner.reservation_id.to_string())
            .bind(owner.token.to_string())
            .bind(i64::from(acquire))
            .execute(&mut *transaction).await?;
        if locked.rows_affected() == 0 {
            return Ok(false);
        }
        let now = self.request_owner_now(&mut transaction).await?;
        sqlx::query("UPDATE request_records SET proxy_owner_expires_at = $1 WHERE id = $2 AND tenant_id = $3 AND key_id = $4 AND reservation_id = $5 AND proxy_owner_token = $6 AND completed_at IS NULL")
            .bind(now.saturating_add(REQUEST_OWNER_LEASE_MILLIS))
            .bind(owner.request_id.to_string()).bind(owner.tenant_id.to_string())
            .bind(owner.key_id.to_string()).bind(owner.reservation_id.to_string())
            .bind(owner.token.to_string()).execute(&mut *transaction).await?;
        sqlx::query("UPDATE response_archive_spools SET updated_at = $1, expires_at = $2 WHERE request_id = $3 AND tenant_id = $4 AND reservation_id = $5 AND state = 'capturing' AND expires_at > $1")
            .bind(now).bind(now.saturating_add(super::super::archive_spool::CAPTURE_TTL))
            .bind(owner.request_id.to_string()).bind(owner.tenant_id.to_string())
            .bind(owner.reservation_id.to_string()).execute(&mut *transaction).await?;
        transaction.commit().await?;
        Ok(true)
    }
}
