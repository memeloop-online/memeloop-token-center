use super::*;

const SOURCE: &str = include_str!("../../../migrations/common/0164_proxy_started_obligation.sql");
fn conflict() -> sqlx::Error {
    sqlx::Error::Protocol("migration164 incompatible or incomplete catalog".into())
}
async fn column_ok(c: &mut AnyConnection) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_attribute a JOIN pg_attrdef d ON d.adrelid=a.attrelid AND d.adnum=a.attnum WHERE a.attrelid='request_records'::regclass AND a.attname='started_event_pending' AND NOT a.attisdropped AND a.atttypid='bigint'::regtype AND a.attnotnull AND pg_get_expr(d.adbin,d.adrelid) IN ('0','0::bigint'))")
        .fetch_one(c).await
}
async fn index_definition_ok(
    c: &mut AnyConnection,
    table: &str,
    name: &str,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_index i JOIN pg_class x ON x.oid=i.indexrelid JOIN pg_am am ON am.oid=x.relam WHERE i.indrelid=to_regclass($1) AND i.indexrelid=to_regclass($2) AND NOT EXISTS(SELECT 1 FROM pg_constraint con WHERE con.conindid=i.indexrelid) AND NOT i.indisunique AND NOT i.indisprimary AND am.amname='btree' AND i.indnkeyatts=2 AND i.indnatts=2 AND i.indexprs IS NULL AND pg_get_indexdef(i.indexrelid,1,true)='created_at' AND pg_get_indexdef(i.indexrelid,2,true)='id' AND pg_get_expr(i.indpred,i.indrelid)='(started_event_pending = 1)')")
        .bind(table).bind(name).fetch_one(c).await
}
async fn index_ok(c: &mut AnyConnection, table: &str, name: &str) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_index i JOIN pg_class x ON x.oid=i.indexrelid JOIN pg_am am ON am.oid=x.relam WHERE i.indrelid=to_regclass($1) AND i.indexrelid=to_regclass($2) AND i.indisvalid AND i.indisready AND i.indislive AND NOT i.indisunique AND NOT i.indisprimary AND am.amname='btree' AND i.indnkeyatts=2 AND i.indnatts=2 AND i.indexprs IS NULL AND pg_get_indexdef(i.indexrelid,1,true)='created_at' AND pg_get_indexdef(i.indexrelid,2,true)='id' AND pg_get_expr(i.indpred,i.indrelid)='(started_event_pending = 1)')")
        .bind(table).bind(name).fetch_one(c).await
}
async fn leaves(c: &mut AnyConnection) -> Result<Vec<(String, String)>, sqlx::Error> {
    let rows=sqlx::query("SELECT quote_ident(n.nspname)||'.'||quote_ident(p.relname) AS relation, quote_ident(n.nspname)||'.'||quote_ident('mtc164_'||p.oid::text) AS index_name FROM pg_partition_tree('request_records') t JOIN pg_class p ON p.oid=t.relid JOIN pg_namespace n ON n.oid=p.relnamespace WHERE t.isleaf ORDER BY p.oid")
        .fetch_all(c).await?;
    rows.into_iter()
        .map(|r| Ok((r.try_get("relation")?, r.try_get("index_name")?)))
        .collect()
}
pub(super) async fn prepare(c: &mut AnyConnection) -> Result<(), sqlx::Error> {
    let ledger: bool = sqlx::query_scalar("SELECT to_regclass('schema_migrations') IS NOT NULL")
        .fetch_one(&mut *c)
        .await?;
    if !ledger {
        return Ok(());
    }
    let ready: bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM schema_migrations WHERE version=163) AND NOT EXISTS(SELECT 1 FROM schema_migrations WHERE version=164)").fetch_one(&mut *c).await?;
    if !ready {
        return Ok(());
    }
    if !column_ok(c).await? {
        let mut tx = c.begin().await?;
        // Recursive lock acquisition is nonblocking, including DEFAULT and every
        // leaf. An occupied leaf never queues an exclusive parent behind readers.
        sqlx::query("LOCK TABLE request_records IN ACCESS EXCLUSIVE MODE NOWAIT")
            .execute(&mut *tx)
            .await?;
        let exists: bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_attribute WHERE attrelid='request_records'::regclass AND attname='started_event_pending' AND NOT attisdropped)").fetch_one(&mut *tx).await?;
        if !exists {
            sqlx::query("ALTER TABLE request_records ADD COLUMN started_event_pending BIGINT NOT NULL DEFAULT 0").execute(&mut *tx).await?;
        }
        if !column_ok(&mut tx).await? {
            return Err(conflict());
        }
        tx.commit().await?;
    }
    let partitioned: bool = sqlx::query_scalar(
        "SELECT relkind='p' FROM pg_class WHERE oid='request_records'::regclass",
    )
    .fetch_one(&mut *c)
    .await?;
    if !partitioned {
        return Err(conflict());
    }
    for (table, index) in leaves(c).await? {
        if index_ok(c, &table, &index).await? {
            continue;
        }
        let exists: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
            .bind(&index)
            .fetch_one(&mut *c)
            .await?;
        if exists {
            let invalid: bool=sqlx::query_scalar("SELECT NOT indisvalid FROM pg_index WHERE indexrelid=to_regclass($1) AND indrelid=to_regclass($2)").bind(&index).bind(&table).fetch_optional(&mut *c).await?.unwrap_or(false);
            let detached: bool = sqlx::query_scalar(
                "SELECT NOT EXISTS(SELECT 1 FROM pg_inherits WHERE inhrelid=to_regclass($1))",
            )
            .bind(&index)
            .fetch_one(&mut *c)
            .await?;
            if !invalid || !detached || !index_definition_ok(c, &table, &index).await? {
                return Err(conflict());
            }
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "DROP INDEX CONCURRENTLY {index}"
            )))
            .execute(&mut *c)
            .await?;
        }
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "CREATE INDEX CONCURRENTLY {} ON {table} (created_at,id) WHERE started_event_pending=1",
            index.rsplit('.').next().ok_or_else(conflict)?
        )))
        .execute(&mut *c)
        .await?;
        if !index_ok(c, &table, &index).await? {
            return Err(conflict());
        }
    }
    // Parent creation/attachment is catalog-only; bound every lock wait.
    let mut tx = c.begin().await?;
    sqlx::query("SET LOCAL statement_timeout='2s'")
        .execute(&mut *tx)
        .await?;
    let attach_started = std::time::Instant::now();
    sqlx::query("SET LOCAL lock_timeout='250ms'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS request_records_started_pending ON ONLY request_records (created_at,id) WHERE started_event_pending=1").execute(&mut *tx).await?;
    for (table, index) in leaves(&mut tx).await? {
        if attach_started.elapsed() >= std::time::Duration::from_secs(2) {
            return Err(conflict());
        }
        if !index_ok(&mut tx, &table, &index).await? {
            return Err(conflict());
        }
        let attached: bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_inherits WHERE inhparent='request_records_started_pending'::regclass AND inhrelid=to_regclass($1))").bind(&index).fetch_one(&mut *tx).await?;
        if !attached {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "ALTER INDEX request_records_started_pending ATTACH PARTITION {index}"
            )))
            .execute(&mut *tx)
            .await?;
        }
    }
    verify(&mut tx).await?;
    tx.commit().await?;
    Ok(())
}
async fn verify(c: &mut AnyConnection) -> Result<(), sqlx::Error> {
    if !column_ok(c).await?
        || !index_ok(c, "request_records", "request_records_started_pending").await?
    {
        return Err(conflict());
    }
    let covered: bool=sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM pg_partition_tree('request_records') t WHERE t.isleaf AND NOT EXISTS(SELECT 1 FROM pg_index i JOIN pg_inherits h ON h.inhrelid=i.indexrelid WHERE i.indrelid=t.relid AND h.inhparent='request_records_started_pending'::regclass AND i.indisvalid AND i.indisready))").fetch_one(c).await?;
    if !covered {
        return Err(conflict());
    }
    Ok(())
}
pub(super) async fn apply(c: &mut AnyConnection, source: &str) -> Result<(), sqlx::Error> {
    if source != SOURCE {
        return Err(conflict());
    }
    let exists: bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_attribute WHERE attrelid='request_records'::regclass AND attname='started_event_pending' AND NOT attisdropped)").fetch_one(&mut *c).await?;
    if !exists {
        sqlx::raw_sql(SOURCE).execute(&mut *c).await?;
    }
    verify(c).await
}
