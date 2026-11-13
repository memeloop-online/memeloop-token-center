//! Maintenance-only consistent source reader. Never used by request completion.
use super::*;
use crate::request_history::{HistoryError, HistoryIdentity};

pub(crate) struct HistorySourceSnapshot {
    pub(crate) receipt: Value,
    pub(crate) transaction: Transaction<'static, Any>,
    source: usize,
    after: String,
    pending: Option<HistorySourcePage>,
    owner: Uuid,
    epoch: i64,
    ordinal: i64,
}

#[derive(Clone)]
pub(crate) struct HistorySourcePage {
    pub(crate) token: Uuid,
    pub(crate) rows: Vec<(HistoryIdentity, i64, Value)>,
    source: usize,
    after: String,
}

impl Database {
    /// The source socket and BEGIN/SET are ready before taking the capture
    /// barrier. Its very first MVCC read occurs while that barrier is held.
    pub(crate) async fn history_begin_source_snapshot(
        &self,
        source_database: &Database,
        owner: Uuid,
    ) -> Result<HistorySourceSnapshot, HistoryError> {
        self.history_begin_source_snapshot_after_ready(
            source_database,
            owner,
            std::future::ready(Ok(())),
        )
        .await
    }

    async fn history_begin_source_snapshot_after_ready(
        &self,
        source_database: &Database,
        owner: Uuid,
        ready: impl std::future::Future<Output = Result<(), HistoryError>>,
    ) -> Result<HistorySourceSnapshot, HistoryError> {
        if !matches!(self.backend, DatabaseBackend::PostgreSql) {
            return Err(HistoryError::Invalid(
                "consistent history snapshot requires PostgreSQL",
            ));
        }
        let mut source = source_database.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *source)
            .await?;
        ready.await?;
        // Do not add SELECT 1, catalog inspection or any other snapshot-taking
        // query to `source` before acquiring both maintenance barriers below.
        let mut barrier = self.pool.begin().await?;
        let mode: String = sqlx::query_scalar(
            "SELECT mode FROM request_history_state WHERE id=1 FOR UPDATE NOWAIT",
        )
        .fetch_one(&mut *barrier)
        .await?;
        if mode == "off" {
            return Err(HistoryError::Changed);
        }
        let active: Option<String> = sqlx::query_scalar(
            "SELECT owner FROM request_history_delivery_control WHERE id=1 FOR UPDATE NOWAIT",
        )
        .fetch_one(&mut *barrier)
        .await?;
        if active.is_some() {
            return Err(HistoryError::Changed);
        }
        let ceiling:i64=sqlx::query_scalar("SELECT CASE WHEN is_called THEN last_value ELSE 0 END FROM request_history_export_revision_seq")
            .fetch_one(&mut *barrier).await?;
        let snapshot: String = sqlx::query_scalar("SELECT pg_current_snapshot()::TEXT")
            .fetch_one(&mut *source)
            .await?;
        let epoch:i64=sqlx::query_scalar("UPDATE request_history_delivery_control SET epoch=epoch+1,owner=$1,ceiling=$2 WHERE id=1 RETURNING epoch")
            .bind(owner.to_string()).bind(ceiling).fetch_one(&mut *barrier).await?;
        let now = chrono::Utc::now().timestamp_millis();
        sqlx::query("INSERT INTO request_history_baseline_runs (owner,epoch,ceiling,source_snapshot,status,created_at,updated_at) VALUES ($1,$2,$3,$4,'active',$5,$5)")
            .bind(owner.to_string()).bind(epoch).bind(ceiling).bind(&snapshot).bind(now)
            .execute(&mut *barrier).await?;
        barrier.commit().await?;
        Ok(HistorySourceSnapshot {
            receipt: serde_json::json!({"owner":owner,"epoch":epoch,"ceiling":ceiling,"source_snapshot":snapshot,"quiescent":false}),
            transaction: source,
            source: 0,
            after: String::new(),
            pending: None,
            owner,
            epoch,
            ordinal: 0,
        })
    }
}

