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

    /// Upgrade only a known off-mode function body. Trigger OIDs, partition
    /// clones and source table locks are untouched; unknown catalogs fail closed.
    pub async fn history_prepare_activation(&self) -> Result<(), HistoryError> {
        if !matches!(self.backend, DatabaseBackend::PostgreSql) {
            return Err(HistoryError::Invalid(
                "activation preparation requires PostgreSQL",
            ));
        }
        let mut transaction = self.pool.begin().await?;
        Self::history_bootstrap_maintenance_lock(&mut transaction).await?;
        let mode: String = sqlx::query_scalar(
            "SELECT mode FROM request_history_state WHERE id=1 FOR SHARE NOWAIT",
        )
        .fetch_one(&mut *transaction)
        .await?;
        if mode != "off" {
            return Err(HistoryError::Changed);
        }
        Self::history_require_unprepared(&mut transaction).await?;
        let previous_definitions =
            self.history_capture_definitions_with_projection_guards(false, false)?;
        for definition in self.history_capture_definitions()? {
            if definition.matches(&mut transaction).await? {
                continue;
            }
            let previous = previous_definitions
                .iter()
                .find(|old| old.name == definition.name)
                .ok_or(HistoryError::Mismatch)?;
            let old_body=previous.body.replacen("BEGIN IF (SELECT mode FROM request_history_state WHERE id=1)='off' THEN RETURN NULL; END IF; ","BEGIN ",1);
            let old = CaptureDefinition {
                table: definition.table,
                event: definition.event,
                name: definition.name.clone(),
                manifest: format!(
                    "mtc-history-bootstrap-v1-{}",
                    crate::request_history::canonical_hash(&(
                        1,
                        definition.table,
                        definition.event,
                        &old_body
                    ))?
                ),
                body: old_body,
                sql: String::new(),
            };
            if !previous.matches(&mut transaction).await? && !old.matches(&mut transaction).await? {
                return Err(HistoryError::Verification(
                    "unknown activation function catalog",
                ));
            }
            let sql = format!(
                "CREATE OR REPLACE FUNCTION {}() RETURNS trigger LANGUAGE plpgsql VOLATILE SECURITY INVOKER AS $history${}$history$; COMMENT ON FUNCTION {}() IS '{}';",
                definition.name, definition.body, definition.name, definition.manifest
            );
            sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
                .execute(&mut *transaction)
                .await?;
            if !definition.matches(&mut transaction).await? {
                return Err(HistoryError::Verification("activation function upgrade"));
            }
        }
        transaction.commit().await?;
        Ok(())
    }

    /// Replace only the known upstream UPDATE function. This changes no
    /// projected business value, capture epoch, source trigger or pending work.
    pub async fn history_upgrade_upstream_capture(&self) -> Result<Value, HistoryError> {
        if !matches!(self.backend, DatabaseBackend::PostgreSql) {
            return Err(HistoryError::Invalid(
                "upstream capture upgrade requires PostgreSQL",
            ));
        }
        let mut tx = self.pool.begin().await?;
        Self::history_bootstrap_maintenance_lock(&mut tx).await?;
        let mode: String = sqlx::query_scalar(
            "SELECT mode FROM request_history_state WHERE id=1 FOR SHARE NOWAIT",
        )
        .fetch_one(&mut *tx)
        .await?;
        let next = self
            .history_capture_definitions()?
            .into_iter()
            .find(|d| d.table == "upstream_accounts" && d.event == "UPDATE")
            .ok_or(HistoryError::Mismatch)?;
        if !next.matches(&mut tx).await? {
            let old = self
                .history_capture_definitions_with_upstream_projection(false)?
                .into_iter()
                .find(|d| d.name == next.name)
                .ok_or(HistoryError::Mismatch)?;
            if !old.matches(&mut tx).await? {
                return Err(HistoryError::Verification(
                    "unknown upstream update capture function",
                ));
            }
            sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
                "CREATE OR REPLACE FUNCTION {}() RETURNS trigger LANGUAGE plpgsql VOLATILE SECURITY INVOKER AS $history${}$history$; COMMENT ON FUNCTION {}() IS '{}';",
                next.name, next.body, next.name, next.manifest,
            ))).execute(&mut *tx).await?;
            if !next.matches(&mut tx).await? {
                return Err(HistoryError::Verification(
                    "upstream update capture upgrade",
                ));
            }
        }
        tx.commit().await?;
        Ok(serde_json::json!({"mode":mode,"upstream_update_capture_upgraded":true}))
    }

    /// Replace only the known terminal UPDATE function. This changes no
    /// projected business value, capture epoch, source trigger or pending work.
    pub async fn history_upgrade_terminal_capture(&self) -> Result<Value, HistoryError> {
        if !matches!(self.backend, DatabaseBackend::PostgreSql) {
            return Err(HistoryError::Invalid(
                "terminal capture upgrade requires PostgreSQL",
            ));
        }
        let mut tx = self.pool.begin().await?;
        Self::history_bootstrap_maintenance_lock(&mut tx).await?;
        let mode: String = sqlx::query_scalar(
            "SELECT mode FROM request_history_state WHERE id=1 FOR SHARE NOWAIT",
        )
        .fetch_one(&mut *tx)
        .await?;
        let next = self
            .history_capture_definitions()?
            .into_iter()
            .find(|d| d.table == "terminal_projection_outbox" && d.event == "UPDATE")
            .ok_or(HistoryError::Mismatch)?;
        if !next.matches(&mut tx).await? {
            let old = self
                .history_capture_definitions_with_projection_guards(true, false)?
                .into_iter()
                .find(|d| d.name == next.name)
                .ok_or(HistoryError::Mismatch)?;
            if !old.matches(&mut tx).await? {
                return Err(HistoryError::Verification(
                    "unknown terminal update capture function",
                ));
            }
            sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
                "CREATE OR REPLACE FUNCTION {}() RETURNS trigger LANGUAGE plpgsql VOLATILE SECURITY INVOKER AS $history${}$history$; COMMENT ON FUNCTION {}() IS '{}';",
                next.name, next.body, next.name, next.manifest,
            ))).execute(&mut *tx).await?;
            if !next.matches(&mut tx).await? {
                return Err(HistoryError::Verification(
                    "terminal update capture upgrade",
                ));
            }
        }
        tx.commit().await?;
        Ok(serde_json::json!({"mode":mode,"terminal_update_capture_upgraded":true}))
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
        Self::history_require_unprepared(&mut transaction).await?;
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
        let actual:i64=sqlx::query_scalar("SELECT COUNT(*) FROM pg_trigger t JOIN pg_class c ON c.oid=t.tgrelid JOIN pg_namespace n ON n.oid=c.relnamespace JOIN pg_proc p ON p.oid=t.tgfoid WHERE n.nspname=current_schema() AND NOT t.tgisinternal AND t.tgparentid=0 AND t.tgname LIKE 'history_%' AND p.proname LIKE 'history_%'")
            .fetch_one(&mut *transaction).await?;
        if actual != expected {
            return Err(HistoryError::Verification(
                "unexpected capture catalog definitions",
            ));
        }
        // A single bounded maintenance wait queues behind short off-mode
        // writers instead of starving on continuous SHARE traffic. This local
        // lock-wait budget neither changes serve settings nor retries a failed
        // flip; later SHARE acquisitions can briefly queue behind this update.
        // It is not an end-to-end latency bound, and long holders still fail.
        sqlx::query("SET LOCAL lock_timeout='100ms'")
            .execute(&mut *transaction)
            .await?;
        // UPDATE takes the state barrier and rechecks off after a concurrent
        // row change. No catalog/source/object operation runs under this lock.
        let activated: Option<i64> = sqlx::query_scalar("UPDATE request_history_state SET mode='shadow',verified_revision=NULL,verification_json=NULL WHERE id=1 AND mode='off' RETURNING id::BIGINT")
            .fetch_optional(&mut *transaction).await?;
        if activated.is_none() {
            return Err(HistoryError::Changed);
        }
        Self::history_mark_activation(&mut transaction).await?;
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
        // PostgreSQL clones parent row triggers onto existing partitions.
        // Their exact parent definition is already checked above, but they
        // must not count as additional independently installed root triggers.
        let roots: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pg_trigger t JOIN pg_class c ON c.oid=t.tgrelid JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname=current_schema() AND NOT t.tgisinternal AND t.tgparentid=0 AND t.tgname LIKE 'history_%'")
            .fetch_one(&resumed.pool).await.unwrap();
        let clones: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pg_trigger t JOIN pg_class c ON c.oid=t.tgrelid JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname=current_schema() AND NOT t.tgisinternal AND t.tgparentid<>0 AND t.tgname LIKE 'history_%'")
            .fetch_one(&resumed.pool).await.unwrap();
        assert_eq!(roots, definitions.len() as i64);
        assert!(
            clones > 0,
            "real partitioned source tables must exercise inherited triggers"
        );
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
        // Model a still-running legacy function invocation during upgrade.
        sqlx::query("SELECT id FROM request_history_state WHERE id=1 FOR SHARE")
            .execute(&mut *old_writer)
            .await
            .unwrap();
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
        let error = match error {
            HistoryError::Sql(error) => error,
            HistoryError::Verification(reason) => {
                panic!("activation catalog rejected before writer barrier: {reason}")
            }
            HistoryError::Changed => {
                panic!("activation mode or maintenance ownership changed before writer barrier")
            }
            _ => panic!("activation returned a non-SQL failure before writer barrier"),
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
        assert_eq!(resumed.history_mode().await.unwrap(), "off");
        let prior_lock_timeout: String = sqlx::query_scalar("SHOW lock_timeout")
            .fetch_one(&resumed.pool)
            .await
            .unwrap();
        assert_ne!(
            prior_lock_timeout, "100ms",
            "SET LOCAL must not leak after timeout rollback"
        );
        old_writer.commit().await.unwrap();
        // A short writer releases only once a single activation is actually
        // queued, proving fairness without retrying activation or timing a
        // wall-clock deadline. Both independent activators still race once.
        let first_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&resumed.pool)
            .await
            .unwrap();
        let second_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&database.pool)
            .await
            .unwrap();
        let mut short_writer = writers.pool.begin().await.unwrap();
        let writer_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *short_writer)
            .await
            .unwrap();
        sqlx::query("SELECT id FROM request_history_state WHERE id=1 FOR SHARE")
            .execute(&mut *short_writer)
            .await
            .unwrap();
        let release_writer = async {
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    let queued: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE pid IN ($1,$2) AND $3=ANY(pg_blocking_pids(pid)))")
                        .bind(first_pid).bind(second_pid).bind(writer_pid)
                        .fetch_one(&admin).await.unwrap();
                    if queued { break; }
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            }).await.expect("one activation must queue behind the short writer");
            // A failed maintenance attempt never aborts a source transaction.
            sqlx::query("SELECT 1")
                .execute(&mut *short_writer)
                .await
                .unwrap();
            short_writer.commit().await.unwrap();
        };
        let (first_activation, second_activation, ()) = tokio::join!(
            resumed.history_activate_capture(),
            database.history_activate_capture(),
            release_writer,
        );
        assert_ne!(
            first_activation.is_ok(),
            second_activation.is_ok(),
            "exactly one concurrent activation owns the off-to-shadow transition"
        );
        assert_eq!(resumed.history_mode().await.unwrap(), "shadow");
        let after_lock_timeout: String = sqlx::query_scalar("SHOW lock_timeout")
            .fetch_one(&resumed.pool)
            .await
            .unwrap();
        assert_eq!(
            after_lock_timeout, prior_lock_timeout,
            "SET LOCAL must not leak after commit"
        );
        let new_event = Uuid::now_v7();
        let mut new_writer = writers.pool.begin().await.unwrap();
        insert_event(&mut new_writer, new_event).await.unwrap();
        new_writer.commit().await.unwrap();
        assert_eq!(resumed.history_pending_count().await.unwrap(), 1);
        // Activation requires the committed old-transaction horizon before
        // backfill may cover pre-activation writes.
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !resumed.history_capture_horizon_ready().await.unwrap() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
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

#[cfg(test)]
#[path = "upstream_projection_tests.rs"]
mod upstream_projection_tests;

#[cfg(test)]
#[path = "terminal_projection_guard_tests.rs"]
mod terminal_projection_guard_tests;
