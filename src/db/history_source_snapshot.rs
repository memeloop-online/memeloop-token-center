//! Maintenance-only consistent source reader. Never used by request completion.
use super::*;
use crate::request_history::{HistoryError, HistoryIdentity};
mod parity;

pub(crate) struct HistorySourceSnapshot {
    pub(crate) receipt: Value,
    pub(crate) transaction: Transaction<'static, Any>,
    source: usize,
    after: String,
    after_created_at: Option<i64>,
    pending: Option<HistorySourcePage>,
    owner: Uuid,
    epoch: i64,
    ordinal: i64,
    activation_epoch: i64,
    manifest: sha2::Sha256,
    parity: parity::BaselineParity,
    parity_verified: bool,
    target_identity: Option<crate::request_history::HistoryTargetIdentity>,
}

#[derive(Clone)]
pub(crate) struct HistorySourcePage {
    pub(crate) token: Uuid,
    pub(crate) rows: Vec<(HistoryIdentity, i64, BaselineAuthority)>,
    source: usize,
    after: String,
    after_created_at: Option<i64>,
}

#[derive(Clone)]
pub(crate) enum BaselineAuthority {
    Captured(Value),
    Frozen(crate::request_history::HistorySnapshot),
    Acknowledged(String),
}

fn baseline_range_cursor(id: &str, created_at: Option<i64>) -> String {
    match created_at {
        Some(time) => serde_json::json!({"created_at":time,"id":id}).to_string(),
        None => id.to_owned(),
    }
}