impl HistorySourceSnapshot {
    /// Bounded SQL-only source envelopes under one RR snapshot. No archive GET
    /// or CH operation belongs here. Materialization uses the ordinary async
    /// delivery path only after this page's SQL reads have finished.
    pub(crate) async fn next_envelopes(
        &mut self,
        page_size: i64,
    ) -> Result<Option<HistorySourcePage>, HistoryError> {
        const SOURCES: [&str; 4] = ["native", "generation", "archive", "event"];
        if !(1..=64).contains(&page_size) {
            return Err(HistoryError::Invalid(
                "snapshot page size must be 1 through 64",
            ));
        }
        if let Some(page) = &self.pending {
            return Ok(Some(page.clone()));
        }
        while let Some(source) = SOURCES.get(self.source) {
            let rows=sqlx::query("SELECT s.source,s.request_id,s.tenant_id,s.key_id,s.created_at,GREATEST(COALESCE(e.revision,0),COALESCE(i.head_revision,0)) AS revision FROM request_history_source_ids s LEFT JOIN request_history_export e ON e.source=s.source AND e.request_id=s.request_id LEFT JOIN request_history_identities i ON i.source=s.source AND i.request_id=s.request_id WHERE s.source=$1 AND s.request_id>$2 ORDER BY s.request_id LIMIT $3")
                .bind(*source).bind(&self.after).bind(page_size).fetch_all(&mut *self.transaction).await?;
            if rows.is_empty() {
                self.source += 1;
                self.after.clear();
                continue;
            }
            let mut envelopes = Vec::with_capacity(rows.len());
            for row in &rows {
                let revision: i64 = row.try_get("revision")?;
                if revision <= 0
                    || revision
                        > self.receipt["ceiling"]
                            .as_i64()
                            .ok_or(HistoryError::Mismatch)?
                {
                    return Err(HistoryError::Changed);
                }
                let request: String = row.try_get("request_id")?;
                let (identity, envelope) = match *source {
                    "native" => {
                        history_immutable_outbox::source_native_envelope_in_transaction(
                            &mut self.transaction,
                            &request,
                            row,
                        )
                        .await?
                    }
                    "generation" => {
                        history_immutable_outbox::source_generation_envelope_in_transaction(
                            &mut self.transaction,
                            &request,
                            row,
                        )
                        .await?
                    }
                    "archive" => {
                        history_immutable_outbox::source_archive_envelope_in_transaction(
                            &mut self.transaction,
                            &request,
                            row,
                        )
                        .await?
                    }
                    "event" => {
                        let identity = HistoryIdentity {
                            source: "event".into(),
                            request_id: parse_uuid(request)?,
                            tenant_id: parse_uuid(row.try_get("tenant_id")?)?,
                            key_id: parse_uuid(row.try_get("key_id")?)?,
                            created_at: row.try_get("created_at")?,
                        };
                        let payload = Database::history_original_event_in_transaction(
                            &mut self.transaction,
                            identity.request_id,
                        )
                        .await?;
                        let original = history_immutable_outbox::original_event_in_transaction(
                            &mut self.transaction,
                            identity.request_id,
                        )
                        .await?;
                        let parent: Option<String> = sqlx::query_scalar(
                            "SELECT request_id FROM request_events WHERE event_id=$1",
                        )
                        .bind(identity.request_id.to_string())
                        .fetch_optional(&mut *self.transaction)
                        .await?;
                        let parent_source: Option<String> = if let Some(parent) = &parent {
                            sqlx::query_scalar("SELECT source FROM request_history_source_ids WHERE request_id=$1 AND tenant_id=$2 AND key_id=$3 AND source IN ('native','generation') ORDER BY source DESC LIMIT 1")
                                .bind(parent).bind(identity.tenant_id.to_string()).bind(identity.key_id.to_string()).fetch_optional(&mut *self.transaction).await?
                        } else {
                            None
                        };
                        let envelope = serde_json::json!({"version":1,"source":"event","revision":revision,"request_id":identity.request_id,"tenant_id":identity.tenant_id,"key_id":identity.key_id,"created_at":identity.created_at,"payload":payload,"read_model":null,"statistics":null,"filter_cost_micros":null,"originals":{"event":original},"archive_identity":{"tenant_id":identity.tenant_id,"request_id":parent,"source":parent_source}});
                        (identity, envelope)
                    }
                    _ => unreachable!("fixed source list"),
                };
                envelopes.push((identity, revision, envelope));
            }
            let page = HistorySourcePage {
                token: Uuid::now_v7(),
                rows: envelopes,
                source: self.source,
                after: rows.last().unwrap().try_get("request_id")?,
            };
            self.pending = Some(page.clone());
            return Ok(Some(page));
        }
        Ok(None)
    }

