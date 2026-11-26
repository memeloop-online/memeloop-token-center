use super::*;
use crate::db::{Database, DatabaseBackend, migrations::POSTGRES_MIGRATIONS};
use sqlx::{AnyConnection, Connection, Row};

fn drift(message: &str) -> sqlx::Error {
    sqlx::Error::Protocol(format!("capability catalog rejected: {message}"))
}

pub(in crate::db) async fn inspect(
    c: &mut AnyConnection,
    role: CapabilityRole,
) -> Result<Option<CapabilityReport>, sqlx::Error> {
    let ledger: bool = sqlx::query_scalar("SELECT to_regclass('schema_migrations') IS NOT NULL")
        .fetch_one(&mut *c)
        .await?;
    if !ledger {
        return Ok(None);
    }
    let rows = sqlx::query("SELECT version,name FROM schema_migrations ORDER BY version")
        .fetch_all(&mut *c)
        .await?;
    let rows = super::sql_schema_rows(rows)?;
    let versions: Vec<i64> = rows
        .iter()
        .map(|r| r.try_get("version"))
        .collect::<Result<_, _>>()?;
    if versions.last().is_none_or(|v| *v < 163) {
        return Ok(None);
    }
    let last = *versions.last().unwrap();
    if ![163, 164, 165, 166].contains(&last) {
        return Err(drift("unsupported migration sequence"));
    }
    let expected: Vec<_> = POSTGRES_MIGRATIONS
        .iter()
        .filter(|m| m.version <= last)
        .collect();
    if rows.len() != expected.len() {
        return Err(drift("migration ledger gap"));
    }
    for (row, migration) in rows.iter().zip(expected) {
        if row.try_get::<i64, _>("version")? != migration.version
            || row.try_get::<String, _>("name")? != migration.name
        {
            return Err(drift("migration identity differs"));
        }
    }
    let track = match last {
        163 => SchemaTrack::Legacy163,
        164 => SchemaTrack::Linear164,
        165 => SchemaTrack::Linear165,
        _ => SchemaTrack::Linear166,
    };
    let mut signatures = Vec::new();
    for requirement in permissions::requirements_at(role, last) {
        let table = requirement.table;
        let row = sqlx::query("SELECT c.oid::bigint AS oid,c.relkind::text AS kind FROM pg_class c WHERE c.oid=to_regclass($1)")
            .bind(table).fetch_optional(&mut *c).await?.ok_or_else(||drift("required relation missing"))?;
        let kind: String = row.try_get("kind")?;
        if !["r", "p"].contains(&kind.as_str()) {
            return Err(drift("required relation incompatible"));
        }
        for operation in requirement.operations {
            let allowed: bool =
                sqlx::query_scalar("SELECT has_table_privilege(to_regclass($1),$2)")
                    .bind(table)
                    .bind(*operation)
                    .fetch_one(&mut *c)
                    .await?;
            if !allowed {
                return Err(drift(&format!(
                    "role {role:?} requires {operation} on {table}"
                )));
            }
        }
        signatures.push(CatalogSignature {
            object: table.into(),
            signature: format!(
                "{}:{kind}:{}",
                row.try_get::<i64, _>("oid")?,
                requirement.operations.join(",")
            ),
        });
    }
    let manifest = manifest();
    for node in manifest.nodes.iter().skip(1) {
        for column in &node.columns {
            let row=sqlx::query("SELECT format_type(a.atttypid,a.atttypmod) AS typ,a.attnotnull FROM pg_attribute a WHERE a.attrelid=to_regclass($1) AND a.attname=$2 AND NOT a.attisdropped")
                .bind(column.table).bind(column.name).fetch_optional(&mut *c).await?;
            let required = last
                >= match node.id {
                    "proxy.started_pending.v1" => 164,
                    "history.baseline_receipt.v1" => 165,
                    "oauth.rejected_generation.v1" => 166,
                    _ => return Err(drift("unknown capability node")),
                };
            if required {
                let row = row.ok_or_else(|| drift("required capability column missing"))?;
                if row.try_get::<String, _>("typ")? != column.postgres_type
                    || row.try_get::<bool, _>("attnotnull")? == column.nullable
                {
                    return Err(drift("column signature differs"));
                }
                signatures.push(CatalogSignature {
                    object: format!("{}.{}", column.table, column.name),
                    signature: format!("{}:nullable={}", column.postgres_type, column.nullable),
                });
            } else if row.is_some() {
                return Err(drift("unregistered capability column; provenance required"));
            }
        }
    }
    if last >= 164 {
        crate::db::migrations::validate_started_capability(c).await?;
    }
    let identity:String=sqlx::query_scalar("SELECT current_database()||':'||(SELECT oid::text FROM pg_database WHERE datname=current_database())||':'||current_schema()").fetch_one(&mut *c).await?;
    Ok(Some(CapabilityReport {
        manifest_digest: manifest
            .digest()
            .map_err(|_| drift("manifest serialization"))?,
        database_identity: identity,
        role,
        track,
        object_signatures: signatures,
        available_capabilities: {
            let mut ids = vec!["schema.legacy163.v1".to_owned()];
            if last >= 164 {
                ids.push("proxy.started_pending.v1".to_owned());
            }
            if last >= 165 {
                ids.push("history.baseline_receipt.v1".to_owned());
            }
            if last >= 166 {
                ids.push("oauth.rejected_generation.v1".to_owned());
            }
            ids
        },
        verified_at: crate::db::unix_millis(),
    }))
}
impl Database {
    pub(crate) fn install_schema_track(&self, track: SchemaTrack) -> Result<(), sqlx::Error> {
        if let Some(existing) = self.schema_track.get() {
            if *existing != track {
                return Err(drift("verified track changed; new runtime required"));
            }
            return Ok(());
        }
        self.schema_track
            .set(track)
            .map_err(|_| drift("concurrent schema verification; retry initialization"))
    }