fn acknowledged_baseline_snapshot(
    identity: &HistoryIdentity,
    revision: i64,
    hash: &str,
    snapshot: Option<crate::request_history::HistorySnapshot>,
) -> Result<crate::request_history::HistorySnapshot, HistoryError> {
    let snapshot = snapshot.ok_or(HistoryError::Changed)?;
    if snapshot.identity != *identity
        || snapshot.revision != revision
        || crate::request_history::canonical_hash(&snapshot)? != hash
    {
        return Err(HistoryError::Changed);
    }
    Ok(snapshot)
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
        if !self.history_capture_horizon_ready().await? {
            return Err(HistoryError::Changed);
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
        if mode != "shadow" {
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
        sqlx::query("UPDATE request_history_state SET verified_revision=NULL,verification_json=NULL WHERE id=1").execute(&mut *barrier).await?;
        let ceiling:i64=sqlx::query_scalar("SELECT CASE WHEN is_called THEN last_value ELSE 0 END FROM request_history_export_revision_seq")
            .fetch_one(&mut *barrier).await?;
        // The optimistic preflight can race a new activation. Revalidate under
        // the state barrier before the RR socket takes its first MVCC snapshot.
        let activation =
            sqlx::query("SELECT epoch,phase FROM request_history_activation WHERE id=1")
                .fetch_one(&mut *barrier)
                .await?;
        let activation_epoch: i64 = activation.try_get("epoch")?;
        let phase: String = activation.try_get("phase")?;
        if phase != "drained" && phase != "legacy" {
            return Err(HistoryError::Changed);
        }
        let snapshot: String = sqlx::query_scalar("SELECT pg_current_snapshot()::TEXT")
            .fetch_one(&mut *source)
            .await?;
        let epoch:i64=sqlx::query_scalar("UPDATE request_history_delivery_control SET epoch=epoch+1,owner=$1,ceiling=$2 WHERE id=1 RETURNING epoch")
            .bind(owner.to_string()).bind(ceiling).fetch_one(&mut *barrier).await?;
        let now = chrono::Utc::now().timestamp_millis();
        sqlx::query("INSERT INTO request_history_baseline_runs (owner,epoch,ceiling,source_snapshot,status,created_at,updated_at,read_model_version,activation_epoch) VALUES ($1,$2,$3,$4,'active',$5,$5,$6,$7)")
            .bind(owner.to_string()).bind(epoch).bind(ceiling).bind(&snapshot).bind(now)
            .bind(crate::request_history::READ_MODEL_VERSION).bind(activation_epoch)
            .execute(&mut *barrier).await?;
        barrier.commit().await?;
        Ok(HistorySourceSnapshot {
            receipt: serde_json::json!({"owner":owner,"epoch":epoch,"ceiling":ceiling,"source_snapshot":snapshot,"quiescent":false}),
            transaction: source,
            source: 0,
            after: String::new(),
            after_created_at: None,
            pending: None,
            owner,
            epoch,
            ordinal: 0,
            activation_epoch,
            manifest: sha2::Digest::new(),
            parity: parity::BaselineParity::default(),
            parity_verified: false,
            target_identity: None,
        })
    }
}

impl HistorySourceSnapshot {
    async fn bind_target(
        &mut self,
        database: &Database,
        writer: &crate::request_history::ClickHouse,
        reader: &crate::request_history::ClickHouse,
    ) -> Result<(), HistoryError> {
        if self.target_identity.is_some() {
            return Ok(());
        }
        let target = writer.target_identity().await?;
        if target != reader.target_identity().await? {
            return Err(HistoryError::Mismatch);
        }
        let changed=sqlx::query("UPDATE request_history_baseline_runs SET target_identity=$1 WHERE owner=$2 AND epoch=$3 AND status='active' AND target_identity IS NULL")
            .bind(serde_json::to_string(&target)?).bind(self.owner.to_string()).bind(self.epoch).execute(&database.pool).await?;
        if changed.rows_affected() != 1 {
            return Err(HistoryError::Changed);
        }
        self.target_identity = Some(target);
        Ok(())
    }
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
            let source_rows = if *source == "event" {
                sqlx::query(super::request_history::EVENT_SOURCE_PAGE_SQL)
                    .bind(self.after_created_at.unwrap_or(i64::MIN))
                    .bind(&self.after)
                    .bind(page_size)
                    .fetch_all(&mut *self.transaction)
                    .await?
            } else {
                sqlx::query("SELECT source,request_id,tenant_id,key_id,created_at FROM request_history_source_ids WHERE source=$1 AND request_id>$2 ORDER BY request_id LIMIT $3")
                    .bind(*source).bind(&self.after).bind(page_size).fetch_all(&mut *self.transaction).await?
            };
            let rows = source_rows;
            if rows.is_empty() {
                self.source += 1;
                self.after.clear();
                self.after_created_at = None;
                continue;
            }
            let mut envelopes = Vec::with_capacity(rows.len());
            for row in &rows {
                let identity = HistoryIdentity {
                    source: row.try_get("source")?,
                    request_id: parse_uuid(row.try_get("request_id")?)?,
                    tenant_id: parse_uuid(row.try_get("tenant_id")?)?,
                    key_id: parse_uuid(row.try_get("key_id")?)?,
                    created_at: row.try_get("created_at")?,
                };
                let head = sqlx::query("SELECT head_revision,acknowledged_revision,acknowledged_hash FROM request_history_identities WHERE source=$1 AND request_id=$2 AND tenant_id=$3 AND key_id=$4 AND created_at=$5")
                    .bind(&identity.source).bind(identity.request_id.to_string()).bind(identity.tenant_id.to_string()).bind(identity.key_id.to_string()).bind(identity.created_at)
                    .fetch_optional(&mut *self.transaction).await?;
                let pending = sqlx::query("SELECT revision,snapshot FROM request_history_export WHERE source=$1 AND request_id=$2 AND tenant_id=$3 AND key_id=$4 AND created_at=$5")
                    .bind(&identity.source).bind(identity.request_id.to_string()).bind(identity.tenant_id.to_string()).bind(identity.key_id.to_string()).bind(identity.created_at)
                    .fetch_optional(&mut *self.transaction).await?;
                let revision = head
                    .as_ref()
                    .map(|r| r.try_get::<i64, _>("head_revision"))
                    .transpose()?
                    .unwrap_or(0)
                    .max(
                        pending
                            .as_ref()
                            .map(|r| r.try_get::<i64, _>("revision"))
                            .transpose()?
                            .unwrap_or(0),
                    );
                if revision <= 0
                    || revision
                        > self.receipt["ceiling"]
                            .as_i64()
                            .ok_or(HistoryError::Mismatch)?
                {
                    return Err(HistoryError::Changed);
                }
                let immutable = sqlx::query("SELECT source,request_id,tenant_id,key_id,created_at,snapshot,delivery_snapshot FROM request_history_immutable_outbox WHERE revision=$1")
                    .bind(revision).fetch_optional(&mut *self.transaction).await?;
                let authority = if let Some(captured) = immutable {
                    let stored = HistoryIdentity {
                        source: captured.try_get("source")?,
                        request_id: parse_uuid(captured.try_get("request_id")?)?,
                        tenant_id: parse_uuid(captured.try_get("tenant_id")?)?,
                        key_id: parse_uuid(captured.try_get("key_id")?)?,
                        created_at: captured.try_get("created_at")?,
                    };
                    if stored != identity {
                        return Err(HistoryError::Mismatch);
                    }
                    let envelope = history_immutable_outbox::decode_captured_envelope(
                        &captured.try_get::<String, _>("snapshot")?,
                        &identity,
                        revision,
                    )?;
                    if let Some(frozen) =
                        captured.try_get::<Option<String>, _>("delivery_snapshot")?
                    {
                        BaselineAuthority::Frozen(serde_json::from_str(&frozen)?)
                    } else {
                        BaselineAuthority::Captured(envelope)
                    }
                } else if let Some(frozen) = pending
                    .as_ref()
                    .map(|r| r.try_get::<Option<String>, _>("snapshot"))
                    .transpose()?
                    .flatten()
                {
                    BaselineAuthority::Frozen(serde_json::from_str(&frozen)?)
                } else if let Some(head) = head {
                    let acknowledged: Option<i64> = head.try_get("acknowledged_revision")?;
                    let hash: Option<String> = head.try_get("acknowledged_hash")?;
                    match (acknowledged, hash) {
                        (Some(ack), Some(hash)) if ack == revision && hash.len() == 64 => {
                            BaselineAuthority::Acknowledged(hash)
                        }
                        _ => return Err(HistoryError::Changed),
                    }
                } else {
                    return Err(HistoryError::Changed);
                };
                envelopes.push((identity, revision, authority));
            }
            let page = HistorySourcePage {
                token: Uuid::now_v7(),
                rows: envelopes,
                source: self.source,
                after: rows.last().unwrap().try_get("request_id")?,
                after_created_at: (*source == "event")
                    .then(|| rows.last().unwrap().try_get("created_at"))
                    .transpose()?,
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
        self.bind_target(database, writer, reader).await?;
        let Some(page) = self.next_envelopes(page_size).await? else {
            return Ok(0);
        };
        let identities = page
            .rows
            .iter()
            .map(|row| row.0.clone())
            .collect::<Vec<_>>();
        let existing = reader.baseline_snapshots(&identities).await?;
        let mut summaries = history_immutable_outbox::SummaryBatch::default();
        let mut expected = Vec::with_capacity(page.rows.len());
        let mut missing = Vec::new();
        for ((identity, revision, authority), existing) in page.rows.iter().zip(existing) {
            let row = match authority {
                BaselineAuthority::Captured(envelope) => {
                    database
                        .history_materialize_envelope_with_batch(
                            envelope,
                            identity.clone(),
                            *revision,
                            &mut summaries,
                        )
                        .await?
                }
                BaselineAuthority::Frozen(snapshot) => snapshot.clone(),
                BaselineAuthority::Acknowledged(hash) => {
                    acknowledged_baseline_snapshot(identity, *revision, hash, existing.clone())?
                }
            };
            if row.identity != *identity || row.revision != *revision {
                return Err(HistoryError::Mismatch);
            }
            match existing {
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
        let target = reader
            .baseline_snapshots(&identities)
            .await?
            .into_iter()
            .map(|row| row.ok_or(HistoryError::Mismatch))
            .collect::<Result<Vec<_>, _>>()?;
        self.acknowledge_page(database, page.token, &expected, &target)
            .await?;
        Ok(expected.len())
    }

    pub(crate) async fn verify_surface_parity(
        &mut self,
        database: &Database,
        reader: &crate::request_history::ClickHouse,
    ) -> Result<(), HistoryError> {
        if self.source != 4 || self.pending.is_some() {
            return Err(HistoryError::Changed);
        }
        if self.target_identity.as_ref() != Some(&reader.target_identity().await?) {
            return Err(HistoryError::Mismatch);
        }
        self.parity_verified = false;
        std::mem::take(&mut self.parity)
            .verify(database, &mut self.transaction, reader)
            .await?;
        self.parity_verified = true;
        Ok(())
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
        let manifest_hash = format!("{:x}", sha2::Digest::finalize(self.manifest));
        let parity = self.parity_verified.then(|| serde_json::json!({"read_model_version":crate::request_history::READ_MODEL_VERSION,"activation_epoch":self.activation_epoch,"baseline_owner":self.owner,"baseline_epoch":self.epoch,"ceiling":self.receipt["ceiling"],"manifest_hash":manifest_hash,"surface_parity":true,"query_version":1,"target_identity":self.target_identity}).to_string());
        let updated = sqlx::query("UPDATE request_history_baseline_runs SET status='source_complete',updated_at=$3,chunk_count=$4,manifest_hash=$5,sources_complete=$6,parity_json=$9 WHERE owner=$1 AND epoch=$2 AND status='active' AND read_model_version=$7 AND activation_epoch=$8")
            .bind(self.owner.to_string()).bind(self.epoch).bind(chrono::Utc::now().timestamp_millis())
            .bind(self.ordinal).bind(&manifest_hash)
            .bind(r#"["native","generation","archive","event"]"#)
            .bind(crate::request_history::READ_MODEL_VERSION).bind(self.activation_epoch).bind(parity)
            .execute(&mut *transaction).await?;
        if updated.rows_affected() != 1 {
            return Err(HistoryError::Changed);
        }
        let binding=sqlx::query("UPDATE request_history_activation SET baseline_owner=$1,baseline_epoch=$2 WHERE id=1 AND phase IN ('drained','legacy') AND epoch=$3")
            .bind(self.owner.to_string()).bind(self.epoch).bind(self.activation_epoch).execute(&mut *transaction).await?;
        if binding.rows_affected() != 1 {
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
            .bind(&page.rows[0].0.source)
            .bind(baseline_range_cursor(&self.after, self.after_created_at))
            .bind(baseline_range_cursor(&page.after, page.after_created_at))
            .bind(page.rows.len() as i64).bind(&source_hash).bind(&target_hash)
            .execute(&mut *transaction).await?;
        let hash: String = sqlx::query_scalar("SELECT source_hash FROM request_history_baseline_chunks WHERE owner=$1 AND epoch=$2 AND ordinal=$3")
            .bind(self.owner.to_string()).bind(self.epoch).bind(self.ordinal).fetch_one(&mut *transaction).await?;
        if hash != source_hash {
            return Err(HistoryError::Mismatch);
        }
        transaction.commit().await?;
        sha2::Digest::update(
            &mut self.manifest,
            canonical_hash(&(
                self.ordinal,
                &page.rows[0].0.source,
                baseline_range_cursor(&self.after, self.after_created_at),
                baseline_range_cursor(&page.after, page.after_created_at),
                page.rows.len(),
                &source_hash,
            ))?,
        );
        self.parity.accumulate(expected)?;
        self.source = page.source;
        self.after.clone_from(&page.after);
        self.after_created_at = page.after_created_at;
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

    #[test]
    fn request_history_baseline_ack_authority_keeps_exact_frozen_revision() {
        use crate::request_history::{HistorySnapshot, canonical_hash};
        let identity = HistoryIdentity {
            source: "event".into(),
            request_id: Uuid::now_v7(),
            tenant_id: Uuid::now_v7(),
            key_id: Uuid::now_v7(),
            created_at: 42,
        };
        let frozen = HistorySnapshot {
            identity: identity.clone(),
            revision: 7,
            payload: Some(serde_json::json!({"legacy":true})),
            read_model: Value::Null,
            statistics: None,
            filter_cost_micros: None,
        };
        let hash = canonical_hash(&frozen).unwrap();
        assert_eq!(
            canonical_hash(
                &acknowledged_baseline_snapshot(&identity, 7, &hash, Some(frozen.clone())).unwrap()
            )
            .unwrap(),
            hash
        );
        assert!(acknowledged_baseline_snapshot(&identity, 7, &hash, None).is_err());
        assert!(acknowledged_baseline_snapshot(&identity, 8, &hash, Some(frozen.clone())).is_err());
        assert!(
            acknowledged_baseline_snapshot(&identity, 7, &"0".repeat(64), Some(frozen.clone()))
                .is_err()
        );
        let mut wrong = frozen;
        wrong.identity.tenant_id = Uuid::now_v7();
        assert!(acknowledged_baseline_snapshot(&identity, 7, &hash, Some(wrong)).is_err());
        assert_eq!(
            baseline_range_cursor("id", Some(42)),
            r#"{"created_at":42,"id":"id"}"#
        );
    }

    #[tokio::test]
    async fn request_history_baseline_event_cursor_reuses_frozen_authority() {
        let directory = tempfile::tempdir().unwrap();
        let database = Database::connect(&format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("baseline.sqlite").display()
        ))
        .await
        .unwrap();
        database.migrate().await.unwrap();
        database.history_start().await.unwrap();
        let tenant = Uuid::now_v7();
        let key = Uuid::now_v7();
        // UUID order deliberately differs from event time, including an equal-time pair.
        let ids = [Uuid::from_u128(9), Uuid::from_u128(2), Uuid::from_u128(3)];
        let times = [42_i64, 43, 43];
        for (id, time) in ids.into_iter().zip(times) {
            sqlx::query("INSERT INTO request_events(event_id,tenant_id,key_id,request_id,event_at,event_kind,protocol,model,status_code,input_tokens,output_tokens,cost_micros) VALUES($1,$2,$3,$4,$5,'finished','openai','fixture',200,1,2,0)")
                .bind(id.to_string()).bind(tenant.to_string()).bind(key.to_string()).bind(Uuid::now_v7().to_string()).bind(time).execute(&database.pool).await.unwrap();
        }
        // Capture the existing journal once, then baseline must use those bytes.
        database.history_capture_pending(64).await.unwrap();
        let mut snapshot = HistorySourceSnapshot {
            receipt: serde_json::json!({"ceiling":i64::MAX}),
            transaction: database.pool.begin().await.unwrap(),
            source: 3,
            after: String::new(),
            after_created_at: None,
            pending: None,
            owner: Uuid::now_v7(),
            epoch: 1,
            ordinal: 0,
            activation_epoch: 1,
        };
        for (id, time) in ids.into_iter().zip(times) {
            let page = snapshot.next_envelopes(1).await.unwrap().unwrap();
            assert_eq!(page.rows[0].0.request_id, id);
            assert_eq!(page.rows[0].0.created_at, time);
            assert!(matches!(page.rows[0].2, BaselineAuthority::Captured(_)));
            assert_eq!(page.after_created_at, Some(time));
            // Only simulate an already acknowledged cursor; real receipts remain covered by PG/CH test.
            snapshot.after = page.after;
            snapshot.after_created_at = page.after_created_at;
            snapshot.pending = None;
        }
        assert!(snapshot.next_envelopes(1).await.unwrap().is_none());
        snapshot.transaction.rollback().await.unwrap();
        // An orphaned head cannot be reconstructed with today's DTO under an old revision.
        sqlx::query("DELETE FROM request_history_immutable_outbox")
            .execute(&database.pool)
            .await
            .unwrap();
        let mut orphan = HistorySourceSnapshot {
            receipt: serde_json::json!({"ceiling":i64::MAX}),
            transaction: database.pool.begin().await.unwrap(),
            source: 3,
            after: String::new(),
            after_created_at: None,
            pending: None,
            owner: Uuid::now_v7(),
            epoch: 1,
            ordinal: 0,
            activation_epoch: 1,
        };
        assert!(matches!(
            orphan.next_envelopes(1).await,
            Err(HistoryError::Changed)
        ));
        orphan.transaction.rollback().await.unwrap();
    }

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
        // The canonical operator view requires a tenant and an eligible key
        // (or reservation/account), exactly as real request admission does.
        let pepper = b"baseline-fixture-pepper-at-least-32-bytes";
        let issued = database
            .create_key(
                crate::db::CreateKeyInput {
                    tenant_external_id: "baseline-tenant".into(),
                    principal_external_id: "baseline-principal".into(),
                    alias: "baseline-key".into(),
                    currency: "USD".into(),
                    policy: crate::model::KeyPolicy::default(),
                    initial_balance: rust_decimal::Decimal::ZERO,
                    idempotency_key: None,
                },
                pepper,
            )
            .await
            .unwrap();
        let authenticated = database
            .authenticate_key(&issued.key, pepper)
            .await
            .unwrap();
        let key = authenticated.key_id;
        let tenant = authenticated.tenant_id;
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
        database.history_capture_pending(64).await.unwrap();
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
        snapshot
            .verify_surface_parity(&database, &client)
            .await
            .unwrap();
        let receipt = snapshot.finish(&database).await.unwrap();
        assert!(database.history_activation_baseline_proved().await.unwrap());
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
        // Deterministically race the successful horizon preflight with a new
        // activation epoch before either source barrier is acquired.
        let rejected = database
            .history_begin_source_snapshot_after_ready(&source, Uuid::now_v7(), async {
                database.history_start().await?;
                Ok(())
            })
            .await;
        assert!(matches!(rejected, Err(HistoryError::Changed)));
        let receipts: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM request_history_baseline_runs")
                .fetch_one(&database.pool)
                .await
                .unwrap();
        assert_eq!(receipts, 0, "pending epoch cannot obtain a source receipt");
        // Await the rejected RR socket's queued rollback before inspecting its
        // virtual transaction; the coordinator must not mistake it for a user.
        drop(source.pool.acquire().await.unwrap());
        assert!(database.history_capture_horizon_ready().await.unwrap());
        // The next preflight naturally drains that cohort. A second activation
        // in the backfill gap must also fail before reading a source or cursor.
        let rejected = database
            .history_backfill_page_after_ready(64, async {
                database.history_start().await?;
                Ok(())
            })
            .await;
        assert!(matches!(rejected, Err(HistoryError::Changed)));
        let phase: String =
            sqlx::query_scalar("SELECT phase FROM request_history_activation WHERE id=1")
                .fetch_one(&database.pool)
                .await
                .unwrap();
        assert_eq!(phase, "pending", "the interleaved activation actually ran");
        let progressed: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM request_history_backfill WHERE done<>0 OR after_id<>''",
        )
        .fetch_one(&database.pool)
        .await
        .unwrap();
        assert_eq!(progressed, 0);
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