    /// One bounded SQL page followed by ordinary archive materialization and
    /// target readback. Neither object access nor CH HTTP holds the short
    /// capture barrier. Ambiguous failures keep this exact pending page.
    pub(crate) async fn verify_next_page(
        &mut self,
        database: &Database,
        writer: &crate::request_history::ClickHouse,
        reader: &crate::request_history::ClickHouse,
        page_size: i64,
    ) -> Result<usize, HistoryError> {
        use crate::request_history::canonical_hash;
        let Some(page) = self.next_envelopes(page_size).await? else {
            return Ok(0);
        };
        let mut expected = Vec::with_capacity(page.rows.len());
        let mut missing = Vec::new();
        for (identity, revision, envelope) in &page.rows {
            let row = database
                .history_materialize_envelope(envelope, identity.clone(), *revision)
                .await?;
            match reader.baseline_snapshot(identity).await? {
                Some(existing) if existing.revision > *revision => {
                    return Err(HistoryError::Changed);
                }
                Some(existing) if existing.revision == *revision => {
                    if canonical_hash(&existing)? != canonical_hash(&row)? {
                        return Err(HistoryError::Mismatch);
                    }
                }
                _ => missing.push(row.clone()),
            }
            expected.push(row);
        }
        writer.append(&missing).await?;
        let mut target = Vec::with_capacity(expected.len());
        for row in &expected {
            target.push(
                reader
                    .baseline_snapshot(&row.identity)
                    .await?
                    .ok_or(HistoryError::Mismatch)?,
            );
        }
        self.acknowledge_page(database, page.token, &expected, &target)
            .await?;
        Ok(expected.len())
    }

    /// End the source MVCC lifetime once every bounded page has a durable
    /// readback receipt. Delivery stays fenced for the separate delta/final
    /// validation phase; this receipt never authorizes cutover or source GC.
    pub(crate) async fn finish(self, database: &Database) -> Result<Value, HistoryError> {
        if self.source != 4 || self.pending.is_some() {
            return Err(HistoryError::Changed);
        }
        self.transaction.rollback().await?;
        let mut transaction = database.pool.begin().await?;
        let fence: Option<i64> = sqlx::query_scalar("SELECT epoch FROM request_history_delivery_control WHERE id=1 AND owner=$1 AND epoch=$2 FOR SHARE")
            .bind(self.owner.to_string()).bind(self.epoch).fetch_optional(&mut *transaction).await?;
        if fence != Some(self.epoch) {
            return Err(HistoryError::Changed);
        }
        let updated = sqlx::query("UPDATE request_history_baseline_runs SET status='source_complete',updated_at=$3 WHERE owner=$1 AND epoch=$2 AND status='active'")
            .bind(self.owner.to_string()).bind(self.epoch).bind(chrono::Utc::now().timestamp_millis())
            .execute(&mut *transaction).await?;
        if updated.rows_affected() != 1 {
            return Err(HistoryError::Changed);
        }
        transaction.commit().await?;
        Ok(
            serde_json::json!({"owner":self.owner,"epoch":self.epoch,"ceiling":self.receipt["ceiling"],"chunks":self.ordinal,"status":"source_complete","quiescent":false,"cutover_verified":false}),
        )
    }

