//! Post-commit activation coverage; request producers never call this module.
use super::*;

pub(super) const ACTIVATION_PROOF_SQL: &str = "SELECT phase='legacy' OR (phase='drained' AND EXISTS (SELECT 1 FROM request_history_baseline_runs r WHERE r.owner=request_history_activation.baseline_owner AND r.epoch=request_history_activation.baseline_epoch AND r.status='source_complete')) FROM request_history_activation WHERE id=1";

impl Database {
    pub(super) async fn history_require_unprepared(
        transaction: &mut Transaction<'_, Any>,
    ) -> Result<(), HistoryError> {
        let maximum: String = sqlx::query_scalar("SHOW max_prepared_transactions")
            .fetch_one(&mut **transaction)
            .await?;
        if maximum != "0" {
            return Err(HistoryError::Invalid(
                "activation horizon requires prepared transactions disabled",
            ));
        }
        Ok(())
    }

    pub(super) async fn history_mark_activation(
        transaction: &mut Transaction<'_, Any>,
    ) -> Result<(), HistoryError> {
        sqlx::query("UPDATE request_history_activation SET epoch=epoch+1,phase='pending',transactions_json=NULL,baseline_owner=NULL,baseline_epoch=NULL WHERE id=1")
            .execute(&mut **transaction).await?;
        Ok(())
    }

