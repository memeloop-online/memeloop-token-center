//! Delivery-only maintenance fencing. Request capture never locks this row.
use super::*;
use crate::request_history::HistoryError;

impl Database {
    pub(crate) async fn history_delivery_status(&self) -> Result<Value, HistoryError> {
        let row = sqlx::query(
            "SELECT epoch,owner,ceiling FROM request_history_delivery_control WHERE id=1",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(serde_json::json!({"epoch":row.try_get::<i64,_>("epoch")?,
            "owner":row.try_get::<Option<String>,_>("owner")?,
            "ceiling":row.try_get::<Option<i64>,_>("ceiling")?,"quiescent":false}))
    }

    /// Called on a dedicated, reused one-connection maintenance pool. Holding
    /// this transaction across append/ACK cannot occupy a request pool slot.
    pub(crate) async fn history_delivery_lease(
        &self,
    ) -> Result<(Option<Transaction<'static, Any>>, Option<i64>), HistoryError> {
        if matches!(self.backend, DatabaseBackend::Sqlite) {
            return Ok((None, None));
        }
        let mut transaction = self.pool.begin().await?;
        // Only this delivery-control socket may be idle across the CH request.
        // Business/request sessions retain their configured idle timeout.
        sqlx::query("SET LOCAL idle_in_transaction_session_timeout='0'")
            .execute(&mut *transaction)
            .await?;
        let ceiling = sqlx::query_scalar::<_, Option<i64>>(
            "SELECT ceiling FROM request_history_delivery_control WHERE id=1 FOR SHARE",
        )
        .fetch_one(&mut *transaction)
        .await?;
        Ok((Some(transaction), ceiling))
    }

    /// Existing source triggers hold state FOR SHARE through commit. Obtaining
    /// its conflicting lock first fences committed sequence allocations. The
    /// separate NOWAIT control lock coordinates healthy page leases. A lost
    /// lease may still have an older <= watermark HTTP request in flight, so
    /// this does not prove target quiescence. There is no wait/retry or network
    /// operation while either exclusive lock is held.
    pub(crate) async fn history_fence_delivery(&self, owner: Uuid) -> Result<Value, HistoryError> {
        if matches!(self.backend, DatabaseBackend::Sqlite) {
            return Err(HistoryError::Invalid(
                "delivery fencing requires PostgreSQL",
            ));
        }
        let mut transaction = self.pool.begin().await?;
        let mode: String = sqlx::query_scalar(
            "SELECT mode FROM request_history_state WHERE id=1 FOR UPDATE NOWAIT",
        )
        .fetch_one(&mut *transaction)
        .await?;
        if mode == "off" {
            return Err(HistoryError::Changed);
        }
        let active: Option<String> = sqlx::query_scalar(
            "SELECT owner FROM request_history_delivery_control WHERE id=1 FOR UPDATE NOWAIT",
        )
        .fetch_one(&mut *transaction)
        .await?;
        if active.is_some() {
            return Err(HistoryError::Changed);
        }
        let ceiling:i64=sqlx::query_scalar("SELECT CASE WHEN is_called THEN last_value ELSE 0 END FROM request_history_export_revision_seq")
            .fetch_one(&mut *transaction).await?;
        let epoch:i64=sqlx::query_scalar("UPDATE request_history_delivery_control SET epoch=epoch+1,owner=$1,ceiling=$2 WHERE id=1 RETURNING epoch")
            .bind(owner.to_string()).bind(ceiling).fetch_one(&mut *transaction).await?;
        transaction.commit().await?;
        // This bounds revisions, not completion of already sent requests.
        // Snapshot/delta/in-flight finalization is still required for cutover.
        Ok(serde_json::json!({"owner":owner,"epoch":epoch,"ceiling":ceiling,"quiescent":false}))
    }

    pub(crate) async fn history_resume_delivery(
        &self,
        owner: Uuid,
        epoch: i64,
    ) -> Result<(), HistoryError> {
        if matches!(self.backend, DatabaseBackend::Sqlite) {
            return Err(HistoryError::Invalid(
                "delivery fencing requires PostgreSQL",
            ));
        }
        let mut transaction = self.pool.begin().await?;
        sqlx::query("SELECT id FROM request_history_delivery_control WHERE id=1 FOR UPDATE NOWAIT")
            .execute(&mut *transaction)
            .await?;
        let changed=sqlx::query("UPDATE request_history_delivery_control SET owner=NULL,ceiling=NULL WHERE id=1 AND owner=$1 AND epoch=$2 AND ceiling IS NOT NULL")
            .bind(owner.to_string()).bind(epoch).execute(&mut *transaction).await?;
        if changed.rows_affected() != 1 {
            return Err(HistoryError::Changed);
        }
        transaction.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    async fn reserve(database: &Database) -> i64 {
        let mut transaction = database.pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM request_history_state WHERE id=1 FOR SHARE")
            .execute(&mut *transaction)
            .await
            .unwrap();
        let revision=sqlx::query_scalar("INSERT INTO request_history_export (source,request_id,tenant_id,key_id,created_at) VALUES ('event',$1,$2,$3,42) RETURNING revision")
            .bind(Uuid::now_v7().to_string()).bind(Uuid::now_v7().to_string()).bind(Uuid::now_v7().to_string()).fetch_one(&mut *transaction).await.unwrap();
        transaction.commit().await.unwrap();
        revision
    }

    fn assert_nowait(error: HistoryError) {
        let HistoryError::Sql(error) = error else {
            panic!("expected PostgreSQL lock failure");
        };
        assert_eq!(
            error.as_database_error().unwrap().code().as_deref(),
            Some("55P03")
        );
    }

    #[tokio::test]
    #[ignore = "existing CI supplies the real PostgreSQL service"]
    async fn request_history_real_services_acceptance_delivery_ceiling_and_recovery() {
        let postgres = std::env::var("MTC_TEST_POSTGRES_URL").expect("PostgreSQL acceptance URL");
        let admin = sqlx::PgPool::connect(&postgres).await.unwrap();
        let schema = format!("history_fence_{}", Uuid::now_v7().simple());
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await
            .unwrap();
        let mut isolated = url::Url::parse(&postgres).unwrap();
        isolated
            .query_pairs_mut()
            .append_pair("options", &format!("-csearch_path={schema}"));
        // The normal CLI pool may have one connection. Its page guard must use
        // its own single connection, never starve snapshot/ACK acquisition.
        let database = Database::connect_with_max(isolated.as_str(), 1)
            .await
            .unwrap();
        database.migrate().await.unwrap();
        database.history_start().await.unwrap();
        let writer_database = database
            .clone()
            .with_auxiliary_pool_max_connections(1)
            .independent_persistence_pool();
        let mut writer = writer_database.pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM request_history_state WHERE id=1 FOR SHARE")
            .execute(&mut *writer)
            .await
            .unwrap();
        let owner = Uuid::now_v7();
        assert_nowait(
            tokio::time::timeout(
                Duration::from_secs(3),
                database.history_fence_delivery(owner),
            )
            .await
            .unwrap()
            .unwrap_err(),
        );
        assert_eq!(
            sqlx::query_scalar::<_, i32>("SELECT 1")
                .fetch_one(&mut *writer)
                .await
                .unwrap(),
            1
        );
        writer.rollback().await.unwrap();
        let first = reserve(&database).await;
        let entered = Arc::new(tokio::sync::Notify::new());
        let released = Arc::new(tokio::sync::Notify::new());
        let sent = Arc::new(std::sync::Mutex::new(Vec::<i64>::new()));
        let requests = Arc::new(AtomicUsize::new(0));
        let router = axum::Router::new().route(
            "/",
            axum::routing::post({
                let entered = entered.clone();
                let released = released.clone();
                let sent = sent.clone();
                let requests = requests.clone();
                move |body: axum::body::Bytes| {
                    let entered = entered.clone();
                    let released = released.clone();
                    let sent = sent.clone();
                    let requests = requests.clone();
                    async move {
                        let row: Value = serde_json::from_slice(&body).unwrap();
                        sent.lock().unwrap().push(row["revision"].as_i64().unwrap());
                        if requests.fetch_add(1, Ordering::SeqCst) == 0 {
                            entered.notify_one();
                            released.notified().await;
                        }
                        ""
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let client = crate::request_history::ClickHouse::new(
            database
                .http_clients
                .handle(crate::http_settings::HttpPurpose::General),
            &endpoint,
            "default".into(),
            "default".into(),
            String::new(),
        )
        .unwrap();
        let delivery = {
            let database = database.clone();
            let client = client.clone();
            tokio::spawn(async move {
                crate::request_history::migration::export_page(&database, &client, 1).await
            })
        };
        tokio::time::timeout(Duration::from_secs(5), entered.notified())
            .await
            .unwrap();
        // The page owns control across the actual network request. Failure must
        // release the briefly acquired state lock before returning to callers.
        assert_nowait(
            tokio::time::timeout(
                Duration::from_secs(3),
                database.history_fence_delivery(owner),
            )
            .await
            .unwrap()
            .unwrap_err(),
        );
        let mut writer = writer_database.pool.begin().await.unwrap();
        tokio::time::timeout(
            Duration::from_secs(3),
            sqlx::query("SELECT id FROM request_history_state WHERE id=1 FOR SHARE")
                .execute(&mut *writer),
        )
        .await
        .unwrap()
        .unwrap();
        writer.rollback().await.unwrap();
        released.notify_one();
        assert_eq!(delivery.await.unwrap().unwrap(), 1);
        assert_eq!(*sent.lock().unwrap(), vec![first]);
        let second = reserve(&database).await;
        let fence = database.history_fence_delivery(owner).await.unwrap();
        assert_eq!(fence["ceiling"], second);
        assert_eq!(fence["quiescent"], false);
        let epoch = fence["epoch"].as_i64().unwrap();
        let third = reserve(&database).await;
        assert!(
            third > second,
            "capture proceeds beyond the frozen delivery watermark"
        );
        assert_eq!(
            crate::request_history::migration::export_page(&database, &client, 1)
                .await
                .unwrap(),
            1
        );
        assert_eq!(*sent.lock().unwrap(), vec![first, second]);
        assert_eq!(database.history_pending_count().await.unwrap(), 1);
        assert!(matches!(
            crate::request_history::migration::drain(&database, &client, 1).await,
            Err(HistoryError::Changed)
        ));
        assert!(
            database
                .history_resume_delivery(Uuid::now_v7(), epoch)
                .await
                .is_err()
        );
        assert!(
            database
                .history_resume_delivery(owner, epoch + 1)
                .await
                .is_err()
        );
        assert_eq!(
            database.history_delivery_status().await.unwrap()["ceiling"],
            second
        );
        let reopened = Database::connect_with_max(isolated.as_str(), 1)
            .await
            .unwrap();
        assert_eq!(reopened.history_delivery_status().await.unwrap(), fence);
        reopened
            .history_resume_delivery(owner, epoch)
            .await
            .unwrap();
        assert_eq!(
            crate::request_history::migration::export_page(&database, &client, 1)
                .await
                .unwrap(),
            1
        );
        assert_eq!(*sent.lock().unwrap(), vec![first, second, third]);
        assert_eq!(database.history_pending_count().await.unwrap(), 0);
        assert!(database.history_delivery_status().await.unwrap()["ceiling"].is_null());
        reopened.close().await;
        writer_database.close().await;
        database.close().await;
        server.abort();
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(&admin)
            .await
            .unwrap();
        admin.close().await;
    }
    #[tokio::test]
    #[ignore = "existing CI supplies the real PostgreSQL service"]
    async fn request_history_real_services_acceptance_capture_fence_skips_writer_and_reads_committed_source()
     {
        let postgres = std::env::var("MTC_TEST_POSTGRES_URL").expect("PostgreSQL acceptance URL");
        let admin = sqlx::PgPool::connect(&postgres).await.unwrap();
        let schema = format!("history_capture_fence_{}", Uuid::now_v7().simple());
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await
            .unwrap();
        let mut isolated = url::Url::parse(&postgres).unwrap();
        isolated
            .query_pairs_mut()
            .append_pair("options", &format!("-csearch_path={schema}"));
        let database = Database::connect_with_max(isolated.as_str(), 2)
            .await
            .unwrap();
        database.migrate().await.unwrap();
        database.history_start().await.unwrap();
        let event = Uuid::now_v7();
        sqlx::query("INSERT INTO request_events (event_id,tenant_id,key_id,request_id,event_at,event_kind,protocol,model,status_code,input_tokens,output_tokens,cost_micros) VALUES ($1,$2,$3,$4,$5,'capture-fence','openai','fixture',200,0,0,0)")
            .bind(event.to_string()).bind(Uuid::now_v7().to_string()).bind(Uuid::now_v7().to_string()).bind(Uuid::now_v7().to_string()).bind(unix_millis()).execute(&database.pool).await.unwrap();
        let mut capture = database.pool.begin().await.unwrap();
        let old_revision:i64=sqlx::query_scalar("SELECT revision FROM request_history_export WHERE source='event' AND request_id=$1 FOR UPDATE")
            .bind(event.to_string()).fetch_one(&mut *capture).await.unwrap();
        let writer_database = database
            .clone()
            .with_auxiliary_pool_max_connections(1)
            .independent_persistence_pool();
        let application = format!("history-writer-{}", Uuid::now_v7().simple());
        let writer = {
            let database = writer_database.clone();
            let application = application.clone();
            tokio::spawn(async move {
                let mut transaction = database.pool.begin().await.unwrap();
                sqlx::query("SELECT set_config('application_name',$1,true)")
                    .bind(application)
                    .execute(&mut *transaction)
                    .await
                    .unwrap();
                sqlx::query("UPDATE request_events SET status_code=202 WHERE event_id=$1")
                    .bind(event.to_string())
                    .execute(&mut *transaction)
                    .await
                    .unwrap();
                transaction.commit().await.unwrap();
            })
        };
        tokio::time::timeout(Duration::from_secs(3),async {
            loop {
                let blocked:i64=sqlx::query_scalar("SELECT COUNT(*) FROM pg_stat_activity WHERE application_name=$1 AND wait_event_type='Lock'")
                    .bind(&application).fetch_one(&admin).await.unwrap();
                if blocked==1 {break;}
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.expect("writer must reach its real AFTER-trigger journal fence");
        // An uncommitted source UPDATE waiting in its trigger is invisible to
        // a plain MVCC source SELECT, which must not request a conflicting lock.
        let committed: i64 = tokio::time::timeout(
            Duration::from_secs(3),
            sqlx::query_scalar("SELECT status_code FROM request_events WHERE event_id=$1")
                .bind(event.to_string())
                .fetch_one(&mut *capture),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(committed, 200);
        tokio::time::timeout(Duration::from_secs(3), database.history_capture_pending(64))
            .await
            .unwrap()
            .unwrap();
        let copied: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM request_history_immutable_outbox WHERE request_id=$1",
        )
        .bind(event.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap();
        assert_eq!(
            copied, 0,
            "locked journal rows are skipped, never copied without their fence"
        );
        capture.rollback().await.unwrap();
        writer.await.unwrap();
        database.history_capture_pending(64).await.unwrap();
        let row = sqlx::query(
            "SELECT revision,snapshot FROM request_history_immutable_outbox WHERE request_id=$1",
        )
        .bind(event.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap();
        assert!(row.try_get::<i64, _>("revision").unwrap() > old_revision);
        let envelope: Value =
            serde_json::from_str(&row.try_get::<String, _>("snapshot").unwrap()).unwrap();
        assert_eq!(envelope["originals"]["event"]["status_code"], 202);
        writer_database.close().await;
        database.close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(&admin)
            .await
            .unwrap();
        admin.close().await;
    }
}