    /// Compare complete canonical rows actually read from the target before
    /// persisting a receipt. The source cursor moves only after that commit.
    pub(crate) async fn acknowledge_page(
        &mut self,
        database: &Database,
        token: Uuid,
        expected: &[crate::request_history::HistorySnapshot],
        target: &[crate::request_history::HistorySnapshot],
    ) -> Result<(), HistoryError> {
        use crate::request_history::canonical_hash;
        let page = self.pending.as_ref().ok_or(HistoryError::Changed)?;
        if page.token != token
            || expected.len() != page.rows.len()
            || target.len() != expected.len()
        {
            return Err(HistoryError::Changed);
        }
        for ((identity, revision, _), row) in page.rows.iter().zip(expected) {
            if row.identity != *identity || row.revision != *revision {
                return Err(HistoryError::Mismatch);
            }
        }
        let source_hash = canonical_hash(&expected)?;
        let target_hash = canonical_hash(&target)?;
        if source_hash != target_hash {
            return Err(HistoryError::Mismatch);
        }
        let mut transaction = database.pool.begin().await?;
        let fence: Option<i64> = sqlx::query_scalar("SELECT epoch FROM request_history_delivery_control WHERE id=1 AND owner=$1 AND epoch=$2 FOR SHARE")
            .bind(self.owner.to_string()).bind(self.epoch).fetch_optional(&mut *transaction).await?;
        if fence != Some(self.epoch) {
            return Err(HistoryError::Changed);
        }
        let active: Option<String> = sqlx::query_scalar("SELECT status FROM request_history_baseline_runs WHERE owner=$1 AND epoch=$2 FOR SHARE")
            .bind(self.owner.to_string()).bind(self.epoch).fetch_optional(&mut *transaction).await?;
        if active.as_deref() != Some("active") {
            return Err(HistoryError::Changed);
        }
        sqlx::query("INSERT INTO request_history_baseline_chunks (owner,epoch,ordinal,source,after_id,through_id,row_count,source_hash,target_hash) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9) ON CONFLICT (owner,epoch,ordinal) DO NOTHING")
            .bind(self.owner.to_string()).bind(self.epoch).bind(self.ordinal)
            .bind(&page.rows[0].0.source).bind(&self.after).bind(&page.after)
            .bind(page.rows.len() as i64).bind(&source_hash).bind(&target_hash)
            .execute(&mut *transaction).await?;
        let hash: String = sqlx::query_scalar("SELECT source_hash FROM request_history_baseline_chunks WHERE owner=$1 AND epoch=$2 AND ordinal=$3")
            .bind(self.owner.to_string()).bind(self.epoch).bind(self.ordinal).fetch_one(&mut *transaction).await?;
        if hash != source_hash {
            return Err(HistoryError::Mismatch);
        }
        transaction.commit().await?;
        self.source = page.source;
        self.after.clone_from(&page.after);
        self.pending = None;
        self.ordinal += 1;
        Ok(())
    }
}