    pub(crate) fn legacy163_mode(&self) -> Result<bool, sqlx::Error> {
        self.schema_track
            .get()
            .map(|track| *track == SchemaTrack::Legacy163)
            .ok_or_else(|| drift("runtime schema capability has not been verified"))
    }

    /// Explicit compatibility startup. This never substitutes for `migrate`:
    /// operators requesting a linear upgrade retain the original resume path.
    pub async fn validate_legacy163_startup(
        &self,
        role: CapabilityRole,
    ) -> Result<CapabilityReport, sqlx::Error> {
        let report = self
            .verify_capabilities(role)
            .await?
            .ok_or_else(|| drift("legacy startup requires an existing verified database"))?;
        if report.track != SchemaTrack::Legacy163 {
            return Err(drift("legacy startup requires exact163"));
        }
        Ok(report)
    }

    pub async fn verify_startup_capabilities(
        &self,
        role: CapabilityRole,
    ) -> Result<Option<CapabilityReport>, sqlx::Error> {
        let report = self.verify_capabilities(role).await?;
        if matches!(self.backend, DatabaseBackend::PostgreSql) && report.is_none() {
            return Err(drift(
                "PostgreSQL startup requires a proven supported schema",
            ));
        }
        Ok(report)
    }

    pub async fn verify_capabilities(
        &self,
        role: CapabilityRole,
    ) -> Result<Option<CapabilityReport>, sqlx::Error> {
        if !matches!(self.backend, DatabaseBackend::PostgreSql) {
            if !self.verify_sqlite_runtime().await? {
                return Err(drift(
                    "SQLite runtime requires completed supported migrations",
                ));
            }
            return Ok(None);
        }
        let mut c = self.pool.acquire().await?;
        let mut tx = c.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
        let report = inspect(&mut tx, role).await?;
        tx.commit().await?;
        if let Some(report) = &report {
            self.install_schema_track(report.track)?;
        }
        Ok(report)
    }
}
