use super::*;

struct Index {
    name: &'static str,
    table: &'static str,
    columns: &'static str,
    statement: &'static str,
}

const INDEXES: [Index; 2] = [
    Index {
        name: "conversation_observations_explicit_session_idx",
        table: "conversation_observations",
        columns: "explicit_session_id, key_id, cluster_id",
        statement: "CREATE INDEX conversation_observations_explicit_session_idx\n    ON conversation_observations (explicit_session_id, key_id, cluster_id);",
    },
    Index {
        name: "request_stats_facts_session_idx",
        table: "request_stats_facts",
        columns: "tenant_id, key_id, session_id, created_at, request_id",
        statement: "CREATE INDEX request_stats_facts_session_idx\n    ON request_stats_facts (tenant_id, key_id, session_id, created_at, request_id);",
    },
];

struct Status {
    correct: bool,
    usable: bool,
    qualified_name: String,
}

async fn inspect(
    connection: &mut AnyConnection,
    index: &Index,
) -> Result<Option<Status>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT quote_ident(n.nspname) || '.' || quote_ident(c.relname) AS qualified_name,
            COALESCE(i.indisvalid AND i.indisready AND i.indislive, false) AS usable,
            COALESCE(i.indrelid = to_regclass($1) AND am.amname = 'btree'
                AND NOT i.indisunique AND NOT i.indisprimary AND NOT i.indisexclusion
                AND i.indpred IS NULL AND i.indexprs IS NULL
                AND i.indnatts = i.indnkeyatts AND c.reloptions IS NULL
                AND (SELECT string_agg(pg_get_indexdef(c.oid, position, true), ', ' ORDER BY position)
                    FROM generate_series(1, i.indnkeyatts) position) = $3, false) AS correct
        FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
        LEFT JOIN pg_index i ON i.indexrelid = c.oid
        LEFT JOIN pg_am am ON am.oid = c.relam
        WHERE c.relname = $2 AND c.relnamespace =
            (SELECT relnamespace FROM pg_class WHERE oid = to_regclass($1))",
    )
    .bind(index.table)
    .bind(index.name)
    .bind(index.columns)
    .fetch_optional(connection)
    .await?;
    row.map(|row| {
        Ok(Status {
            correct: row.try_get("correct")?,
            usable: row.try_get("usable")?,
            qualified_name: row.try_get("qualified_name")?,
        })
    })
    .transpose()
}

fn conflict(index: &Index) -> sqlx::Error {
    sqlx::Error::Protocol(format!(
        "migration147 index {} has an incompatible or unprepared catalog definition",
        index.name
    ))
}

pub(super) async fn prepare(connection: &mut AnyConnection) -> Result<(), sqlx::Error> {
    let ledger_exists: bool =
        sqlx::query_scalar("SELECT to_regclass('schema_migrations') IS NOT NULL")
            .fetch_one(&mut *connection)
            .await?;
    if !ledger_exists {
        return Ok(());
    }
    let applied = sqlx::query("SELECT version FROM schema_migrations WHERE version = 147")
        .fetch_optional(&mut *connection)
        .await?;
    if applied.is_some() {
        return Ok(());
    }
    for index in &INDEXES {
        let exists: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
            .bind(index.table)
            .fetch_one(&mut *connection)
            .await?;
        if !exists {
            return Ok(());
        }
    }
    for index in &INDEXES {
        match inspect(connection, index).await? {
            Some(status) if !status.correct => return Err(conflict(index)),
            Some(status) if status.usable => continue,
            Some(status) => {
                sqlx::query(sqlx::AssertSqlSafe(format!(
                    "DROP INDEX CONCURRENTLY {}",
                    status.qualified_name
                )))
                .execute(&mut *connection)
                .await?;
            }
            None => {}
        }
        sqlx::query(sqlx::AssertSqlSafe(index.statement.replacen(
            "CREATE INDEX",
            "CREATE INDEX CONCURRENTLY",
            1,
        )))
        .execute(&mut *connection)
        .await?;
        if !inspect(connection, index)
            .await?
            .is_some_and(|status| status.correct && status.usable)
        {
            return Err(conflict(index));
        }
    }
    Ok(())
}

pub(super) async fn apply(connection: &mut AnyConnection, source: &str) -> Result<(), sqlx::Error> {
    let mut sql = source.to_owned();
    for index in &INDEXES {
        if !source.contains(index.statement) {
            return Err(conflict(index));
        }
        match inspect(connection, index).await? {
            Some(status) if status.correct && status.usable => {
                sql = sql.replace(index.statement, "")
            }
            Some(_) => return Err(conflict(index)),
            None => {}
        }
    }
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
        .execute(connection)
        .await?;
    Ok(())
}