impl Database {
    /// Explicit recovery for a lost source connection. This never resumes its
    /// invalid PostgreSQL snapshot, reuses its cursor, or deletes queued delta.
    pub(crate) async fn history_abort_source_snapshot(
        &self,
        owner: Uuid,
        epoch: i64,
    ) -> Result<(), HistoryError> {
        let mut transaction = self.pool.begin().await?;
        let released = sqlx::query("UPDATE request_history_delivery_control SET owner=NULL,ceiling=NULL WHERE id=1 AND owner=$1 AND epoch=$2")
            .bind(owner.to_string()).bind(epoch).execute(&mut *transaction).await?;
        if released.rows_affected() != 1 {
            return Err(HistoryError::Changed);
        }
        let changed = sqlx::query("UPDATE request_history_baseline_runs SET status='aborted',updated_at=$3 WHERE owner=$1 AND epoch=$2 AND status IN ('active','source_complete')")
            .bind(owner.to_string()).bind(epoch).bind(chrono::Utc::now().timestamp_millis()).execute(&mut *transaction).await?;
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

    #[tokio::test]
    #[ignore = "existing CI supplies real PostgreSQL and ClickHouse services"]
    async fn request_history_real_services_acceptance_source_baseline_receipts_and_abort() {
        let postgres = std::env::var("MTC_TEST_POSTGRES_URL").unwrap();
        let endpoint = std::env::var("MTC_TEST_CLICKHOUSE_URL").unwrap();
        let admin = sqlx::PgPool::connect(&postgres).await.unwrap();
        let schema = format!("history_baseline_{}", Uuid::now_v7().simple());
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
        database.history_start().await.unwrap();
        let source = Database::connect_with_max(isolated.as_str(), 1)
            .await
            .unwrap();
        let request = Uuid::now_v7();
        let key = Uuid::now_v7();
        let tenant = Uuid::now_v7();
        // The canonical operator view joins the real tenant directory.
        sqlx::query("INSERT INTO tenants (id,external_id,created_at) VALUES ($1,$2,$3)")
            .bind(tenant.to_string())
            .bind(format!("baseline-{tenant}"))
            .bind(chrono::Utc::now().timestamp_millis())
            .execute(&database.pool)
            .await
            .unwrap();
        database
            .record_request_started(NewRequest {
                request_id: request,
                key_id: key,
                tenant_id: tenant,
                protocol: "openai".into(),
                model: "before-watermark".into(),
                request_object: "gap://fixture/request".into(),
                reservation_id: Uuid::now_v7(),
                upstream_account_id: None,
                model_route_id: None,
            })
            .await
            .unwrap();
        reqwest::Client::new()
            .post(&endpoint)
            .body(format!("CREATE DATABASE {schema}"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        let client = crate::request_history::ClickHouse::new(
            database
                .http_clients
                .handle(crate::http_settings::HttpPurpose::General),
            &endpoint,
            schema.clone(),
            "default".into(),
            String::new(),
        )
        .unwrap();
        client.initialize().await.unwrap();
        let owner = Uuid::now_v7();
        let mut snapshot = database
            .history_begin_source_snapshot(&source, owner)
            .await
            .unwrap();
        let epoch = snapshot.epoch;
        let watermark = snapshot.receipt["ceiling"].as_i64().unwrap();
        sqlx::query("UPDATE request_records SET model='after-watermark' WHERE id=$1")
            .bind(request.to_string())
            .execute(&database.pool)
            .await
            .unwrap();
        let page = snapshot.next_envelopes(64).await.unwrap().unwrap();
        assert!(
            snapshot
                .acknowledge_page(&database, Uuid::now_v7(), &[], &[])
                .await
                .is_err()
        );
        assert_eq!(
            snapshot.next_envelopes(64).await.unwrap().unwrap().token,
            page.token
        );
        while snapshot
            .verify_next_page(&database, &client, &client, 64)
            .await
            .unwrap()
            != 0
        {}
        let receipt = snapshot.finish(&database).await.unwrap();
        assert_eq!(receipt["status"], "source_complete");
        assert_eq!(receipt["quiescent"], false);
        assert_eq!(receipt["cutover_verified"], false);
        let identity = database.history_identity_for_view(request).await.unwrap();
        let stored = client.baseline_snapshot(&identity).await.unwrap().unwrap();
        assert!(stored.revision <= watermark);
        assert_eq!(stored.payload.unwrap()["model"], "before-watermark");
        let mismatches: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM request_history_baseline_chunks WHERE source_hash<>target_hash",
        )
        .fetch_one(&database.pool)
        .await
        .unwrap();
        assert_eq!(mismatches, 0);
        let newer: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM request_history_export WHERE revision>$1")
                .bind(watermark)
                .fetch_one(&database.pool)
                .await
                .unwrap();
        assert!(newer > 0, "capture continues above the baseline watermark");
        assert!(
            database
                .history_abort_source_snapshot(Uuid::now_v7(), epoch)
                .await
                .is_err()
        );
        database
            .history_abort_source_snapshot(owner, epoch)
            .await
            .unwrap();
        assert!(database.history_delivery_status().await.unwrap()["owner"].is_null());
        let status: String = sqlx::query_scalar(
            "SELECT status FROM request_history_baseline_runs WHERE owner=$1 AND epoch=$2",
        )
        .bind(owner.to_string())
        .bind(epoch)
        .fetch_one(&database.pool)
        .await
        .unwrap();
        assert_eq!(status, "aborted");
        reqwest::Client::new()
            .post(&endpoint)
            .body(format!("DROP DATABASE {schema}"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        source.pool.close().await;
        database.pool.close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(&admin)
            .await
            .unwrap();
        admin.close().await;
    }

    #[tokio::test]
    #[ignore = "existing CI supplies the real PostgreSQL service"]
    async fn request_history_real_services_acceptance_source_snapshot_barrier_order() {
        let postgres = std::env::var("MTC_TEST_POSTGRES_URL").expect("PostgreSQL acceptance URL");
        let admin = sqlx::PgPool::connect(&postgres).await.unwrap();
        let schema = format!("history_snapshot_{}", Uuid::now_v7().simple());
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
        database.history_start().await.unwrap();
        let source = Database::connect_with_max(isolated.as_str(), 1)
            .await
            .unwrap();
        let writer = Database::connect_with_max(isolated.as_str(), 1)
            .await
            .unwrap();
        let request = Uuid::now_v7().to_string();
        let tenant = Uuid::now_v7().to_string();
        let key = Uuid::now_v7().to_string();
        let mut snapshot = database.history_begin_source_snapshot_after_ready(
            &source, Uuid::now_v7(), async {
                // This commit happens after source BEGIN/SET but before the
                // capture barrier. Any accidental earlier MVCC read makes the
                // returned RR snapshot miss this committed row.
                let mut transaction = writer.pool.begin().await?;
                sqlx::query("SELECT id FROM request_history_state WHERE id=1 FOR SHARE")
                    .execute(&mut *transaction).await?;
                sqlx::query("INSERT INTO request_history_export (source,request_id,tenant_id,key_id,created_at) VALUES ('event',$1,$2,$3,42)")
                    .bind(&request).bind(&tenant).bind(&key).execute(&mut *transaction).await?;
                transaction.commit().await?;
                Ok(())
            },
        ).await.unwrap();
        let at_barrier: i64 =
            sqlx::query_scalar("SELECT revision FROM request_history_export WHERE request_id=$1")
                .bind(&request)
                .fetch_one(&mut *snapshot.transaction)
                .await
                .unwrap();
        assert_eq!(snapshot.receipt["ceiling"].as_i64(), Some(at_barrier));
        let mut late = writer.pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM request_history_state WHERE id=1 FOR SHARE")
            .execute(&mut *late)
            .await
            .unwrap();
        sqlx::query("UPDATE request_history_export SET revision=nextval('request_history_export_revision_seq') WHERE request_id=$1")
            .bind(&request).execute(&mut *late).await.unwrap();
        late.commit().await.unwrap();
        let still_at_barrier: i64 =
            sqlx::query_scalar("SELECT revision FROM request_history_export WHERE request_id=$1")
                .bind(&request)
                .fetch_one(&mut *snapshot.transaction)
                .await
                .unwrap();
        assert_eq!(still_at_barrier, at_barrier);
        assert_eq!(snapshot.receipt["quiescent"], false);
        snapshot.transaction.rollback().await.unwrap();
        source.pool.close().await;
        writer.pool.close().await;
        database.pool.close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(&admin)
            .await
            .unwrap();
        admin.close().await;
    }
}
