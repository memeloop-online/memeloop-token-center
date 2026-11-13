//! Off-mode bootstrap: one short table transaction, then a separate state flip.
//! PostgreSQL catalogs are the durable version manifest; no new schema is needed.
use super::*;

const MAINTENANCE_LOCK: i64 = 5569905361512744002;

impl Database {
    pub(super) async fn history_bootstrap_maintenance_lock(
        transaction: &mut Transaction<'_, Any>,
    ) -> Result<(), HistoryError> {
        let acquired: bool = sqlx::query_scalar(
            "SELECT pg_try_advisory_xact_lock(hashtextextended(current_schema(),$1))",
        )
        .bind(MAINTENANCE_LOCK)
        .fetch_one(&mut **transaction)
        .await?;
        if !acquired {
            return Err(HistoryError::Changed);
        }
        Ok(())
    }

    pub async fn history_bootstrap_capture(&self) -> Result<Value, HistoryError> {
        if matches!(self.backend, DatabaseBackend::Sqlite) {
            return Err(HistoryError::Invalid(
                "incremental capture bootstrap requires PostgreSQL",
            ));
        }
        let definitions = self.history_capture_definitions()?;
        let mut installed = Vec::new();
        let mut pending = Vec::new();
        for group in definitions.chunks(3) {
            let table = group[0].table;
            let result: Result<(), HistoryError> = async {
                let mut transaction = self.pool.begin().await?;
                // Table first: do not hold the state row while waiting for a
                // live writer. Every contested table fails immediately.
                sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
                    "LOCK TABLE {table} IN SHARE ROW EXCLUSIVE MODE NOWAIT"
                )))
                .execute(&mut *transaction)
                .await?;
                Self::history_bootstrap_maintenance_lock(&mut transaction).await?;
                let mode: String = sqlx::query_scalar(
                    "SELECT mode FROM request_history_state WHERE id=1 FOR SHARE NOWAIT",
                )
                .fetch_one(&mut *transaction)
                .await?;
                if mode != "off" {
                    return Err(HistoryError::Changed);
                }
                for definition in group {
                    if !definition.matches(&mut transaction).await? {
                        sqlx::raw_sql(sqlx::AssertSqlSafe(definition.sql.clone()))
                            .execute(&mut *transaction)
                            .await?;
                    }
                    if !definition.matches(&mut transaction).await? {
                        return Err(HistoryError::Verification("capture catalog definition"));
                    }
                }
                transaction.commit().await?;
                Ok(())
            }
            .await;
            match result {
                Ok(()) => installed.push(table),
                Err(HistoryError::Sql(error))
                    if error
                        .as_database_error()
                        .and_then(|error| error.code())
                        .as_deref()
                        == Some("55P03") =>
                {
                    pending.push(table)
                }
                Err(error) => return Err(error),
            }
        }
        Ok(
            serde_json::json!({"mode":"off","installed":installed,"pending":pending,"capture_version":1}),
        )
    }

    pub async fn history_activate_capture(&self) -> Result<(), HistoryError> {
        if matches!(self.backend, DatabaseBackend::Sqlite) {
            return Err(HistoryError::Invalid(
                "incremental capture bootstrap requires PostgreSQL",
            ));
        }
        let definitions = self.history_capture_definitions()?;
        let mut transaction = self.pool.begin().await?;
        Self::history_bootstrap_maintenance_lock(&mut transaction).await?;
        // The maintenance mutex, not the business state barrier, protects the
        // catalog validation phase from another bootstrap/atomic installer.
        let mode: String = sqlx::query_scalar("SELECT mode FROM request_history_state WHERE id=1")
            .fetch_one(&mut *transaction)
            .await?;
        if mode != "off" {
            return Err(HistoryError::Changed);
        }
        let expected = definitions.len() as i64;
        for definition in definitions {
            if !definition.matches(&mut transaction).await? {
                return Err(HistoryError::Verification(
                    "capture bootstrap incomplete or changed",
                ));
            }
        }
        let actual:i64=sqlx::query_scalar("SELECT COUNT(*) FROM pg_trigger t JOIN pg_class c ON c.oid=t.tgrelid JOIN pg_namespace n ON n.oid=c.relnamespace JOIN pg_proc p ON p.oid=t.tgfoid WHERE n.nspname=current_schema() AND NOT t.tgisinternal AND t.tgname LIKE 'history_%' AND p.proname LIKE 'history_%'")
            .fetch_one(&mut *transaction).await?;
        if actual != expected {
            return Err(HistoryError::Verification(
                "unexpected capture catalog definitions",
            ));
        }
        // Only the final mode recheck/update runs under the business barrier.
        // No per-definition catalog queries remain after this NOWAIT lock.
        let mode: String = sqlx::query_scalar(
            "SELECT mode FROM request_history_state WHERE id=1 FOR UPDATE NOWAIT",
        )
        .fetch_one(&mut *transaction)
        .await?;
        if mode != "off" {
            return Err(HistoryError::Changed);
        }
        // Off-mode triggers already participate in this shared state barrier.
        // No source/DDL/network operation occurs under the exclusive flip.
        sqlx::query("UPDATE request_history_state SET mode='shadow',verified_revision=NULL,verification_json=NULL WHERE id=1")
            .execute(&mut *transaction).await?;
        transaction.commit().await?;
        Ok(())
    }
}

