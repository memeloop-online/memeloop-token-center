//! SQLite runtime verification uses its own ledger and catalog, never PG roles.
use crate::db::{Database, DatabaseBackend, migrations::SQLITE_MIGRATIONS};
use sqlx::Row;

impl Database {
    pub(in crate::db) async fn verify_sqlite_runtime(&self) -> Result<bool, sqlx::Error> {
        if !matches!(self.backend, DatabaseBackend::Sqlite) {
            return Ok(false);
        }
        let mut tx = self.pool.begin().await?;
        let ledger: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='schema_migrations'",
        )
        .fetch_one(&mut *tx)
        .await?;
        if ledger == 0 {
            return Ok(false);
        }
        let rows = sqlx::query("SELECT version,name FROM schema_migrations ORDER BY version")
            .fetch_all(&mut *tx)
            .await?;
        let last = rows
            .last()
            .map(|r| r.try_get::<i64, _>("version"))
            .transpose()?;
        if last.is_none_or(|v| v < 165) {
            return Ok(false);
        }
        if rows.len() != SQLITE_MIGRATIONS.len() {
            return Err(sqlx::Error::Protocol(
                "SQLite runtime migration ledger differs".into(),
            ));
        }
        for (row, migration) in rows.iter().zip(SQLITE_MIGRATIONS) {
            if row.try_get::<i64, _>("version")? != migration.version
                || row.try_get::<String, _>("name")? != migration.name
            {
                return Err(sqlx::Error::Protocol(
                    "SQLite runtime migration identity differs".into(),
                ));
            }
        }
        for table in [
            "request_records",
            "request_record_locators",
            "usage_reservations",
            "request_events",
            "terminal_projection_outbox",
            "request_history_state",
            "request_history_baseline_runs",
        ] {
            let exists: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=$1",
            )
            .bind(table)
            .fetch_one(&mut *tx)
            .await?;
            if exists != 1 {
                return Err(sqlx::Error::Protocol(
                    "SQLite runtime relation missing".into(),
                ));
            }
        }
        let columns = sqlx::query("PRAGMA table_info(request_records)")
            .fetch_all(&mut *tx)
            .await?;
        let marker = columns
            .iter()
            .find(|r| {
                r.try_get::<String, _>("name")
                    .is_ok_and(|name| name == "started_event_pending")
            })
            .ok_or_else(|| sqlx::Error::Protocol("SQLite runtime started marker missing".into()))?;
        if marker.try_get::<String, _>("type")?.to_uppercase() != "BIGINT"
            || marker.try_get::<i64, _>("notnull")? != 1
            || marker
                .try_get::<Option<String>, _>("dflt_value")?
                .as_deref()
                != Some("0")
        {
            return Err(sqlx::Error::Protocol(
                "SQLite runtime started marker differs".into(),
            ));
        }
        let index:Option<String>=sqlx::query_scalar("SELECT sql FROM sqlite_master WHERE type='index' AND name='request_records_started_pending' AND tbl_name='request_records'").fetch_optional(&mut *tx).await?;
        let normalize = |value: &str| {
            value
                .chars()
                .filter(|c| !c.is_whitespace() && *c != ';')
                .collect::<String>()
                .to_lowercase()
        };
        if index.as_deref().map(normalize)
            != Some(normalize(
                "CREATE INDEX request_records_started_pending ON request_records (created_at, id) WHERE started_event_pending = 1",
            ))
        {
            return Err(sqlx::Error::Protocol(
                "SQLite runtime started index differs".into(),
            ));
        }
        tx.commit().await?;
        self.install_schema_track(super::SchemaTrack::Linear165)?;
        Ok(true)
    }
}