    /// One inspection, never a wait on request locks. A pending cohort survives
    /// process loss. If no cohort was durably recorded after activation COMMIT,
    /// a new post-commit cohort is conservative, including no-XID RR readers.
    pub(crate) async fn history_capture_horizon_ready(&self) -> Result<bool, HistoryError> {
        if matches!(self.backend, DatabaseBackend::Sqlite) {
            return Ok(true);
        }
        let mut transaction = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
            .execute(&mut *transaction)
            .await?;
        let row=sqlx::query("SELECT phase,transactions_json FROM request_history_activation WHERE id=1 FOR UPDATE NOWAIT")
            .fetch_one(&mut *transaction).await?;
        let phase: String = row.try_get("phase")?;
        if phase == "legacy" || phase == "drained" {
            return Ok(true);
        }
        let mode: String = sqlx::query_scalar("SELECT mode FROM request_history_state WHERE id=1")
            .fetch_one(&mut *transaction)
            .await?;
        if mode != "shadow" {
            return Err(HistoryError::Changed);
        }
        Self::history_require_unprepared(&mut transaction).await?;
        let cohort: Vec<Value> = if phase == "pending" {
            let rows=sqlx::query("SELECT a.pid::BIGINT AS pid,a.backend_start::TEXT AS backend_start,l.virtualxid::TEXT AS virtualxid FROM pg_locks l JOIN pg_stat_activity a ON a.pid=l.pid WHERE l.locktype='virtualxid' AND l.mode='ExclusiveLock' AND l.granted AND a.datid=(SELECT oid FROM pg_database WHERE datname=current_database()) AND a.pid<>pg_backend_pid() ORDER BY a.pid")
                .fetch_all(&mut *transaction).await?;
            rows.into_iter().map(|row| -> Result<Value,sqlx::Error> {Ok(serde_json::json!({"pid":row.try_get::<i64,_>("pid")?,"backend_start":row.try_get::<String,_>("backend_start")?,"virtualxid":row.try_get::<String,_>("virtualxid")?}))}).collect::<Result<_,_>>()?
        } else {
            serde_json::from_str(&row.try_get::<String, _>("transactions_json")?)?
        };
        let mut pending = Vec::new();
        for identity in cohort {
            let active:bool=sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_locks l JOIN pg_stat_activity a ON a.pid=l.pid WHERE l.locktype='virtualxid' AND l.mode='ExclusiveLock' AND l.granted AND a.pid::BIGINT=$1 AND a.backend_start=$2::timestamptz AND l.virtualxid::TEXT=$3)")
                .bind(identity["pid"].as_i64().ok_or(HistoryError::Mismatch)?)
                .bind(identity["backend_start"].as_str().ok_or(HistoryError::Mismatch)?)
                .bind(identity["virtualxid"].as_str().ok_or(HistoryError::Mismatch)?)
                .fetch_one(&mut *transaction).await?;
            if active {
                pending.push(identity);
            }
        }
        let ready = pending.is_empty();
        sqlx::query(
            "UPDATE request_history_activation SET phase=$1,transactions_json=$2 WHERE id=1",
        )
        .bind(if ready { "drained" } else { "observing" })
        .bind(serde_json::to_string(&pending)?)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(ready)
    }

    pub(crate) async fn history_activation_baseline_proved(&self) -> Result<bool, HistoryError> {
        if matches!(self.backend, DatabaseBackend::Sqlite) {
            return Ok(true);
        }
        Ok(sqlx::query_scalar(ACTIVATION_PROOF_SQL)
            .fetch_one(&self.pool)
            .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn insert_event(tx: &mut Transaction<'_, Any>, event: Uuid) {
        sqlx::query("INSERT INTO request_events (event_id,tenant_id,key_id,request_id,event_at,event_kind,protocol,model,status_code,input_tokens,output_tokens,cost_micros) VALUES ($1,$2,$3,$4,42,'horizon','openai','fixture',200,0,0,0)")
            .bind(event.to_string()).bind(Uuid::now_v7().to_string()).bind(Uuid::now_v7().to_string()).bind(Uuid::now_v7().to_string()).execute(&mut **tx).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "existing CI supplies the real PostgreSQL service"]
    async fn request_history_real_services_acceptance_activation_horizon_covers_old_no_xid_snapshots()
     {
        let postgres = std::env::var("MTC_TEST_POSTGRES_URL").unwrap();
        let admin = sqlx::PgPool::connect(&postgres).await.unwrap();
        let schema = format!("history_activation_{}", Uuid::now_v7().simple());
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await
            .unwrap();
        let mut url = url::Url::parse(&postgres).unwrap();
        url.query_pairs_mut()
            .append_pair("options", &format!("-csearch_path={schema}"));
        let database = Database::connect_with_max(url.as_str(), 1).await.unwrap();
        database.migrate().await.unwrap();
        database.history_bootstrap_capture().await.unwrap();
        // Simulate the known deployed v1 functions; trigger/partition OIDs stay
        // unchanged when PrepareActivation replaces only function bodies.
        for definition in database
            .history_capture_definitions_with_projection_guards(false, false)
            .unwrap()
        {
            let old=definition.body.replacen("BEGIN IF (SELECT mode FROM request_history_state WHERE id=1)='off' THEN RETURN NULL; END IF; ","BEGIN ",1);
            let manifest = format!(
                "mtc-history-bootstrap-v1-{}",
                crate::request_history::canonical_hash(&(
                    1,
                    definition.table,
                    definition.event,
                    &old
                ))
                .unwrap()
            );
            sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE OR REPLACE FUNCTION {}() RETURNS trigger LANGUAGE plpgsql VOLATILE SECURITY INVOKER AS $history${old}$history$; COMMENT ON FUNCTION {}() IS '{manifest}';",definition.name,definition.name))).execute(&database.pool).await.unwrap();
        }
        let roots_before:Vec<i64>=sqlx::query_scalar("SELECT t.oid::BIGINT FROM pg_trigger t JOIN pg_class c ON c.oid=t.tgrelid WHERE c.relnamespace=to_regnamespace(current_schema()) AND t.tgname LIKE 'history_%' ORDER BY t.oid").fetch_all(&database.pool).await.unwrap();
        let writers = Database::connect_with_max(url.as_str(), 2).await.unwrap();
        let mut rr = writers.pool.begin().await.unwrap();
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
            .execute(&mut *rr)
            .await
            .unwrap();
        let mode: String = sqlx::query_scalar("SELECT mode FROM request_history_state WHERE id=1")
            .fetch_one(&mut *rr)
            .await
            .unwrap();
        assert_eq!(mode, "off");
        let xid: Option<String> =
            sqlx::query_scalar("SELECT pg_current_xact_id_if_assigned()::TEXT")
                .fetch_one(&mut *rr)
                .await
                .unwrap();
        assert!(xid.is_none());
        database.history_prepare_activation().await.unwrap();
        database.history_prepare_activation().await.unwrap();
        let roots_after:Vec<i64>=sqlx::query_scalar("SELECT t.oid::BIGINT FROM pg_trigger t JOIN pg_class c ON c.oid=t.tgrelid WHERE c.relnamespace=to_regnamespace(current_schema()) AND t.tgname LIKE 'history_%' ORDER BY t.oid").fetch_all(&database.pool).await.unwrap();
        assert_eq!(roots_before, roots_after);
        let mut rc = writers.pool.begin().await.unwrap();
        let rc_event = Uuid::now_v7();
        insert_event(&mut rc, rc_event).await;
        database.history_activate_capture().await.unwrap();
        assert_eq!(database.history_mode().await.unwrap(), "shadow");
        assert!(!database.history_capture_horizon_ready().await.unwrap());
        assert!(database.history_backfill_page(64).await.is_err());
        assert!(database.history_prepare_activation().await.is_err());
        let rr_event = Uuid::now_v7();
        insert_event(&mut rr, rr_event).await;
        let pending: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM request_history_export WHERE source='event'")
                .fetch_one(&database.pool)
                .await
                .unwrap();
        assert_eq!(
            pending, 0,
            "both old-snapshot/off writes need subsequent coverage"
        );
        rr.commit().await.unwrap();
        assert!(!database.history_capture_horizon_ready().await.unwrap());
        rc.commit().await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !database.history_capture_horizon_ready().await.unwrap() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        while !database.history_backfill_done().await.unwrap() {
            database.history_backfill_page(64).await.unwrap();
        }
        let captured:i64=sqlx::query_scalar("SELECT COUNT(*) FROM request_history_export WHERE source='event' AND request_id IN ($1,$2)").bind(rc_event.to_string()).bind(rr_event.to_string()).fetch_one(&database.pool).await.unwrap();
        assert_eq!(captured, 2);
        assert!(
            database.history_switch(true).await.is_err(),
            "horizon alone is not baseline or cutover proof"
        );
        let source = Database::connect_with_max(url.as_str(), 1).await.unwrap();
        let owner = Uuid::now_v7();
        let snapshot = database
            .history_begin_source_snapshot(&source, owner)
            .await
            .unwrap();
        let epoch = snapshot.receipt["epoch"].as_i64().unwrap();
        let mut tx = snapshot.transaction;
        let covered: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM request_events WHERE event_id IN ($1,$2)")
                .bind(rc_event.to_string())
                .bind(rr_event.to_string())
                .fetch_one(&mut *tx)
                .await
                .unwrap();
        assert_eq!(
            covered, 2,
            "a fresh RR after natural drain includes both late commits"
        );
        tx.rollback().await.unwrap();
        database
            .history_abort_source_snapshot(owner, epoch)
            .await
            .unwrap();
        source.close().await;
        writers.close().await;
        database.close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(&admin)
            .await
            .unwrap();
        admin.close().await;
    }
}
