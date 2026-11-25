//! Resolve the non-partitioned request identity before touching its body row.
use super::*;

pub(in crate::db) struct AuthoritativeRequestLocator {
    pub(in crate::db) key_id: Uuid,
    pub(in crate::db) created_at: i64,
}

impl AuthoritativeRequestLocator {
    /// Only the decoded catalog timestamp becomes SQL text. Other identities
    /// remain bound, and callers disable caching for this per-request statement.
    pub(in crate::db) fn created_at_predicate(
        &self,
        backend: DatabaseBackend,
        column: &'static str,
        sqlite_parameter: &'static str,
    ) -> String {
        match backend {
            DatabaseBackend::PostgreSql => format!("{column} = {}", self.created_at),
            DatabaseBackend::Sqlite => format!("{column} = {sqlite_parameter}"),
        }
    }
}

impl Database {
    pub(in crate::db) async fn authoritative_request_locator_in_transaction(
        &self,
        transaction: &mut Transaction<'_, Any>,
        request_id: Uuid,
        tenant_id: Uuid,
    ) -> Result<Option<AuthoritativeRequestLocator>, AppError> {
        let row = sqlx::query(
            "SELECT key_id, created_at FROM request_record_locators WHERE id=$1 AND tenant_id=$2",
        )
        .bind(request_id.to_string())
        .bind(tenant_id.to_string())
        .fetch_optional(&mut **transaction)
        .await?;
        row.map(|row| {
            Ok(AuthoritativeRequestLocator {
                key_id: parse_uuid(row.try_get("key_id")?)?,
                created_at: row.try_get("created_at")?,
            })
        })
        .transpose()
    }
}