impl CaptureDefinition {
    async fn matches(&self, transaction: &mut Transaction<'_, Any>) -> Result<bool, HistoryError> {
        let trigger_type = match self.event {
            "INSERT" => 5_i64,
            "UPDATE" => 17,
            "DELETE" => 9,
            _ => unreachable!("static capture events"),
        };
        Ok(sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_trigger t JOIN pg_class c ON c.oid=t.tgrelid JOIN pg_namespace n ON n.oid=c.relnamespace JOIN pg_proc p ON p.oid=t.tgfoid JOIN pg_language l ON l.oid=p.prolang WHERE n.nspname=current_schema() AND c.relname=$1 AND t.tgname=$2 AND NOT t.tgisinternal AND t.tgenabled='O' AND t.tgtype::BIGINT=$3 AND t.tgqual IS NULL AND t.tgnargs=0 AND t.tgattr=''::int2vector AND t.tgoldtable IS NULL AND t.tgnewtable IS NULL AND p.pronamespace=n.oid AND p.proname=$2 AND p.prosrc=$4 AND obj_description(p.oid,'pg_proc')=$5 AND NOT p.prosecdef AND p.provolatile='v' AND p.proconfig IS NULL AND p.prorettype='trigger'::regtype AND l.lanname='plpgsql')")
            .bind(self.table).bind(&self.name).bind(trigger_type).bind(&self.body).bind(&self.manifest).fetch_one(&mut **transaction).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn insert_event(
        transaction: &mut Transaction<'_, Any>,
        event: Uuid,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("INSERT INTO request_events (event_id,tenant_id,key_id,request_id,event_at,event_kind,protocol,model,status_code,input_tokens,output_tokens,cost_micros) VALUES ($1,$2,$3,$4,$5,'bootstrap-fixture','openai','fixture',200,0,0,0)")
            .bind(event.to_string()).bind(Uuid::now_v7().to_string()).bind(Uuid::now_v7().to_string()).bind(Uuid::now_v7().to_string()).bind(unix_millis()).execute(&mut **transaction).await?;
        Ok(())
    }

    #[tokio::test]
    #[ignore = "existing CI supplies the real PostgreSQL service"]
    async fn request_history_real_services_acceptance_bootstrap_partial_recovery_and_activation_barrier()
     {
        let postgres = std::env::var("MTC_TEST_POSTGRES_URL").expect("PostgreSQL acceptance URL");
        let admin = sqlx::PgPool::connect(&postgres).await.unwrap();
        let schema = format!("history_bootstrap_{}", Uuid::now_v7().simple());
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await
            .unwrap();
        let mut isolated = url::Url::parse(&postgres).unwrap();
        isolated
            .query_pairs_mut()
            .append_pair("options", &format!("-csearch_path={schema}"));
        let database = Database::connect_with_max(isolated.as_str(), 1)
            .await
            .unwrap();
        database.migrate().await.unwrap();
        let writers = database
            .clone()
            .with_auxiliary_pool_max_connections(1)
            .independent_persistence_pool();
        let mut writer = writers.pool.begin().await.unwrap();
        let preinstall_event = Uuid::now_v7();
        insert_event(&mut writer, preinstall_event).await.unwrap();
        let expected_tables = database.history_capture_definitions().unwrap().len() / 3;
        let partial =
            tokio::time::timeout(Duration::from_secs(5), database.history_bootstrap_capture())
                .await
                .unwrap()
                .unwrap();
        assert_eq!(partial["mode"], "off");
        assert_eq!(partial["pending"], serde_json::json!(["request_events"]));
        assert_eq!(
            partial["installed"].as_array().unwrap().len(),
            expected_tables - 1
        );
        assert_eq!(database.history_mode().await.unwrap(), "off");
        assert!(database.history_activate_capture().await.is_err());
        assert_eq!(
            sqlx::query_scalar::<_, i32>("SELECT 1")
                .fetch_one(&mut *writer)
                .await
                .unwrap(),
            1
        );
        writer.commit().await.unwrap();
        // Persisted partial catalogs survive a CLI/pool restart, without reset.
        let resumed = Database::connect_with_max(isolated.as_str(), 1)
            .await
            .unwrap();
        let complete = resumed.history_bootstrap_capture().await.unwrap();
        assert!(complete["pending"].as_array().unwrap().is_empty());
        assert_eq!(
            complete["installed"].as_array().unwrap().len(),
            expected_tables
        );
        let definitions = resumed.history_capture_definitions().unwrap();
        let mut inspection = resumed.pool.begin().await.unwrap();
        for definition in &definitions {
            assert!(definition.matches(&mut inspection).await.unwrap());
        }
        inspection.rollback().await.unwrap();
        // A matching count is insufficient: corrupt the durable version label.
        sqlx::raw_sql("COMMENT ON FUNCTION history_request_events_insert() IS 'wrong-version'")
            .execute(&resumed.pool)
            .await
            .unwrap();
        assert!(resumed.history_activate_capture().await.is_err());
        assert_eq!(resumed.history_mode().await.unwrap(), "off");
        resumed.history_bootstrap_capture().await.unwrap();
        // Concurrent maintenance is rejected, without leaving table locks that
        // block a regular source INSERT. Source triggers never use this mutex.
        let mut maintenance = writers.pool.begin().await.unwrap();
        Database::history_bootstrap_maintenance_lock(&mut maintenance)
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(3), resumed.history_bootstrap_capture())
                .await
                .unwrap()
                .is_err()
        );
        maintenance.rollback().await.unwrap();
        let old_event = Uuid::now_v7();
        let mut old_writer = writers.pool.begin().await.unwrap();
        insert_event(&mut old_writer, old_event).await.unwrap();
        assert_eq!(
            resumed.history_pending_count().await.unwrap(),
            0,
            "off-mode trigger does not enqueue"
        );
        let error =
            tokio::time::timeout(Duration::from_secs(3), resumed.history_activate_capture())
                .await
                .unwrap()
                .unwrap_err();
        let HistoryError::Sql(error) = error else {
            panic!("activation must fail on the still-live off writer");
        };
        assert_eq!(
            error.as_database_error().unwrap().code().as_deref(),
            Some("55P03")
        );
        assert_eq!(
            sqlx::query_scalar::<_, i32>("SELECT 1")
                .fetch_one(&mut *old_writer)
                .await
                .unwrap(),
            1
        );
        old_writer.commit().await.unwrap();
        resumed.history_activate_capture().await.unwrap();
        assert_eq!(resumed.history_mode().await.unwrap(), "shadow");
        let new_event = Uuid::now_v7();
        let mut new_writer = writers.pool.begin().await.unwrap();
        insert_event(&mut new_writer, new_event).await.unwrap();
        new_writer.commit().await.unwrap();
        assert_eq!(resumed.history_pending_count().await.unwrap(), 1);
        while !resumed.history_backfill_done().await.unwrap() {
            resumed.history_backfill_page(64).await.unwrap();
        }
        let captured:i64=sqlx::query_scalar("SELECT COUNT(*) FROM request_history_export WHERE source='event' AND request_id IN ($1,$2)")
            .bind(old_event.to_string()).bind(new_event.to_string()).fetch_one(&resumed.pool).await.unwrap();
        assert_eq!(
            captured, 2,
            "backfill includes pre-activation commits, live capture includes post-activation writes"
        );
        let preinstall_captured: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM request_history_export WHERE source='event' AND request_id=$1",
        )
        .bind(preinstall_event.to_string())
        .fetch_one(&resumed.pool)
        .await
        .unwrap();
        assert_eq!(
            preinstall_captured, 1,
            "untrapped writers commit before their table can be bootstrapped, then backfill covers them"
        );
        assert!(
            resumed.history_bootstrap_capture().await.is_err(),
            "live capture cannot drop/reinstall triggers"
        );
        let mut inspection = resumed.pool.begin().await.unwrap();
        for definition in definitions {
            assert!(definition.matches(&mut inspection).await.unwrap());
        }
        inspection.rollback().await.unwrap();
        resumed.close().await;
        writers.close().await;
        database.close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(&admin)
            .await
            .unwrap();
        admin.close().await;
    }
}
