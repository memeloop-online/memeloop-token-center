use std::collections::HashSet;
use std::fmt::Write as _;

use super::super::*;

pub(crate) mod prefix_storage;
mod projection_retry;
mod semantic_payload;

#[cfg(test)]
mod semantic_order_tests;

#[cfg(test)]
mod archive_readiness_tests;

use projection_retry::ConversationContentError;

// Candidate selection reads at most 50 observations. Persisting the complete
// atom list for a near-limit request would otherwise let one ordinary request
// make the next request materialize hundreds of MiB of JSON. The full Merkle
// leaf remains in `leaf_node_hash`, so retries and true prefix continuations do
// not lose precision when the diagnostic fingerprint is capped.
const MAX_CONVERSATION_FINGERPRINT_ATOMS: usize = 1_024;
const MAX_CONVERSATION_FINGERPRINT_JSON_BYTES: usize = 70_000;
// Stay below SQLite's historical default of 999 bind variables as well as
// PostgreSQL's much larger protocol limit.
const CONVERSATION_INSERT_BATCH_BIND_LIMIT: usize = 900;
const SEMANTIC_ATOM_INSERT_BIND_COUNT: usize = 7;
// The worker cgroup limit is 384Mi. A full session's raw JSON is at most
// 128MiB; keep each insert batch far below the rest of that budget.
const CONVERSATION_INSERT_BATCH_BYTES: usize = 256 * 1024;
const CONTEXT_NODE_INSERT_BIND_COUNT: usize = 6;
const EXPLICIT_SESSION_LOCK_SEED: i64 = 734_627_102_948_338;
const CONVERSATION_REFERENCE_LOCK_SEED: i64 = 734_627_102_948_339;

#[derive(Clone, Debug)]
pub struct ConversationListFilter {
    pub limit: i64,
    pub before_updated_at: Option<i64>,
    pub before_cluster_id: Option<Uuid>,
}

#[derive(Clone, Debug)]
pub struct ConversationDetailFilter {
    pub limit: i64,
    pub before_created_at: Option<i64>,
    pub before_request_id: Option<Uuid>,
}

struct ConversationSelection {
    observation_id: String,
    relation: RelationKind,
    confidence: i64,
    direct_parent: bool,
    same_turn: bool,
    semantic_prefix: bool,
    compaction_overlap: bool,
    client_match: bool,
    write_edge: bool,
    reverse_edge: bool,
}

pub(crate) struct ConversationObservationInput<'a> {
    pub(crate) key: &'a AuthenticatedKey,
    pub(crate) request_id: Uuid,
    pub(crate) request_json: &'a serde_json::Value,
    pub(crate) hints: &'a ConversationHints,
    pub(crate) client_name: Option<&'a str>,
    /// If the terminal transaction already knows the upstream response id,
    /// include it in the record-phase reference lock ordering.
    pub(crate) upstream_response_id: Option<&'a str>,
    pub(crate) observed_at: i64,
    /// Archive-only observations intentionally have no request_records row.
    pub(crate) attach_request_record: bool,
    /// Content-addressed atoms and prefixes were committed before this
    /// transaction, so the session lock never nests shared unique-row waits.
    pub(crate) content_materialized: bool,
}

/// A lease-owned terminal observation awaiting semantic materialization.
///
/// The payload intentionally remains private to the database layer: workers
/// only need the stable request identity to project it, which prevents an
/// untrusted caller from altering a persisted observation between claim and
/// projection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConversationProjectionTask {
    pub request_id: Uuid,
    pub observed_at: i64,
}

pub(crate) struct SessionBodyLocators {
    pub(crate) request: String,
    pub(crate) response: Option<String>,
    pub(crate) pending: bool,
    pub(crate) source_identity: Option<ArchiveSpoolIdentity>,
    pub(crate) source_failed: bool,
    pub(crate) source_recorded: bool,
}

pub(crate) struct ConversationProjectionEnqueueInput<'a> {
    pub(crate) request_id: Uuid,
    pub(crate) key: &'a AuthenticatedKey,
    /// 结算时已经收好的对话字段。不是带填充的原始请求 JSON。
    pub(crate) request_json: &'a serde_json::Value,
    pub(crate) hints: &'a ConversationHints,
    pub(crate) client_name: Option<&'a str>,
    pub(crate) upstream_response_id: Option<&'a str>,
    pub(crate) observed_at: i64,
}

#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct ConversationSemanticSnapshot {
    leaf: Option<String>,
    atom_hashes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    payload_digest: Option<String>,
}

fn empty_semantic_snapshot(item: &ConversationSemanticSnapshot) -> bool {
    item.leaf.is_none() && item.atom_hashes.is_empty() && item.payload_digest.is_none()
}

const CONVERSATION_PROJECTION_BATCH_LIMIT: i64 = 32;
const CONVERSATION_PROJECTION_LEASE_MILLIS: i64 = 5 * 60 * 1_000;
const SESSION_BODY_REBUILD_LEASE_OWNER: &str = "session-body-rebuild";
const SESSION_BODY_REBUILD_OUTCOME: &str = "session_body";
const SESSION_BODY_INLINE_REQUEST_LIMIT: usize = 1024 * 1024;

fn inline_session_request(value: &serde_json::Value) -> String {
    let Ok(encoded) = serde_json::to_string(value) else {
        return "{}".to_owned();
    };
    if encoded.len() > SESSION_BODY_INLINE_REQUEST_LIMIT {
        "{}".to_owned()
    } else {
        encoded
    }
}

/// 网关结算只把请求身份放进队列。
/// 不在这里解析原始请求 JSON 或原始返回 JSON。
/// 会话正文归档由 worker 读取这两个 locator 之后写入 CNPG。
pub(crate) async fn enqueue_conversation_projection_in_transaction(
    transaction: &mut Transaction<'_, Any>,
    input: ConversationProjectionEnqueueInput<'_>,
) -> Result<(), AppError> {
    let ConversationProjectionEnqueueInput {
        request_id,
        key,
        request_json,
        hints,
        client_name,
        upstream_response_id,
        observed_at,
    } = input;
    // 这里不解析对象存储里的原始请求 JSON。只留下结算时已经收好的对话字段，
    // 供 locator 仍是 gap:// 时归档。超过上限的内容仍留给对象存储。
    //
    // The raw request body never lands in PostgreSQL: object storage owns it, and
    // this row only carries the bounded conversation fields the projection needs
    // while the locator is still `gap://`. An earlier revision persisted the full
    // body into `conversation_semantic_payloads` for every request, which grew the
    // database by roughly a gigabyte per hour of Codex traffic.
    let payload: Option<(String, String)> = None;
    let request_json = inline_session_request(request_json);
    let semantic_snapshot = {
        let snapshot = ConversationSemanticSnapshot {
            leaf: None,
            atom_hashes: Vec::new(),
            payload_digest: None,
        };
        Some(serde_json::to_string(&snapshot).map_err(|_| {
            AppError::internal("conversations_enqueue_conversation_projection_in_transaction_1")
        })?)
    };
    let hints_json = serde_json::to_string(hints).map_err(|_| {
        AppError::internal("conversations_enqueue_conversation_projection_in_transaction_2")
    })?;
    let key_snapshot = serde_json::json!({
        "account_id": key.account_id,
        "alias": key.alias,
        "currency": key.currency,
        "credential_generation": key.credential_generation,
        "policy": key.policy,
    })
    .to_string();
    let inserted = sqlx::query(
        "INSERT INTO conversation_projection_outbox (request_id, tenant_id, key_id, principal_id, request_json, hints_json, client_name, upstream_response_id, observed_at, key_snapshot_json, semantic_snapshot_json, semantic_payload_snapshot_json, lease_owner, lease_expires_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, CASE WHEN COALESCE($11, $12) IS NOT NULL THEN 'terminal-v118' END, CASE WHEN COALESCE($11, $12) IS NOT NULL THEN 9223372036854775807 END) ON CONFLICT(request_id) DO NOTHING",
    )
    .bind(request_id.to_string())
    .bind(key.tenant_id.to_string())
    .bind(key.key_id.to_string())
    .bind(key.principal_id.to_string())
    .bind(&request_json)
    .bind(&hints_json)
    .bind(client_name)
    .bind(upstream_response_id)
    .bind(observed_at)
    .bind(&key_snapshot)
    .bind(if payload.is_some() { None } else { semantic_snapshot.as_deref() })
    .bind(if payload.is_some() { semantic_snapshot.as_deref() } else { None })
    .execute(&mut **transaction)
    .await?;
    if inserted.rows_affected() == 0 {
        let existing = sqlx::query("SELECT tenant_id, key_id, principal_id, request_json, hints_json, client_name, upstream_response_id, observed_at, key_snapshot_json, projected_at, COALESCE(semantic_payload_snapshot_json, semantic_snapshot_json) AS semantic_snapshot_json, CAST(CASE WHEN semantic_payload_snapshot_json IS NOT NULL THEN 1 ELSE 0 END AS BIGINT) AS payload_required FROM conversation_projection_outbox WHERE request_id = $1")
            .bind(request_id.to_string()).fetch_one(&mut **transaction).await?;
        let completed = existing
            .try_get::<Option<i64>, _>("projected_at")?
            .is_some();
        // `observed_at` is when the winning terminal completed, not part of this
        // row's identity. Two owners that raced their own terminals legitimately
        // disagree by a few milliseconds, and `ON CONFLICT DO NOTHING` already
        // made the first value authoritative, so a later replay is not a content
        // conflict. Everything below is content and stays fatal.
        let observed_at_differs = existing.try_get::<i64, _>("observed_at")? != observed_at;
        let mismatched: Vec<&'static str> = [
            (
                "tenant_id",
                existing.try_get::<String, _>("tenant_id")? != key.tenant_id.to_string(),
            ),
            (
                "key_id",
                existing.try_get::<String, _>("key_id")? != key.key_id.to_string(),
            ),
            (
                "principal_id",
                existing.try_get::<String, _>("principal_id")? != key.principal_id.to_string(),
            ),
            (
                "request_json",
                existing.try_get::<String, _>("request_json")? != request_json,
            ),
            (
                "hints_json",
                existing.try_get::<String, _>("hints_json")? != hints_json,
            ),
            (
                "client_name",
                existing
                    .try_get::<Option<String>, _>("client_name")?
                    .as_deref()
                    != client_name,
            ),
            (
                "upstream_response_id",
                existing
                    .try_get::<Option<String>, _>("upstream_response_id")?
                    .as_deref()
                    != upstream_response_id,
            ),
            // `observed_at` is when the winning terminal completed, not part of
            // this row's identity. Two owners that raced their own terminals
            // legitimately disagree by a few milliseconds, and `ON CONFLICT DO
            // NOTHING` already made the first value authoritative. Treating it
            // as a content conflict turned a correct exactly-once settlement
            // into a 409 for the loser.
            (
                "key_snapshot_json",
                existing
                    .try_get::<Option<String>, _>("key_snapshot_json")?
                    .as_deref()
                    != Some(key_snapshot.as_str()),
            ),
            (
                "semantic_snapshot_json",
                !semantic_payload::snapshot_matches(
                    existing
                        .try_get::<Option<String>, _>("semantic_snapshot_json")?
                        .as_deref(),
                    semantic_snapshot.as_deref(),
                    completed,
                ),
            ),
            (
                "payload_required",
                (existing.try_get::<i64, _>("payload_required")? != 0) != payload.is_some(),
            ),
        ]
        .into_iter()
        .filter_map(|(field, differs)| differs.then_some(field))
        .collect();
        if !mismatched.is_empty() {
            tracing::warn!(
                %request_id,
                fields = %mismatched.join(","),
                "conversation projection enqueue disagrees with the stored row"
            );
            return Err(AppError::Conflict(
                "会话正文归档的入队记录和已有记录不一致".into(),
            ));
        }
        if observed_at_differs {
            // Expected when concurrent owners settle their own terminals.
            tracing::debug!(
                %request_id,
                "conversation projection replay carries a different observation time"
            );
        }
        if completed {
            return Ok(());
        }
    }
    Ok(())
}

/// 已有观测的历史行换成会话正文的叶子。没有观测时返回 false，调用方再插入。
async fn rewrite_session_body_observation_in_transaction(
    transaction: &mut Transaction<'_, Any>,
    tenant_id: &str,
    request_id: &str,
    snapshot: &ConversationSemanticSnapshot,
) -> Result<bool, ConversationContentError> {
    if let Some(leaf) = snapshot.leaf.as_deref() {
        let mut roots = std::collections::BTreeSet::new();
        roots.insert(leaf.to_owned());
        let graph = prefix_storage::load_paths(transaction, tenant_id, roots).await?;
        let full = graph.hashes(Some(leaf), None)?;
        if full
            .iter()
            .take(MAX_CONVERSATION_FINGERPRINT_ATOMS)
            .ne(snapshot.atom_hashes.iter())
        {
            return Err(AppError::Conflict("conversation prefix content mismatch".into()).into());
        }
    } else if !snapshot.atom_hashes.is_empty() {
        return Err(AppError::Conflict("conversation prefix content mismatch".into()).into());
    }
    let fingerprint = prefix_storage::Fingerprint::from_hashes(&snapshot.atom_hashes);
    let encoded = serde_json::to_string(&fingerprint)
        .map_err(|_| AppError::internal("conversations_rewrite_session_body_observation_1"))?;
    let updated = sqlx::query(
        "UPDATE conversation_observations SET leaf_node_hash = $1, atom_hashes_json = $2 WHERE request_id = $3",
    )
    .bind(snapshot.leaf.as_deref())
    .bind(&encoded)
    .bind(request_id)
    .execute(&mut **transaction)
    .await?;
    Ok(updated.rows_affected() == 1)
}

async fn rewrite_reopened_session_body_if_present(
    transaction: &mut Transaction<'_, Any>,
    request_id: Uuid,
) -> Result<(), ConversationContentError> {
    let row = sqlx::query(
        "SELECT tenant_id, statistics_outcome, COALESCE(semantic_payload_snapshot_json, semantic_snapshot_json) AS semantic_snapshot_json FROM conversation_projection_outbox WHERE request_id = $1",
    )
    .bind(request_id.to_string())
    .fetch_optional(&mut **transaction)
    .await?;
    let Some(row) = row else {
        return Ok(());
    };
    if row
        .try_get::<Option<String>, _>("statistics_outcome")?
        .as_deref()
        != Some(SESSION_BODY_REBUILD_OUTCOME)
    {
        return Ok(());
    }
    let Some(encoded) = row.try_get::<Option<String>, _>("semantic_snapshot_json")? else {
        return Ok(());
    };
    let snapshot: ConversationSemanticSnapshot = serde_json::from_str(&encoded)
        .map_err(|_| AppError::internal("conversations_rewrite_reopened_session_body_1"))?;
    let tenant_id: String = row.try_get("tenant_id")?;
    let request_id = request_id.to_string();
    rewrite_session_body_observation_in_transaction(
        transaction,
        &tenant_id,
        &request_id,
        &snapshot,
    )
    .await?;
    Ok(())
}

pub(crate) async fn wake_session_body_archive_in_transaction(
    transaction: &mut Transaction<'_, Any>,
    request_id: Uuid,
) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE conversation_projection_outbox
         SET lease_expires_at = CASE WHEN lease_owner IS NULL THEN NULL ELSE lease_expires_at END,
             terminal_lease_expires_at = NULL
         WHERE request_id = $1 AND projected_at IS NULL
           AND terminal_lease_owner IS NULL
           AND (lease_owner IS NULL OR lease_owner = 'terminal-v118')",
    )
    .bind(request_id.to_string())
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

impl Database {
    /// Retain conversation evidence without acknowledging financial settlement.
    /// The operational receipt supplies a stable timestamp for exact replay.
    pub(crate) async fn enqueue_operational_conversation_projection(
        &self,
        claim: &PendingProxySettlementClaim,
        receipt_id: Uuid,
        conversation: &ProxyConversationInput<'_>,
    ) -> Result<(), AppError> {
        if conversation.key.tenant_id != claim.tenant_id
            || conversation.key.key_id != claim.key_id
            || conversation.key.account_id != claim.account_id
        {
            return Err(AppError::NotFound);
        }
        let mut transaction = self.begin_write_transaction().await?;
        let observed_at: Option<i64> = sqlx::query_scalar(
            r#"SELECT o.completed_at FROM proxy_operational_terminals o
               JOIN pending_proxy_settlements p
                 ON p.request_id = o.request_id AND p.fence = o.fence
                AND p.owner_token = o.owner_token AND p.contract_digest = o.contract_digest
               WHERE o.request_id = $1 AND o.tenant_id = $2 AND o.key_id = $3
                 AND o.account_id = $4 AND o.reservation_id = $5
                 AND o.owner_token = $6 AND o.fence = $7
                 AND o.contract_digest = $8 AND o.receipt_id = $9"#,
        )
        .bind(claim.request_id.to_string())
        .bind(claim.tenant_id.to_string())
        .bind(claim.key_id.to_string())
        .bind(claim.account_id.to_string())
        .bind(claim.reservation_id.to_string())
        .bind(claim.owner_token.to_string())
        .bind(claim.fence.to_string())
        .bind(&claim.contract_digest)
        .bind(receipt_id.to_string())
        .fetch_optional(&mut *transaction)
        .await?;
        // Runtime publication can race this background attempt. Absence is
        // retryable; it is not a conflicting financial receipt.
        let observed_at = observed_at.ok_or(AppError::NotFound)?;
        enqueue_conversation_projection_in_transaction(
            &mut transaction,
            ConversationProjectionEnqueueInput {
                request_id: claim.request_id,
                key: conversation.key,
                request_json: conversation.request_json,
                hints: conversation.hints,
                client_name: conversation.client_name,
                upstream_response_id: conversation.upstream_response_id,
                observed_at,
            },
        )
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    #[allow(dead_code)]
    async fn materialize_conversation_content(
        &self,
        request_id: Uuid,
        lease_owner: Uuid,
        tenant_id: &str,
        request_json: &serde_json::Value,
        observed_at: i64,
    ) -> Result<(), AppError> {
        let atoms = extract_atoms(request_json);
        let nodes = build_prefix(&atoms);
        let mut transaction = self.begin_write_transaction().await?;
        let result = materialize_conversation_content_with_statement_error(
            &mut transaction,
            tenant_id,
            &atoms,
            &nodes,
            observed_at,
        )
        .await;
        if let Err(error) = result {
            let retryable_sqlstate = error.retryable_sqlstate();
            let rollback_confirmed = transaction.rollback().await.is_ok();
            tracing::warn!(
                %request_id,
                rollback_confirmed,
                sqlstate = retryable_sqlstate.unwrap_or("unclassified"),
                "conversation content transaction failed"
            );
            if rollback_confirmed && retryable_sqlstate.is_some() {
                self.try_schedule_rolled_back_conversation_retry(request_id, lease_owner)
                    .await;
            }
            return Err(error.into());
        }
        transaction.commit().await?;
        Ok(())
    }

    /// 把已经写过 `projected_at`、并且原始请求 JSON 与原始返回 JSON 的 locator 都还在的行
    /// 收成小批，交给新 worker 重写会话正文。
    ///
    /// 不能只把 `projected_at` 清掉：旧 worker 的第二条领取会忽略主租约，只要快照还在就签收。
    /// 这里同时清掉两份快照，并把主租约钉在 `session-body-rebuild` 上，直到新 worker 换成自己的领取。
    /// `statistics_outcome = session_body` 留在完成之后，避免同一行被反复重开。
    /// locator 缺一就不重开。不改 `request_object` / `response_object`。
    pub async fn reopen_session_body_rebuilds(&self, limit: i64) -> Result<u64, AppError> {
        let limit = limit.clamp(1, CONVERSATION_PROJECTION_BATCH_LIMIT);
        let mut transaction = self.begin_write_transaction().await?;
        let (outcome_predicate, lock) = match self.backend {
            DatabaseBackend::PostgreSql => (
                "o.statistics_outcome IS DISTINCT FROM 'session_body'",
                " FOR UPDATE OF o SKIP LOCKED",
            ),
            DatabaseBackend::Sqlite => ("o.statistics_outcome IS NOT 'session_body'", ""),
        };
        let statement = format!(
            "UPDATE conversation_projection_outbox SET projected_at = NULL, semantic_snapshot_json = NULL, semantic_payload_snapshot_json = NULL, lease_owner = '{SESSION_BODY_REBUILD_LEASE_OWNER}', lease_expires_at = 9223372036854775807, terminal_lease_owner = NULL, terminal_lease_expires_at = NULL, statistics_outcome = '{SESSION_BODY_REBUILD_OUTCOME}' WHERE request_id IN (SELECT o.request_id FROM conversation_projection_outbox o JOIN request_records r ON r.id = o.request_id WHERE o.projected_at IS NOT NULL AND {outcome_predicate} AND r.request_object <> '' AND r.response_object IS NOT NULL AND r.response_object <> '' ORDER BY o.observed_at ASC, o.request_id ASC LIMIT $1{lock})"
        );
        let updated = sqlx::query(sqlx::AssertSqlSafe(statement))
            .bind(limit)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(updated.rows_affected())
    }

    /// Deletes projection queue rows whose request never reached a terminal.
    ///
    /// A request that dies mid-flight (client abort, upstream reset, gateway
    /// restart) is never projected, so neither its outbox row nor its payload is
    /// ever consumed and the compaction pass - which only looks at projected
    /// rows - cannot retire it. Left alone these rows accumulate forever; at
    /// Codex traffic that was roughly a gigabyte of dead conversation rows per
    /// hour, which is what filled the database volume.
    ///
    /// The payload table cascades from the outbox primary key, so one delete
    /// frees both.
    pub(crate) async fn reap_abandoned_conversation_projections(
        &self,
        older_than_millis: i64,
        limit: i64,
    ) -> Result<u64, AppError> {
        let cutoff = unix_millis().saturating_sub(older_than_millis);
        let deleted = sqlx::query(
            "DELETE FROM conversation_projection_outbox WHERE request_id IN (SELECT request_id FROM conversation_projection_outbox WHERE projected_at IS NULL AND observed_at < $1 LIMIT $2)",
        )
        .bind(cutoff)
        .bind(limit)
        .execute(&self.pool)
        .await?;
        Ok(deleted.rows_affected())
    }

    /// Claims a bounded batch of terminal conversation observations for one
    /// projector. A crashed worker's lease expires and another worker can
    /// safely retry the same durable payload.
    pub async fn claim_conversation_projection_tasks(
        &self,
        lease_owner: Uuid,
        limit: i64,
    ) -> Result<Vec<ConversationProjectionTask>, AppError> {
        let now = unix_millis();
        let expires_at = now.saturating_add(CONVERSATION_PROJECTION_LEASE_MILLIS);
        let limit = limit.clamp(1, CONVERSATION_PROJECTION_BATCH_LIMIT);
        let mut transaction = self.begin_write_transaction().await?;
        let claimable = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT request_id FROM conversation_projection_outbox WHERE projected_at IS NULL AND (lease_expires_at IS NULL OR lease_expires_at <= $3) ORDER BY observed_at ASC, request_id ASC LIMIT $4 FOR UPDATE SKIP LOCKED"
            }
            // SQLite's BEGIN IMMEDIATE writer transaction serializes claimers,
            // so the same bounded selector is race-free without row locks.
            DatabaseBackend::Sqlite => {
                "SELECT request_id FROM conversation_projection_outbox WHERE projected_at IS NULL AND (lease_expires_at IS NULL OR lease_expires_at <= $3) ORDER BY observed_at ASC, request_id ASC LIMIT $4"
            }
        };
        let statement = format!(
            "UPDATE conversation_projection_outbox SET lease_owner = $1, lease_expires_at = $2, attempts = attempts + 1 WHERE request_id IN ({claimable}) RETURNING request_id, observed_at"
        );
        let sentinel_lock = match self.backend {
            DatabaseBackend::PostgreSql => " FOR UPDATE SKIP LOCKED",
            DatabaseBackend::Sqlite => "",
        };
        // 只领取租约主人仍是 session-body-rebuild 的行。主租约未过期，两份快照又是空的，
        // 所以旧 worker 的两条领取都匹配不到。
        let sentinel_statement = format!(
            "UPDATE conversation_projection_outbox SET lease_owner = $1, lease_expires_at = $2, attempts = attempts + 1 WHERE request_id IN (SELECT request_id FROM conversation_projection_outbox WHERE projected_at IS NULL AND lease_owner = '{SESSION_BODY_REBUILD_LEASE_OWNER}' ORDER BY observed_at ASC, request_id ASC LIMIT $3{sentinel_lock}) RETURNING request_id, observed_at"
        );
        let mut rows = sqlx::query(sqlx::AssertSqlSafe(sentinel_statement))
            .bind(lease_owner.to_string())
            .bind(expires_at)
            .bind(limit)
            .fetch_all(&mut *transaction)
            .await?;
        let mut budget = limit.saturating_sub(rows.len() as i64);
        if budget > 0 {
            let primary = sqlx::query(sqlx::AssertSqlSafe(statement))
                .bind(lease_owner.to_string())
                .bind(expires_at)
                .bind(now)
                .bind(budget)
                .fetch_all(&mut *transaction)
                .await?;
            budget = budget.saturating_sub(primary.len() as i64);
            rows.extend(primary);
        }
        if budget > 0 {
            let remaining = budget;
            let suffix = match self.backend {
                DatabaseBackend::PostgreSql => " FOR UPDATE SKIP LOCKED",
                DatabaseBackend::Sqlite => "",
            };
            let statement = format!(
                "UPDATE conversation_projection_outbox SET terminal_lease_owner = $1, terminal_lease_expires_at = $2, attempts = attempts + 1 WHERE request_id IN (SELECT request_id FROM conversation_projection_outbox WHERE projected_at IS NULL AND (semantic_snapshot_json IS NOT NULL OR semantic_payload_snapshot_json IS NOT NULL) AND (terminal_lease_owner IS NULL OR terminal_lease_owner <> $1) AND (terminal_lease_expires_at IS NULL OR terminal_lease_expires_at <= $3) ORDER BY observed_at, request_id LIMIT $4{suffix}) RETURNING request_id, observed_at"
            );
            rows.extend(
                sqlx::query(sqlx::AssertSqlSafe(statement))
                    .bind(lease_owner.to_string())
                    .bind(expires_at)
                    .bind(now)
                    .bind(remaining)
                    .fetch_all(&mut *transaction)
                    .await?,
            );
        }
        transaction.commit().await?;
        rows.into_iter()
            .map(|row| {
                Ok(ConversationProjectionTask {
                    request_id: parse_uuid(row.try_get("request_id")?)?,
                    observed_at: row.try_get("observed_at")?,
                })
            })
            .collect()
    }

    /// 读取这条请求上的原始请求 JSON locator 和原始返回 JSON locator。
    /// 没有记录时两个都空，调用方应跳过会话正文归档，而不是让客户端失败。
    pub(crate) async fn session_body_locators(
        &self,
        request_id: Uuid,
    ) -> Result<Option<SessionBodyLocators>, AppError> {
        let row = sqlx::query(
            "SELECT r.request_object, r.response_object, r.tenant_id, r.reservation_id, q.state AS source_state,
             CAST(CASE WHEN s.state IN ('capturing', 'pending', 'uploading')
                OR (s.request_id IS NULL AND r.response_object = 'gap://' || r.id || '/response')
                THEN 1 ELSE 0 END AS BIGINT) AS response_pending,
             CAST(CASE WHEN
                q.state IN ('capturing', 'pending', 'uploading')
                OR s.state IN ('capturing', 'pending', 'uploading')
                OR (q.request_id IS NULL AND r.request_object = 'gap://' || r.id || '/request')
                OR (s.request_id IS NULL AND r.response_object = 'gap://' || r.id || '/response')
                THEN 1 ELSE 0 END AS BIGINT) AS archives_pending
             FROM request_records r
             LEFT JOIN request_archive_spools q ON q.request_id = r.id AND q.tenant_id = r.tenant_id AND q.reservation_id = r.reservation_id
             LEFT JOIN response_archive_spools s ON s.request_id = r.id AND s.tenant_id = r.tenant_id AND s.reservation_id = r.reservation_id
             WHERE r.id = $1 LIMIT 1",
        )
        .bind(request_id.to_string())
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let request: String = row.try_get("request_object")?;
        let source_state: Option<String> = row.try_get("source_state")?;
        let source_identity = (crate::gateway_body::request_spool::source_request_id(&request)
            == Some(request_id))
        .then(|| -> Result<_, AppError> {
            Ok(ArchiveSpoolIdentity {
                request_id,
                tenant_id: parse_uuid(row.try_get("tenant_id")?)?,
                reservation_id: parse_uuid(row.try_get("reservation_id")?)?,
            })
        })
        .transpose()?;
        Ok(Some(SessionBodyLocators {
            request,
            response: row.try_get("response_object")?,
            pending: row.try_get::<i64, _>("response_pending")? != 0
                || (source_identity.is_none() && row.try_get::<i64, _>("archives_pending")? != 0),
            source_identity,
            source_failed: source_state.as_deref() == Some("gap"),
            source_recorded: source_state.as_deref() == Some("bound"),
        }))
    }

    pub(crate) async fn defer_session_body_archive(
        &self,
        lease_owner: Uuid,
        request_id: Uuid,
    ) -> Result<(), AppError> {
        sqlx::query(
            "UPDATE conversation_projection_outbox SET lease_owner = CASE WHEN lease_owner = 'terminal-v118' THEN lease_owner ELSE NULL END, lease_expires_at = CASE WHEN lease_owner = 'terminal-v118' THEN lease_expires_at ELSE $1 END, terminal_lease_owner = NULL, terminal_lease_expires_at = $1 WHERE request_id = $2 AND projected_at IS NULL AND COALESCE(terminal_lease_owner, lease_owner) = $3",
        )
        .bind(unix_millis().saturating_add(5000))
        .bind(request_id.to_string())
        .bind(lease_owner.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// locator 是 gap:// 时，用结算时已经收好的对话字段。不读对象存储。
    pub async fn session_body_inline_request(
        &self,
        lease_owner: Uuid,
        request_id: Uuid,
    ) -> Result<Option<String>, AppError> {
        let now = unix_millis();
        let row = sqlx::query(
            "SELECT request_json FROM conversation_projection_outbox WHERE request_id = $1 AND projected_at IS NULL AND COALESCE(terminal_lease_owner, lease_owner) = $2 AND COALESCE(terminal_lease_expires_at, lease_expires_at) >= $3",
        )
        .bind(request_id.to_string())
        .bind(lease_owner.to_string())
        .bind(now)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|row| row.try_get("request_json").ok()))
    }

    /// 把已经整理好的会话正文写入 CNPG。媒体 locator 记在 `conversation_media_parts`。
    /// 不读取对象存储，也不碰 `generation_assets`。
    pub async fn prepare_session_body_archive(
        &self,
        lease_owner: Uuid,
        request_id: Uuid,
        atoms: &[SemanticAtom],
        nodes: &[PrefixNode],
        media: &[crate::conversation::SessionMediaLocator],
    ) -> Result<bool, AppError> {
        let mut transaction = self.begin_write_transaction().await?;
        let prepared = self
            .prepare_session_body_archive_in_transaction(
                &mut transaction,
                lease_owner,
                request_id,
                atoms,
                nodes,
                media,
            )
            .await?;
        if !prepared {
            transaction.rollback().await?;
            return Ok(false);
        }
        transaction.commit().await?;
        Ok(prepared)
    }

    pub(in crate::db) async fn prepare_session_body_archive_in_transaction(
        &self,
        transaction: &mut Transaction<'_, Any>,
        lease_owner: Uuid,
        request_id: Uuid,
        atoms: &[SemanticAtom],
        nodes: &[PrefixNode],
        media: &[crate::conversation::SessionMediaLocator],
    ) -> Result<bool, AppError> {
        let now = unix_millis();
        let row = sqlx::query(
            "SELECT tenant_id, observed_at FROM conversation_projection_outbox WHERE request_id = $1 AND projected_at IS NULL AND COALESCE(terminal_lease_owner, lease_owner) = $2 AND COALESCE(terminal_lease_expires_at, lease_expires_at) >= $3",
        )
        .bind(request_id.to_string())
        .bind(lease_owner.to_string())
        .bind(now)
        .fetch_optional(&mut **transaction)
        .await?;
        let Some(row) = row else {
            return Ok(false);
        };
        let tenant_id: String = row.try_get("tenant_id")?;
        let observed_at: i64 = row.try_get("observed_at")?;
        materialize_conversation_content_in_transaction(
            transaction,
            &tenant_id,
            atoms,
            nodes,
            observed_at,
        )
        .await?;
        for item in media {
            if item.part_index < 0
                || item.byte_length < 0
                || item.media_type.is_empty()
                || item.media_type.len() > 128
                || item.content_blake3.len() != 64
                || item.object_locator.is_empty()
                || item.object_locator.len() > 1024
                || item.turn_id.is_empty()
            {
                continue;
            }
            sqlx::query(
                "INSERT INTO conversation_media_parts (tenant_id, turn_id, part_index, media_type, byte_length, content_blake3, object_locator) VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT(tenant_id, turn_id, part_index) DO NOTHING",
            )
            .bind(&tenant_id)
            .bind(&item.turn_id)
            .bind(item.part_index)
            .bind(&item.media_type)
            .bind(item.byte_length)
            .bind(&item.content_blake3)
            .bind(&item.object_locator)
            .execute(&mut **transaction)
            .await?;
        }
        let snapshot = ConversationSemanticSnapshot {
            leaf: nodes.last().map(|node| node.node_hash.clone()),
            atom_hashes: bounded_atom_hashes(atoms),
            payload_digest: None,
        };
        let encoded = serde_json::to_string(&snapshot)
            .map_err(|_| AppError::internal("conversations_prepare_session_body_archive_1"))?;
        let updated = sqlx::query(
            "UPDATE conversation_projection_outbox SET semantic_snapshot_json = $1, semantic_payload_snapshot_json = NULL WHERE request_id = $2 AND projected_at IS NULL AND COALESCE(terminal_lease_owner, lease_owner) = $3",
        )
        .bind(encoded)
        .bind(request_id.to_string())
        .bind(lease_owner.to_string())
        .execute(&mut **transaction)
        .await?;
        if updated.rows_affected() != 1 {
            return Ok(false);
        }
        Ok(true)
    }

    /// 缺少 locator 或原始文档读不出来时结束这条队列记录。
    /// 不清理 `request_object` / `response_object`，因此回收不会把还能引用的对象删掉。
    pub async fn skip_session_body_archive(
        &self,
        lease_owner: Uuid,
        request_id: Uuid,
        reason: &str,
    ) -> Result<bool, AppError> {
        let now = unix_millis();
        let note = serde_json::json!({ "session_body_skip": reason }).to_string();
        let updated = sqlx::query(
            "UPDATE conversation_projection_outbox SET projected_at = $1, semantic_snapshot_json = $2, semantic_payload_snapshot_json = NULL, statistics_outcome = 'session_body', lease_owner = NULL, lease_expires_at = NULL, terminal_lease_owner = NULL, terminal_lease_expires_at = NULL WHERE request_id = $3 AND projected_at IS NULL AND COALESCE(terminal_lease_owner, lease_owner) = $4",
        )
        .bind(now)
        .bind(note)
        .bind(request_id.to_string())
        .bind(lease_owner.to_string())
        .execute(&self.pool)
        .await?;
        Ok(updated.rows_affected() == 1)
    }

    /// 训练导出只读 CNPG 里的会话正文。不打开、不列出对象存储。
    pub async fn export_training_dialogue(
        &self,
        tenant_id: Uuid,
        leaf_node_hash: &str,
    ) -> Result<Vec<crate::conversation::TrainingUtterance>, AppError> {
        let rows = sqlx::query(
            "WITH RECURSIVE chain AS ( \
                SELECT node_hash, parent_hash, atom_hash, depth FROM context_nodes WHERE tenant_id = $1 AND node_hash = $2 \
                UNION ALL \
                SELECT n.node_hash, n.parent_hash, n.atom_hash, n.depth FROM context_nodes n \
                JOIN chain c ON n.tenant_id = $1 AND n.node_hash = c.parent_hash \
                WHERE c.depth > 0 AND c.depth < 4096 \
            ) \
            SELECT a.content_json FROM chain c \
            JOIN semantic_atoms a ON a.tenant_id = $1 AND a.content_hash = c.atom_hash \
            ORDER BY c.depth ASC LIMIT 4096",
        )
        .bind(tenant_id.to_string())
        .bind(leaf_node_hash)
        .fetch_all(&self.pool)
        .await?;
        let mut turns = Vec::with_capacity(rows.len());
        for row in rows {
            let content: String = row.try_get("content_json")?;
            let value = serde_json::from_str(&content)
                .map_err(|_| AppError::internal("conversations_export_training_dialogue_1"))?;
            turns.push(value);
        }
        Ok(crate::conversation::training_dialogue(&turns))
    }

    /// Materializes one task claimed by `lease_owner`. The observation writes,
    /// request membership update, usage reclassification, and `projected_at`
    /// acknowledgement commit atomically, making a retry exactly-once.
    pub async fn project_claimed_conversation_projection_task(
        &self,
        lease_owner: Uuid,
        request_id: Uuid,
    ) -> Result<bool, AppError> {
        // Materialize immutable, tenant-scoped content in its own transaction
        // before taking the explicit-session lock below. A stale lease is
        // rechecked in the final transaction; its durable outbox row makes a
        // committed content-only prefix retryable rather than user-visible.
        let prepared_snapshot = {
            let prepare_now = unix_millis();
            let prepare = sqlx::query(
                "SELECT tenant_id, key_id, principal_id, request_json, observed_at, lease_owner, COALESCE(semantic_payload_snapshot_json, semantic_snapshot_json) AS semantic_snapshot_json, CAST(CASE WHEN semantic_payload_snapshot_json IS NOT NULL THEN 1 ELSE 0 END AS BIGINT) AS payload_required, statistics_outcome FROM conversation_projection_outbox WHERE request_id = $1 AND projected_at IS NULL AND COALESCE(terminal_lease_owner, lease_owner) = $2 AND COALESCE(terminal_lease_expires_at, lease_expires_at) >= $3",
            )
            .bind(request_id.to_string())
            .bind(lease_owner.to_string())
            .bind(prepare_now)
            .fetch_optional(&self.pool)
            .await?;
            let Some(prepare) = prepare else {
                return Ok(false);
            };
            let snapshot: Option<ConversationSemanticSnapshot> = prepare
                .try_get::<Option<String>, _>("semantic_snapshot_json")?
                .map(|encoded| {
                    serde_json::from_str(&encoded).map_err(|_| {
                        AppError::internal(
                            "conversations_project_claimed_conversation_projection_task_1",
                        )
                    })
                })
                .transpose()?;
            let mut snapshot = snapshot;
            if snapshot.as_ref().is_some_and(empty_semantic_snapshot) {
                // Keep the primary terminal-v118 sentinel pinned so legacy
                // workers cannot claim the deferred body. Release only this
                // worker's lease when the empty snapshot needs another pass.
                let pinned = prepare
                    .try_get::<Option<String>, _>("lease_owner")?
                    .as_deref()
                    == Some("terminal-v118");
                if !pinned {
                    self.release_conversation_projection_lease(request_id, lease_owner)
                        .await?;
                    return Ok(false);
                }
                let terminal_open: Option<Option<i64>> = sqlx::query_scalar(
                    "SELECT projected_at FROM terminal_projection_outbox WHERE request_id = $1",
                )
                .bind(request_id.to_string())
                .fetch_optional(&self.pool)
                .await?;
                if terminal_open.is_some_and(|projected_at| projected_at.is_none()) {
                    self.release_conversation_projection_lease(request_id, lease_owner)
                        .await?;
                    return Ok(false);
                }
                let inline: String = prepare.try_get("request_json")?;
                let body = if inline != "{}" && !inline.is_empty() {
                    inline
                } else {
                    semantic_payload::load_reduced(
                        self,
                        request_id,
                        &prepare.try_get::<String, _>("tenant_id")?,
                        &prepare.try_get::<String, _>("key_id")?,
                        &prepare.try_get::<String, _>("principal_id")?,
                    )
                    .await?
                    .filter(|body| body != "{}" && !body.is_empty())
                    .unwrap_or_default()
                };
                if body.is_empty() {
                    self.release_conversation_projection_lease(request_id, lease_owner)
                        .await?;
                    return Ok(false);
                }
                let (leaf, atom_hashes) = conversation_prefix_from_request_json(body.as_bytes())
                    .map_err(|_| {
                        AppError::internal(
                            "conversations_project_claimed_conversation_projection_task_2",
                        )
                    })?;
                let rebuilt = ConversationSemanticSnapshot {
                    leaf,
                    atom_hashes,
                    payload_digest: None,
                };
                if empty_semantic_snapshot(&rebuilt) {
                    self.release_conversation_projection_lease(request_id, lease_owner)
                        .await?;
                    return Ok(false);
                }
                self.materialize_streamed_conversation_content(
                    request_id,
                    lease_owner,
                    &prepare.try_get::<String, _>("tenant_id")?,
                    body,
                    prepare.try_get("observed_at")?,
                    Some(&rebuilt),
                )
                .await?;
                let encoded = serde_json::to_string(&rebuilt).map_err(|_| {
                    AppError::internal(
                        "conversations_project_claimed_conversation_projection_task_3",
                    )
                })?;
                sqlx::query("UPDATE conversation_projection_outbox SET semantic_snapshot_json = $1, semantic_payload_snapshot_json = NULL WHERE request_id = $2 AND projected_at IS NULL AND COALESCE(terminal_lease_owner, lease_owner) = $3")
                    .bind(&encoded)
                    .bind(request_id.to_string())
                    .bind(lease_owner.to_string())
                    .execute(&self.pool)
                    .await?;
                snapshot = Some(rebuilt);
            }
            let payload_required = prepare.try_get::<i64, _>("payload_required")? != 0;
            if payload_required
                != snapshot
                    .as_ref()
                    .is_some_and(|s| s.payload_digest.is_some())
            {
                return Err(AppError::Conflict(
                    "conversation semantic payload marker mismatch".into(),
                ));
            }
            if snapshot.is_none()
                || snapshot
                    .as_ref()
                    .is_some_and(|s| s.payload_digest.is_some())
            {
                let prepare_tenant_id: String = prepare.try_get("tenant_id")?;
                let prepare_request_json = if let Some(expected) =
                    snapshot.as_ref().and_then(|s| s.payload_digest.as_deref())
                {
                    semantic_payload::load(
                        self,
                        request_id,
                        &prepare_tenant_id,
                        &prepare.try_get::<String, _>("key_id")?,
                        &prepare.try_get::<String, _>("principal_id")?,
                        expected,
                    )
                    .await?
                } else {
                    prepare.try_get::<String, _>("request_json")?
                };
                // Drop the raw session before the observation transaction. The
                // streamed insert below already released each message.
                self.materialize_streamed_conversation_content(
                    request_id,
                    lease_owner,
                    &prepare_tenant_id,
                    prepare_request_json,
                    prepare.try_get("observed_at")?,
                    snapshot.as_ref(),
                )
                .await?;
            }
            snapshot
        };

        let mut transaction = self.begin_write_transaction().await?;
        let result: Result<bool, ConversationContentError> = async {
        lock_request_stats_projection_writer_in_transaction(&mut transaction).await?;
        let now = unix_millis();
        let terminal = sqlx::query(
            "SELECT created_at, projected_at FROM terminal_projection_outbox WHERE request_id = $1",
        )
        .bind(request_id.to_string())
        .fetch_optional(&mut *transaction)
        .await?;
        if let Some(terminal) = terminal.as_ref()
            && terminal
                .try_get::<Option<i64>, _>("projected_at")?
                .is_none()
        {
            // Release ownership after rollback so the next worker can retry
            // as soon as the terminal projection commits.
            return Ok(false);
        }
        if let Some(terminal) = terminal.as_ref()
            && super::terminal_projection::statistics_pruned_in_transaction(
                &mut transaction,
                terminal.try_get("created_at")?,
            )
            .await?
        {
            rewrite_reopened_session_body_if_present(&mut transaction, request_id).await?;
            let skipped = sqlx::query("UPDATE conversation_projection_outbox SET projected_at = $1, statistics_outcome = CASE WHEN statistics_outcome = 'session_body' THEN 'session_body' ELSE 'pruned' END, lease_owner = NULL, lease_expires_at = NULL, terminal_lease_owner = NULL, terminal_lease_expires_at = NULL WHERE request_id = $2 AND projected_at IS NULL AND COALESCE(terminal_lease_owner, lease_owner) = $3 AND COALESCE(terminal_lease_expires_at, lease_expires_at) > $1")
                .bind(now).bind(request_id.to_string()).bind(lease_owner.to_string()).execute(&mut *transaction).await?;
            if skipped.rows_affected() == 1 {
                sqlx::query("DELETE FROM conversation_semantic_payloads WHERE request_id = $1")
                    .bind(request_id.to_string())
                    .execute(&mut *transaction)
                    .await?;
            }
            return Ok(skipped.rows_affected() == 1);
        }
        let select = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT tenant_id, key_id, principal_id, request_json, hints_json, client_name, upstream_response_id, observed_at, key_snapshot_json, COALESCE(semantic_payload_snapshot_json, semantic_snapshot_json) AS semantic_snapshot_json, CAST(CASE WHEN semantic_payload_snapshot_json IS NOT NULL THEN 1 ELSE 0 END AS BIGINT) AS payload_required, statistics_outcome FROM conversation_projection_outbox WHERE request_id = $1 AND projected_at IS NULL AND COALESCE(terminal_lease_owner, lease_owner) = $2 AND COALESCE(terminal_lease_expires_at, lease_expires_at) >= $3 FOR UPDATE"
            }
            DatabaseBackend::Sqlite => {
                "SELECT tenant_id, key_id, principal_id, request_json, hints_json, client_name, upstream_response_id, observed_at, key_snapshot_json, COALESCE(semantic_payload_snapshot_json, semantic_snapshot_json) AS semantic_snapshot_json, CAST(CASE WHEN semantic_payload_snapshot_json IS NOT NULL THEN 1 ELSE 0 END AS BIGINT) AS payload_required, statistics_outcome FROM conversation_projection_outbox WHERE request_id = $1 AND projected_at IS NULL AND COALESCE(terminal_lease_owner, lease_owner) = $2 AND COALESCE(terminal_lease_expires_at, lease_expires_at) >= $3"
            }
        };
        let task = sqlx::query(select)
            .bind(request_id.to_string())
            .bind(lease_owner.to_string())
            .bind(now)
            .fetch_optional(&mut *transaction)
            .await?;
        let Some(task) = task else {
            return Ok(false);
        };

        let tenant_id = parse_uuid(task.try_get("tenant_id")?)?;
        let key_id = parse_uuid(task.try_get("key_id")?)?;
        let principal_id = parse_uuid(task.try_get("principal_id")?)?;
        let projection_key = if let Some(snapshot) =
            task.try_get::<Option<String>, _>("key_snapshot_json")?
        {
            let snapshot: serde_json::Value =
                serde_json::from_str(&snapshot).map_err(|_| AppError::internal("conversations_site_1"))?;
            AuthenticatedKey {
                key_id,
                tenant_id,
                principal_id,
                account_id: serde_json::from_value(snapshot["account_id"].clone())
                    .map_err(|_| AppError::internal("conversations_site_2"))?,
                alias: serde_json::from_value(snapshot["alias"].clone())
                    .map_err(|_| AppError::internal("conversations_site_3"))?,
                currency: serde_json::from_value(snapshot["currency"].clone())
                    .map_err(|_| AppError::internal("conversations_site_4"))?,
                credential_generation: serde_json::from_value(
                    snapshot["credential_generation"].clone(),
                )
                .map_err(|_| AppError::internal("conversations_site_5"))?,
                policy: serde_json::from_value(snapshot["policy"].clone())
                    .map_err(|_| AppError::internal("conversations_site_6"))?,
            }
        } else {
            let key_row = sqlx::query(
            "SELECT id, tenant_id, principal_id, account_id, alias, currency, credential_generation, policy_json FROM key_records WHERE id = $1 AND tenant_id = $2 AND principal_id = $3",
        )
        .bind(key_id.to_string())
        .bind(tenant_id.to_string())
        .bind(principal_id.to_string())
        .fetch_optional(&mut *transaction)
        .await?
        .ok_or(AppError::NotFound)?;
            AuthenticatedKey {
                key_id: parse_uuid(key_row.try_get("id")?)?,
                tenant_id: parse_uuid(key_row.try_get("tenant_id")?)?,
                principal_id: parse_uuid(key_row.try_get("principal_id")?)?,
                account_id: parse_uuid(key_row.try_get("account_id")?)?,
                alias: key_row.try_get("alias")?,
                currency: key_row.try_get("currency")?,
                credential_generation: key_row.try_get("credential_generation")?,
                policy: serde_json::from_str(&key_row.try_get::<String, _>("policy_json")?)
                    .map_err(|_| AppError::internal("conversations_site_7"))?,
            }
        };
        let request_json = serde_json::from_str(&task.try_get::<String, _>("request_json")?)
            .map_err(|_| AppError::internal("conversations_site_8"))?;
        let hints = serde_json::from_str(&task.try_get::<String, _>("hints_json")?)
            .map_err(|_| AppError::internal("conversations_site_9"))?;
        let client_name: Option<String> = task.try_get("client_name")?;
        let upstream_response_id: Option<String> = task.try_get("upstream_response_id")?;
        let observed_at: i64 = task.try_get("observed_at")?;

        let snapshot: Option<ConversationSemanticSnapshot> = task
            .try_get::<Option<String>, _>("semantic_snapshot_json")?
            .map(|encoded| serde_json::from_str(&encoded).map_err(|_| AppError::internal("conversations_site_10")))
            .transpose()?;
        let payload_required = task.try_get::<i64, _>("payload_required")? != 0;
        if payload_required
            != snapshot
                .as_ref()
                .is_some_and(|s| s.payload_digest.is_some())
        {
            return Err(AppError::Conflict(
                "conversation semantic payload marker mismatch".into(),
                ).into());
        }
        if snapshot != prepared_snapshot {
            return Err(AppError::Conflict(
                "conversation semantic snapshot changed during projection".into(),
            ).into());
        }
        let rebuilding = task
            .try_get::<Option<String>, _>("statistics_outcome")?
            .as_deref()
            == Some(SESSION_BODY_REBUILD_OUTCOME);
        let rewrote_existing = if rebuilding && let Some(snapshot) = snapshot.as_ref() {
            let tenant = tenant_id.to_string();
            let request = request_id.to_string();
            rewrite_session_body_observation_in_transaction(
                &mut transaction,
                &tenant,
                &request,
                snapshot,
            )
            .await?
        } else {
            false
        };
        if !rewrote_existing {
            if let Some(expected) = snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.payload_digest.as_deref())
            {
                semantic_payload::verify_in_transaction(
                    &mut transaction,
                    self.backend,
                    request_id,
                    &projection_key,
                    expected,
                )
                .await?;
            }
            self.record_conversation_observation_with_snapshot_in_transaction(
                &mut transaction,
                ConversationObservationInput {
                    key: &projection_key,
                    request_id,
                    request_json: &request_json,
                    hints: &hints,
                    client_name: client_name.as_deref(),
                    upstream_response_id: upstream_response_id.as_deref(),
                    observed_at,
                    attach_request_record: true,
                    content_materialized: true,
                },
                snapshot.as_ref(),
            )
            .await?;
        }
        if let Some(upstream_response_id) = upstream_response_id.as_deref() {
            attach_conversation_upstream_response_in_transaction(
                &mut transaction,
                self.backend,
                request_id,
                upstream_response_id,
            )
            .await?;
        }
        let receipt = task.try_get::<Option<String>, _>("semantic_snapshot_json")?.as_deref().map(semantic_payload::completed_receipt);
        let acknowledged = sqlx::query(
            "UPDATE conversation_projection_outbox SET projected_at = $1, statistics_outcome = 'session_body', lease_owner = NULL, lease_expires_at = NULL, terminal_lease_owner = NULL, terminal_lease_expires_at = NULL, semantic_snapshot_json = CASE WHEN semantic_payload_snapshot_json IS NULL THEN $4 ELSE NULL END, semantic_payload_snapshot_json = CASE WHEN semantic_payload_snapshot_json IS NOT NULL THEN $4 ELSE NULL END WHERE request_id = $2 AND projected_at IS NULL AND COALESCE(terminal_lease_owner, lease_owner) = $3",
        )
        .bind(now)
        .bind(request_id.to_string())
        .bind(lease_owner.to_string())
        .bind(receipt)
        .execute(&mut *transaction)
        .await?;
        if acknowledged.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "conversation projection lease ownership changed".into(),
            ).into());
        }
        sqlx::query("DELETE FROM conversation_semantic_payloads WHERE request_id = $1 AND tenant_id = $2 AND key_id = $3")
            .bind(request_id.to_string()).bind(tenant_id.to_string()).bind(key_id.to_string()).execute(&mut *transaction).await?;
        emit_conversation_projected_event_in_transaction(
            &mut transaction,
            request_id,
            tenant_id,
            key_id,
        )
        .await?;
        Ok(true)
        }.await;
        match result {
            Ok(true) => {
                transaction.commit().await?;
                Ok(true)
            }
            Ok(false) => {
                transaction.rollback().await?;
                self.release_conversation_projection_lease(request_id, lease_owner)
                    .await?;
                Ok(false)
            }
            Err(error) => {
                let retryable_sqlstate = error.retryable_sqlstate();
                let rollback_confirmed = transaction.rollback().await.is_ok();
                tracing::warn!(
                    %request_id,
                    rollback_confirmed,
                    sqlstate = retryable_sqlstate.unwrap_or("unclassified"),
                    "conversation projection transaction failed"
                );
                if rollback_confirmed && retryable_sqlstate.is_some() {
                    self.try_schedule_rolled_back_conversation_retry(request_id, lease_owner)
                        .await;
                }
                Err(error.into())
            }
        }
    }

    pub async fn record_conversation_observation(
        &self,
        key: &AuthenticatedKey,
        request_id: Uuid,
        request_json: &serde_json::Value,
        hints: &ConversationHints,
        client_name: Option<&str>,
    ) -> Result<Uuid, AppError> {
        let mut transaction = self.begin_write_transaction().await?;
        let cluster_id = self
            .record_conversation_observation_in_transaction(
                &mut transaction,
                ConversationObservationInput {
                    key,
                    request_id,
                    request_json,
                    hints,
                    client_name,
                    upstream_response_id: None,
                    observed_at: unix_millis(),
                    attach_request_record: true,
                    content_materialized: false,
                },
            )
            .await?;
        transaction.commit().await?;
        Ok(cluster_id)
    }

    pub(crate) async fn record_conversation_observation_in_transaction(
        &self,
        transaction: &mut Transaction<'_, Any>,
        input: ConversationObservationInput<'_>,
    ) -> Result<Uuid, AppError> {
        self.record_conversation_observation_with_snapshot_in_transaction(transaction, input, None)
            .await
            .map_err(Into::into)
    }

    async fn record_conversation_observation_with_snapshot_in_transaction(
        &self,
        transaction: &mut Transaction<'_, Any>,
        input: ConversationObservationInput<'_>,
        snapshot: Option<&ConversationSemanticSnapshot>,
    ) -> Result<Uuid, ConversationContentError> {
        let ConversationObservationInput {
            key,
            request_id,
            request_json,
            hints,
            client_name,
            upstream_response_id,
            observed_at,
            attach_request_record,
            content_materialized,
        } = input;
        // Join the live projection-writer cohort before request source, explicit-session, or
        // conversation-reference locks. Full rebuilds take the exclusive form of this advisory.
        lock_request_stats_projection_writer_in_transaction(transaction).await?;
        let atoms = extract_atoms(request_json);
        let nodes = build_prefix(&atoms);
        let atom_hashes = snapshot
            .map(|snapshot| snapshot.atom_hashes.clone())
            .unwrap_or_else(|| bounded_atom_hashes(&atoms));
        let leaf = snapshot
            .map(|snapshot| snapshot.leaf.clone())
            .unwrap_or_else(|| nodes.last().map(|node| node.node_hash.clone()));
        let now = observed_at;
        let observation_id = Uuid::now_v7();
        let request_id = request_id.to_string();
        let request_created_at = if attach_request_record {
            let locator = sqlx::query(
                "SELECT created_at, tenant_id, key_id FROM request_record_locators WHERE id = $1 UNION ALL SELECT created_at, tenant_id, key_id FROM terminal_projection_outbox WHERE request_id = $1 AND NOT EXISTS (SELECT 1 FROM request_record_locators WHERE id = $1)",
            )
            .bind(&request_id)
            .fetch_optional(&mut **transaction)
            .await?
            .ok_or(AppError::NotFound)?;
            if locator.try_get::<String, _>("tenant_id")? != key.tenant_id.to_string()
                || locator.try_get::<String, _>("key_id")? != key.key_id.to_string()
            {
                return Err(AppError::NotFound.into());
            }
            Some(locator.try_get::<i64, _>("created_at")?)
        } else {
            None
        };
        let request_fact_exists = if let Some(request_created_at) = request_created_at {
            lock_request_records_projection_source_in_transaction(transaction).await?;
            if matches!(self.backend, DatabaseBackend::PostgreSql) {
                let source = sqlx::query(
                    "SELECT id FROM request_records WHERE id = $1 AND created_at = $2 FOR UPDATE",
                )
                .bind(&request_id)
                .bind(request_created_at)
                .fetch_optional(&mut **transaction)
                .await?;
                if source.is_none() && sqlx::query("SELECT request_id FROM terminal_projection_outbox WHERE request_id = $1 AND created_at = $2")
                    .bind(&request_id).bind(request_created_at).fetch_optional(&mut **transaction).await?.is_none() {
                    return Err(AppError::NotFound.into());
                }
            }
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM request_stats_facts WHERE request_id = $1",
            )
            .bind(&request_id)
            .fetch_one(&mut **transaction)
            .await?
                != 0
        } else {
            false
        };
        if matches!(self.backend, DatabaseBackend::PostgreSql)
            && let Some(session_id) = hints.session_id.as_deref()
        {
            // Explicit session identity is authoritative within one stable key.
            // Serialize candidate selection for that identity so two PostgreSQL
            // writers cannot both observe an empty cluster and create one.
            // Synchronous writers keep this lock before their content-addressed
            // INSERT ... ON CONFLICT writes below. Moving it later would invert
            // lock order with an older process during a rolling deploy. Projected
            // tasks instead commit those rows in a separate transaction first, so
            // they retain no content-row lock when they enter this session scope.
            sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, $2))")
                .bind(format!("{}:{session_id}", key.key_id))
                .bind(EXPLICIT_SESSION_LOCK_SEED)
                .execute(&mut **transaction)
                .await?;
        }

        let tenant_id = key.tenant_id.to_string();
        if !content_materialized {
            materialize_conversation_content_in_transaction(
                transaction,
                &tenant_id,
                &atoms,
                &nodes,
                now,
            )
            .await?;
        }

        let principal_id = key.principal_id.to_string();
        let key_id = key.key_id.to_string();
        lock_conversation_references_in_transaction(
            transaction,
            self.backend,
            &tenant_id,
            &principal_id,
            &key_id,
            &[
                hints.parent_turn_id.as_deref(),
                hints.turn_id.as_deref(),
                upstream_response_id,
            ],
        )
        .await?;
        // Every structured match below is decisive: the selection loop stops on
        // a direct parent, same turn, or explicit session. Fetch only that one
        // indexed row instead of reading and sorting up to 50 wide fingerprints
        // through one OR predicate while the explicit-session lock is held.
        let structured_candidate = fetch_structured_conversation_candidate(
            transaction,
            &tenant_id,
            &principal_id,
            &key_id,
            hints,
            now,
        )
        .await?;
        let candidates = if let Some(candidate) = structured_candidate {
            vec![candidate]
        } else if hints.parent_turn_id.is_some() {
            // A declared parent is authoritative. If it is not committed yet,
            // keep this observation isolated and durable for reconciliation
            // instead of attaching it to an unrelated semantic candidate.
            Vec::new()
        } else {
            sqlx::query(
                "SELECT o.id, o.cluster_id, o.atom_hashes_json, o.leaf_node_hash, o.explicit_session_id, o.turn_id, o.upstream_response_id, o.branch_id, o.client_name, o.created_at FROM conversation_observations o JOIN conversation_clusters c ON c.id = o.cluster_id WHERE c.tenant_id = $1 AND c.principal_id = $2 AND o.key_id = $3 AND o.created_at <= $4 ORDER BY o.created_at DESC LIMIT 50",
            )
            .bind(&tenant_id)
            .bind(&principal_id)
            .bind(&key_id)
            .bind(now)
            .fetch_all(&mut **transaction)
            .await?
        };

        let fingerprint = prefix_storage::Fingerprint::from_hashes(&atom_hashes);
        let graph = prefix_storage::load_candidate_paths(
            transaction,
            &tenant_id,
            &candidates,
            leaf.as_deref(),
        )
        .await?;
        {
            // A reference must never publish before its complete content-addressed
            // path exists, including when an importer claims content_materialized.
            let full = graph.hashes(leaf.as_deref(), None)?;
            if full
                .iter()
                .take(MAX_CONVERSATION_FINGERPRINT_ATOMS)
                .ne(atom_hashes.iter())
                || (snapshot.is_none() && full.len() != atoms.len())
            {
                return Err(
                    AppError::Conflict("conversation prefix content mismatch".into()).into(),
                );
            }
        }
        let atom_hashes_json = serde_json::to_string(&fingerprint)
            .map_err(|_| AppError::internal("conversations_site_11"))?;
        let has_semantic_atoms = !atom_hashes.is_empty();
        debug_assert!(atom_hashes_json.len() <= MAX_CONVERSATION_FINGERPRINT_JSON_BYTES);
        let current_node_hashes: HashSet<&str> =
            nodes.iter().map(|node| node.node_hash.as_str()).collect();
        let current_ancestors = if snapshot.is_some() {
            graph.ancestors(leaf.as_deref())?
        } else {
            HashSet::new()
        };
        let mut selected: Option<ConversationSelection> = None;
        let mut candidate_selection: Option<ConversationSelection> = None;
        for row in candidates {
            let candidate_session: Option<String> = row.try_get("explicit_session_id")?;
            let candidate_turn: Option<String> = row.try_get("turn_id")?;
            let candidate_response: Option<String> = row.try_get("upstream_response_id")?;
            let candidate_branch: Option<String> = row.try_get("branch_id")?;
            let candidate_client: Option<String> = row.try_get("client_name")?;
            let candidate_leaf: Option<String> = row.try_get("leaf_node_hash")?;
            let previous_hashes = prefix_storage::candidate_hashes(&row, &graph)?;
            let has_previous_semantic_atoms = !previous_hashes.is_empty();
            let is_prefix = candidate_leaf.as_deref().is_some_and(|candidate| {
                if snapshot.is_some() {
                    current_ancestors.contains(candidate)
                } else {
                    current_node_hashes.contains(candidate)
                }
            });
            let merkle_prefix =
                leaf.is_some() && (leaf.as_deref() == candidate_leaf.as_deref() || is_prefix);
            let (relation, confidence) =
                if leaf.as_deref() == candidate_leaf.as_deref() && leaf.is_some() {
                    (RelationKind::Retry, 980)
                } else if is_prefix {
                    (RelationKind::Continues, 950)
                } else if has_semantic_atoms && has_previous_semantic_atoms {
                    infer_hash_relation(&previous_hashes, &atom_hashes)
                } else {
                    (RelationKind::Candidate, 0)
                };
            let created_at: i64 = row.try_get("created_at")?;
            let direct_parent = hints.parent_turn_id.is_some()
                && (hints.parent_turn_id.as_deref() == candidate_turn.as_deref()
                    || hints.parent_turn_id.as_deref() == candidate_response.as_deref());
            let same_turn =
                hints.turn_id.is_some() && hints.turn_id.as_deref() == candidate_turn.as_deref();
            let explicit_match = hints.session_id.is_some()
                && hints.session_id.as_deref() == candidate_session.as_deref();
            // Settlement can finish after a later request has already settled.
            // Native session ancestry follows admission time, not that async
            // completion order. Imports retain their supplied observation time.
            let causally_prior = if explicit_match
                && let Some(request_created_at) = request_created_at
            {
                let candidate_created_at: i64 = sqlx::query_scalar(
                    "SELECT COALESCE(r.created_at, t.created_at, o.created_at) FROM conversation_observations o LEFT JOIN request_record_locators r ON r.id = o.request_id LEFT JOIN terminal_projection_outbox t ON t.request_id = o.request_id WHERE o.id = $1",
                )
                .bind(row.try_get::<String, _>("id")?)
                .fetch_one(&mut **transaction)
                .await?;
                candidate_created_at <= request_created_at
            } else {
                created_at <= now
            };
            let conflicting_sessions =
                hints.session_id.is_some() && candidate_session.is_some() && !explicit_match;
            let exact_prefix = merkle_prefix
                || (has_semantic_atoms && has_previous_semantic_atoms && confidence >= 700);
            let recent_candidate = now.saturating_sub(created_at) <= 30 * 60 * 1_000;
            let same_client = client_name.is_some() && client_name == candidate_client.as_deref();
            let compaction_overlap = hints.compaction
                && recent_candidate
                && same_client
                && meaningful_atom_overlap(&previous_hashes, &atom_hashes);
            if direct_parent
                || same_turn
                || explicit_match
                || (exact_prefix && !conflicting_sessions)
                || (compaction_overlap && !conflicting_sessions)
            {
                let branch_changed = hints.branch_id.is_some()
                    && candidate_branch.is_some()
                    && hints.branch_id.as_deref() != candidate_branch.as_deref();
                let reverse_prefix = !causally_prior
                    && explicit_match
                    && !direct_parent
                    && !same_turn
                    && !branch_changed
                    && !hints.compaction
                    && leaf.is_some()
                    && leaf != candidate_leaf
                    && row
                        .try_get::<Option<String>, _>("parent_turn_id")?
                        .is_none()
                    && row.try_get::<i64, _>("compaction")? == 0
                    && graph
                        .ancestors(candidate_leaf.as_deref())?
                        .contains(leaf.as_deref().unwrap());
                let relation = if reverse_prefix {
                    RelationKind::Continues
                } else if direct_parent && hints.subagent {
                    RelationKind::Subagent
                } else if same_turn {
                    RelationKind::Retry
                } else if hints.compaction
                    || (explicit_match && atom_hashes.len() * 2 < previous_hashes.len())
                {
                    RelationKind::Compacts
                } else if direct_parent && branch_changed {
                    RelationKind::Branch
                } else if direct_parent {
                    RelationKind::Continues
                } else {
                    relation
                };
                let confidence = if direct_parent || same_turn {
                    995
                } else if explicit_match {
                    confidence.max(990)
                } else if compaction_overlap {
                    confidence.max(880)
                } else {
                    confidence
                };
                selected = Some(ConversationSelection {
                    observation_id: row.try_get("id")?,
                    relation,
                    confidence,
                    direct_parent,
                    same_turn,
                    semantic_prefix: exact_prefix || reverse_prefix,
                    compaction_overlap,
                    client_match: same_client,
                    // A durable session id is sufficient to place observations in one
                    // cluster, but it does not prove that two adjacent requests are a
                    // continuation. Persist a directed edge only when the protocol names
                    // the parent/turn, the payload establishes a Merkle-prefix relation,
                    // or the client explicitly marks a compaction.
                    write_edge: direct_parent
                        || (causally_prior && (same_turn || exact_prefix || hints.compaction))
                        || reverse_prefix,
                    reverse_edge: reverse_prefix,
                });
                if direct_parent || same_turn || explicit_match || exact_prefix {
                    break;
                }
            } else if candidate_selection.is_none()
                && has_semantic_atoms
                && has_previous_semantic_atoms
                && recent_candidate
                && same_client
                && !conflicting_sessions
            {
                candidate_selection = Some(ConversationSelection {
                    observation_id: row.try_get("id")?,
                    relation: RelationKind::Candidate,
                    confidence: confidence.clamp(1, 699),
                    direct_parent: false,
                    same_turn: false,
                    semantic_prefix: false,
                    compaction_overlap: false,
                    client_match: true,
                    write_edge: true,
                    reverse_edge: false,
                });
            }
        }

        let unresolved_parent_reference = hints.parent_turn_id.as_deref().filter(|_| {
            !selected
                .as_ref()
                .is_some_and(|selection| selection.direct_parent)
        });
        let cluster_id = if let Some(selection) = &selected {
            lock_selected_conversation_cluster_in_transaction(
                transaction,
                self.backend,
                &selection.observation_id,
            )
            .await?
        } else {
            let id = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO conversation_clusters (id, tenant_id, principal_id, explicit_session_id, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(id.to_string())
            .bind(key.tenant_id.to_string())
            .bind(key.principal_id.to_string())
            .bind(hints.session_id.as_deref())
            .bind(now)
            .bind(now)
            .execute(&mut **transaction)
            .await?;
            id
        };

        let execution_metadata = hints.execution_metadata();
        let labels_json = serde_json::to_string(&hints.labels)
            .map_err(|_| AppError::internal("conversations_site_12"))?;

        sqlx::query(
            "INSERT INTO conversation_observations (id, cluster_id, request_id, key_id, leaf_node_hash, atom_hashes_json, explicit_session_id, client_name, created_at, inference_version, turn_id, parent_turn_id, branch_id, compaction, session_name, trace_id, span_id, parent_span_id, agent_id, parent_agent_id, task_kind, labels_json, metadata_source) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 2, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22)",
        )
        .bind(observation_id.to_string())
        .bind(cluster_id.to_string())
        .bind(request_id.to_string())
        .bind(key.key_id.to_string())
        .bind(leaf)
        .bind(atom_hashes_json)
        .bind(hints.session_id.as_deref())
        .bind(client_name)
        .bind(now)
        .bind(hints.turn_id.as_deref())
        .bind(hints.parent_turn_id.as_deref())
        .bind(hints.branch_id.as_deref())
        .bind(i64::from(hints.compaction))
        .bind(hints.session_name.as_deref())
        .bind(hints.trace_id.as_deref())
        .bind(hints.span_id.as_deref())
        .bind(hints.parent_span_id.as_deref())
        .bind(hints.agent_id.as_deref())
        .bind(hints.parent_agent_id.as_deref())
        .bind(hints.task_kind.as_deref())
        .bind(labels_json)
        .bind(execution_metadata.as_ref().map(|metadata| metadata.source.as_str()))
        .execute(&mut **transaction)
        .await?;

        if let Some(parent_reference) = unresolved_parent_reference {
            sqlx::query(
                "INSERT INTO conversation_unresolved_explicit_parents (child_observation_id, tenant_id, principal_id, key_id, parent_reference, subagent, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT(child_observation_id) DO NOTHING",
            )
            .bind(observation_id.to_string())
            .bind(&tenant_id)
            .bind(&principal_id)
            .bind(&key_id)
            .bind(parent_reference)
            .bind(i64::from(hints.subagent))
            .bind(now)
            .execute(&mut **transaction)
            .await?;
        }

        let edge_selection = selected
            .filter(|selection| selection.write_edge)
            .or(candidate_selection);
        let candidate_edge_increment = i64::from(
            edge_selection
                .as_ref()
                .is_some_and(|selection| selection.relation == RelationKind::Candidate),
        );
        if let Some(selection) = edge_selection {
            let (from_observation_id, to_observation_id) = if selection.reverse_edge {
                (observation_id.to_string(), selection.observation_id)
            } else {
                (selection.observation_id, observation_id.to_string())
            };
            sqlx::query(
                "INSERT INTO conversation_edges (id, cluster_id, from_observation_id, to_observation_id, relation_kind, confidence_millis, evidence_json, pinned, inference_version, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7, 0, 2, $8)",
            )
            .bind(Uuid::now_v7().to_string())
            .bind(cluster_id.to_string())
            .bind(from_observation_id)
            .bind(to_observation_id)
            .bind(relation_name(selection.relation))
            .bind(selection.confidence)
            .bind(serde_json::json!({
                "explicit_session": hints.session_id.is_some(),
                "explicit_parent": selection.direct_parent,
                "same_turn": selection.same_turn,
                "branch": hints.branch_id.is_some(),
                "compaction": hints.compaction,
                "subagent": hints.subagent && selection.direct_parent,
                "semantic_prefix": selection.semantic_prefix,
                "compaction_overlap": selection.compaction_overlap,
                "client_match": selection.client_match,
                "inference_version": 2
            }).to_string())
            .bind(now)
            .execute(&mut **transaction)
            .await?;
        }
        sqlx::query("UPDATE conversation_clusters SET created_at = CASE WHEN created_at > $1 THEN $1 ELSE created_at END, updated_at = CASE WHEN updated_at < $1 THEN $1 ELSE updated_at END, explicit_session_id = COALESCE(explicit_session_id, $2) WHERE id = $3")
            .bind(now)
            .bind(hints.session_id.as_deref())
            .bind(cluster_id.to_string())
            .execute(&mut **transaction)
            .await?;
        sqlx::query(
            "INSERT INTO conversation_key_clusters (key_id, cluster_id, explicit_session_id, updated_at, request_count, candidate_edge_count) VALUES ($1, $2, $3, $4, 1, $5) ON CONFLICT(key_id, cluster_id) DO UPDATE SET explicit_session_id = COALESCE(conversation_key_clusters.explicit_session_id, excluded.explicit_session_id), updated_at = CASE WHEN conversation_key_clusters.updated_at < excluded.updated_at THEN excluded.updated_at ELSE conversation_key_clusters.updated_at END, request_count = conversation_key_clusters.request_count + 1, candidate_edge_count = conversation_key_clusters.candidate_edge_count + excluded.candidate_edge_count",
        )
        .bind(key.key_id.to_string())
        .bind(cluster_id.to_string())
        .bind(hints.session_id.as_deref())
        .bind(now)
        .bind(candidate_edge_increment)
        .execute(&mut **transaction)
        .await?;
        if client_name != Some("session-title")
            && hints
                .session_name
                .as_deref()
                .is_none_or(|name| name.trim().is_empty())
        {
            enqueue_initial_session_title(transaction, key.tenant_id, key.key_id, cluster_id)
                .await?;
        }
        if let Some(request_created_at) = request_created_at {
            let attached = sqlx::query(
                "UPDATE request_records SET conversation_cluster_id = $1 WHERE id = $2 AND created_at = $3",
            )
                .bind(cluster_id.to_string())
                .bind(&request_id)
                .bind(request_created_at)
                .execute(&mut **transaction)
                .await?;
            let snapshot = sqlx::query("UPDATE terminal_projection_outbox SET session_id = $1 WHERE request_id = $2 AND created_at = $3")
                .bind(cluster_id.to_string()).bind(&request_id).bind(request_created_at).execute(&mut **transaction).await?;
            if attached.rows_affected() != 1 && snapshot.rows_affected() != 1 {
                return Err(AppError::internal("conversations_site_1").into());
            }
            if request_fact_exists {
                reclassify_request_session_in_transaction(
                    transaction,
                    parse_uuid(request_id.clone())?,
                )
                .await?;
            }
        }
        if let Some(turn_id) = hints.turn_id.as_deref() {
            reconcile_unresolved_explicit_parents_in_transaction(
                transaction,
                self.backend,
                &observation_id.to_string(),
                turn_id,
            )
            .await?;
        }
        Ok(cluster_id)
    }

    pub async fn attach_conversation_upstream_response(
        &self,
        request_id: Uuid,
        upstream_response_id: &str,
    ) -> Result<(), AppError> {
        let mut transaction = self.begin_write_transaction().await?;
        attach_conversation_upstream_response_in_transaction(
            &mut transaction,
            self.backend,
            request_id,
            upstream_response_id,
        )
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    pub async fn conversation_clusters(
        &self,
        key_id: Uuid,
        filter: ConversationListFilter,
    ) -> Result<Vec<ConversationClusterView>, AppError> {
        let limit = filter.limit.clamp(1, 100);
        let key_id = key_id.to_string();
        let rows = if let (Some(before_updated_at), Some(before_cluster_id)) =
            (filter.before_updated_at, filter.before_cluster_id)
        {
            sqlx::query(
                "SELECT p.cluster_id AS id, p.explicit_session_id, p.updated_at, p.request_count, p.candidate_edge_count FROM conversation_key_clusters p WHERE p.key_id = $1 AND (p.updated_at < $2 OR (p.updated_at = $2 AND p.cluster_id < $3)) ORDER BY p.updated_at DESC, p.cluster_id DESC LIMIT $4",
            )
            .bind(&key_id)
            .bind(before_updated_at)
            .bind(before_cluster_id.to_string())
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query(
                "SELECT p.cluster_id AS id, p.explicit_session_id, p.updated_at, p.request_count, p.candidate_edge_count FROM conversation_key_clusters p WHERE p.key_id = $1 ORDER BY p.updated_at DESC, p.cluster_id DESC LIMIT $2",
            )
            .bind(&key_id)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
        };
        rows.into_iter()
            .map(|row| {
                Ok(ConversationClusterView {
                    cluster_id: parse_uuid(row.try_get("id")?)?,
                    explicit_session_id: row.try_get("explicit_session_id")?,
                    updated_at: row.try_get("updated_at")?,
                    request_count: row.try_get("request_count")?,
                    candidate_edge_count: row.try_get("candidate_edge_count")?,
                })
            })
            .collect()
    }

    pub async fn conversation_cluster_detail(
        &self,
        key_id: Uuid,
        cluster_id: Uuid,
        filter: ConversationDetailFilter,
    ) -> Result<ConversationClusterDetail, AppError> {
        let key_id = key_id.to_string();
        let cluster_id = cluster_id.to_string();
        let cluster_row = sqlx::query(
            "SELECT p.cluster_id AS id, p.explicit_session_id, p.updated_at, p.request_count, p.candidate_edge_count FROM conversation_key_clusters p WHERE p.key_id = $1 AND p.cluster_id = $2",
        )
        .bind(&key_id)
        .bind(&cluster_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::NotFound)?;
        let cluster = ConversationClusterView {
            cluster_id: parse_uuid(cluster_row.try_get("id")?)?,
            explicit_session_id: cluster_row.try_get("explicit_session_id")?,
            updated_at: cluster_row.try_get("updated_at")?,
            request_count: cluster_row.try_get("request_count")?,
            candidate_edge_count: cluster_row.try_get("candidate_edge_count")?,
        };
        let limit = filter.limit.clamp(1, 200);
        // Bound each ordered source before touching archive spools. Applying
        // only the outer LIMIT makes large sessions hash-join the entire spool
        // tables and sort the complete history just to display one page.
        // Keep the cursor inside both sources as well as on the merged page.
        let request_rows = if let (Some(before_created_at), Some(before_request_id)) =
            (filter.before_created_at, filter.before_request_id)
        {
            sqlx::query(
                "SELECT requests.*, observation.session_name, observation.trace_id, observation.span_id, observation.parent_span_id, observation.agent_id, observation.parent_agent_id, observation.task_kind, observation.labels_json, observation.metadata_source, observation.explicit_session_id, observation.turn_id, observation.parent_turn_id, observation.upstream_response_id, observation.branch_id, observation.compaction, observation.client_name FROM (SELECT r.id, r.created_at, r.completed_at, CAST(NULL AS BIGINT) AS source_completed_at, r.protocol, r.model, r.upstream_model, r.upstream_account_id, r.model_route_id AS route_id, r.status_code, r.duration_ms, r.first_output_ms, r.generation_duration_ms, r.usage_basis, r.input_tokens, r.cached_input_tokens, r.cache_write_tokens, r.output_tokens, r.billed_units, r.billing_unit, r.cost_micros, r.currency, r.error_code, r.terminal_cause_code, r.in_flight_stage, CASE WHEN request_spool.state = 'uploading' OR spool.state = 'uploading' THEN 'uploading' WHEN request_spool.state = 'pending' OR spool.state = 'pending' THEN 'pending' WHEN request_spool.state = 'capturing' OR spool.state = 'capturing' OR r.completed_at IS NULL THEN 'capturing' WHEN request_spool.state = 'gap' OR spool.state = 'gap' OR r.request_object LIKE 'gap://%' OR r.response_object IS NULL OR r.response_object LIKE 'gap://%' THEN 'gap' ELSE 'bound' END AS archive_state, 'live' AS source_kind, 'native' AS provenance_kind, CAST(0 AS BIGINT) AS unlinked, NULL AS archive_source, NULL AS external_request_id, r.conversation_cluster_id AS session_id, 'confirmed' AS session_association FROM (SELECT * FROM request_records WHERE key_id = $1 AND conversation_cluster_id = $2 AND (created_at < $3 OR (created_at = $3 AND id < $4)) ORDER BY created_at DESC, id DESC LIMIT $5) r LEFT JOIN request_archive_spools request_spool ON request_spool.request_id = r.id AND request_spool.tenant_id = r.tenant_id AND request_spool.reservation_id = r.reservation_id LEFT JOIN response_archive_spools spool ON spool.request_id = r.id AND spool.tenant_id = r.tenant_id AND spool.reservation_id = r.reservation_id WHERE r.key_id = $1 AND r.conversation_cluster_id = $2 UNION ALL SELECT archive_request_id AS id, source_started_at AS created_at, CAST(NULL AS BIGINT) AS completed_at, source_completed_at, protocol, model, CAST(NULL AS TEXT) AS upstream_model, CAST(NULL AS TEXT) AS upstream_account_id, CAST(NULL AS TEXT) AS route_id, status_code, duration_ms, CAST(NULL AS BIGINT) AS first_output_ms, CAST(NULL AS BIGINT) AS generation_duration_ms, CAST(NULL AS TEXT) AS usage_basis, input_tokens, CAST(0 AS BIGINT) AS cached_input_tokens, CAST(0 AS BIGINT) AS cache_write_tokens, output_tokens, CAST(0 AS BIGINT) AS billed_units, CAST(NULL AS TEXT) AS billing_unit, CAST(0 AS BIGINT) AS cost_micros, NULL AS currency, error_code, CAST(NULL AS TEXT) AS terminal_cause_code, CAST(NULL AS TEXT) AS in_flight_stage, CASE WHEN request_object IS NULL OR request_object LIKE 'gap://%' OR response_object IS NULL OR response_object LIKE 'gap://%' THEN 'gap' ELSE 'bound' END AS archive_state, 'session_archive' AS source_kind, 'archive_unlinked' AS provenance_kind, CAST(1 AS BIGINT) AS unlinked, source AS archive_source, external_request_id, conversation_cluster_id AS session_id, 'unlinked' AS session_association FROM (SELECT * FROM session_archive_unlinked_requests WHERE key_id = $1 AND conversation_cluster_id = $2 AND (source_started_at < $3 OR (source_started_at = $3 AND archive_request_id < $4)) ORDER BY source_started_at DESC, archive_request_id DESC LIMIT $5) archive_page) requests LEFT JOIN conversation_observations observation ON observation.request_id = requests.id AND observation.key_id = $1 AND observation.cluster_id = requests.session_id WHERE requests.created_at < $3 OR (requests.created_at = $3 AND requests.id < $4) ORDER BY requests.created_at DESC, requests.id DESC LIMIT $5",
            )
            .bind(&key_id)
            .bind(&cluster_id)
            .bind(before_created_at)
            .bind(before_request_id.to_string())
            .bind(limit + 1)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query(
                "SELECT requests.*, observation.session_name, observation.trace_id, observation.span_id, observation.parent_span_id, observation.agent_id, observation.parent_agent_id, observation.task_kind, observation.labels_json, observation.metadata_source, observation.explicit_session_id, observation.turn_id, observation.parent_turn_id, observation.upstream_response_id, observation.branch_id, observation.compaction, observation.client_name FROM (SELECT r.id, r.created_at, r.completed_at, CAST(NULL AS BIGINT) AS source_completed_at, r.protocol, r.model, r.upstream_model, r.upstream_account_id, r.model_route_id AS route_id, r.status_code, r.duration_ms, r.first_output_ms, r.generation_duration_ms, r.usage_basis, r.input_tokens, r.cached_input_tokens, r.cache_write_tokens, r.output_tokens, r.billed_units, r.billing_unit, r.cost_micros, r.currency, r.error_code, r.terminal_cause_code, r.in_flight_stage, CASE WHEN request_spool.state = 'uploading' OR spool.state = 'uploading' THEN 'uploading' WHEN request_spool.state = 'pending' OR spool.state = 'pending' THEN 'pending' WHEN request_spool.state = 'capturing' OR spool.state = 'capturing' OR r.completed_at IS NULL THEN 'capturing' WHEN request_spool.state = 'gap' OR spool.state = 'gap' OR r.request_object LIKE 'gap://%' OR r.response_object IS NULL OR r.response_object LIKE 'gap://%' THEN 'gap' ELSE 'bound' END AS archive_state, 'live' AS source_kind, 'native' AS provenance_kind, CAST(0 AS BIGINT) AS unlinked, NULL AS archive_source, NULL AS external_request_id, r.conversation_cluster_id AS session_id, 'confirmed' AS session_association FROM (SELECT * FROM request_records WHERE key_id = $1 AND conversation_cluster_id = $2 ORDER BY created_at DESC, id DESC LIMIT $3) r LEFT JOIN request_archive_spools request_spool ON request_spool.request_id = r.id AND request_spool.tenant_id = r.tenant_id AND request_spool.reservation_id = r.reservation_id LEFT JOIN response_archive_spools spool ON spool.request_id = r.id AND spool.tenant_id = r.tenant_id AND spool.reservation_id = r.reservation_id WHERE r.key_id = $1 AND r.conversation_cluster_id = $2 UNION ALL SELECT archive_request_id AS id, source_started_at AS created_at, CAST(NULL AS BIGINT) AS completed_at, source_completed_at, protocol, model, CAST(NULL AS TEXT) AS upstream_model, CAST(NULL AS TEXT) AS upstream_account_id, CAST(NULL AS TEXT) AS route_id, status_code, duration_ms, CAST(NULL AS BIGINT) AS first_output_ms, CAST(NULL AS BIGINT) AS generation_duration_ms, CAST(NULL AS TEXT) AS usage_basis, input_tokens, CAST(0 AS BIGINT) AS cached_input_tokens, CAST(0 AS BIGINT) AS cache_write_tokens, output_tokens, CAST(0 AS BIGINT) AS billed_units, CAST(NULL AS TEXT) AS billing_unit, CAST(0 AS BIGINT) AS cost_micros, NULL AS currency, error_code, CAST(NULL AS TEXT) AS terminal_cause_code, CAST(NULL AS TEXT) AS in_flight_stage, CASE WHEN request_object IS NULL OR request_object LIKE 'gap://%' OR response_object IS NULL OR response_object LIKE 'gap://%' THEN 'gap' ELSE 'bound' END AS archive_state, 'session_archive' AS source_kind, 'archive_unlinked' AS provenance_kind, CAST(1 AS BIGINT) AS unlinked, source AS archive_source, external_request_id, conversation_cluster_id AS session_id, 'unlinked' AS session_association FROM (SELECT * FROM session_archive_unlinked_requests WHERE key_id = $1 AND conversation_cluster_id = $2 ORDER BY source_started_at DESC, archive_request_id DESC LIMIT $3) archive_page) requests LEFT JOIN conversation_observations observation ON observation.request_id = requests.id AND observation.key_id = $1 AND observation.cluster_id = requests.session_id ORDER BY requests.created_at DESC, requests.id DESC LIMIT $3",
            )
            .bind(&key_id)
            .bind(&cluster_id)
            .bind(limit + 1)
            .fetch_all(&self.pool)
            .await?
        };
        let mut requests = conversation_request_views(request_rows)?;
        let has_more = requests.len() > limit as usize;
        if has_more {
            requests.truncate(limit as usize);
        }
        let next_cursor = has_more.then(|| {
            let oldest = requests
                .last()
                .expect("a page with another row has at least one returned request");
            ConversationCursor {
                before_created_at: oldest.request.created_at,
                before_request_id: oldest.request.request_id,
            }
        });
        let page_request_ids: Vec<String> = requests
            .iter()
            .map(|request| request.request.request_id.to_string())
            .collect();
        requests.reverse();

        let edge_limit = (limit * 2).clamp(1, 400);
        let edge_rows = if page_request_ids.is_empty() {
            Vec::new()
        } else {
            let request_ids_json = serde_json::to_string(&page_request_ids)
                .map_err(|_| AppError::internal("conversations_site_13"))?;
            let query = match self.backend {
                DatabaseBackend::PostgreSql => {
                    "SELECT source_o.request_id AS from_request_id, target_o.request_id AS to_request_id, e.relation_kind, e.confidence_millis, e.evidence_json FROM conversation_edges e JOIN conversation_observations target_o ON target_o.id = e.to_observation_id LEFT JOIN conversation_observations source_o ON source_o.id = e.from_observation_id WHERE e.cluster_id = $1 AND target_o.key_id = $2 AND (source_o.key_id = $3 OR source_o.id IS NULL) AND target_o.request_id IN (SELECT jsonb_array_elements_text(CAST($4 AS JSONB))) ORDER BY target_o.created_at ASC, target_o.id ASC, e.id ASC LIMIT $5"
                }
                DatabaseBackend::Sqlite => {
                    "SELECT source_o.request_id AS from_request_id, target_o.request_id AS to_request_id, e.relation_kind, e.confidence_millis, e.evidence_json FROM conversation_edges e JOIN conversation_observations target_o ON target_o.id = e.to_observation_id LEFT JOIN conversation_observations source_o ON source_o.id = e.from_observation_id WHERE e.cluster_id = $1 AND target_o.key_id = $2 AND (source_o.key_id = $3 OR source_o.id IS NULL) AND target_o.request_id IN (SELECT value FROM json_each($4)) ORDER BY target_o.created_at ASC, target_o.id ASC, e.id ASC LIMIT $5"
                }
            };
            sqlx::query(query)
                .bind(&cluster_id)
                .bind(&key_id)
                .bind(&key_id)
                .bind(request_ids_json)
                .bind(edge_limit + 1)
                .fetch_all(&self.pool)
                .await?
        };
        let mut edges = edge_rows
            .into_iter()
            .map(|row| {
                let from_request_id: Option<String> = row.try_get("from_request_id")?;
                let evidence: String = row.try_get("evidence_json")?;
                let confidence: i64 = row.try_get("confidence_millis")?;
                Ok(ConversationEdgeView {
                    from_request_id: from_request_id.map(parse_uuid).transpose()?,
                    to_request_id: parse_uuid(row.try_get("to_request_id")?)?,
                    relation: row.try_get("relation_kind")?,
                    confidence: confidence as f64 / 1_000.0,
                    evidence: serde_json::from_str(&evidence).unwrap_or(serde_json::Value::Null),
                })
            })
            .collect::<Result<Vec<_>, AppError>>()?;
        let edges_truncated = edges.len() > edge_limit as usize;
        if edges_truncated {
            edges.truncate(edge_limit as usize);
        }
        Ok(ConversationClusterDetail {
            cluster,
            requests,
            edges,
            has_more,
            next_cursor,
            edges_truncated,
        })
    }
}

async fn fetch_structured_conversation_candidate(
    transaction: &mut Transaction<'_, Any>,
    tenant_id: &str,
    principal_id: &str,
    key_id: &str,
    hints: &ConversationHints,
    observed_at: i64,
) -> Result<Option<AnyRow>, AppError> {
    if let Some(parent_turn_id) = hints.parent_turn_id.as_deref() {
        let by_turn = sqlx::query(
            "SELECT o.id, o.cluster_id, o.atom_hashes_json, o.leaf_node_hash, o.explicit_session_id, o.turn_id, o.upstream_response_id, o.branch_id, o.client_name, o.created_at FROM conversation_observations o JOIN conversation_clusters c ON c.id = o.cluster_id WHERE c.tenant_id = $1 AND c.principal_id = $2 AND o.key_id = $3 AND o.turn_id = $4 ORDER BY o.created_at DESC LIMIT 1",
        )
        .bind(tenant_id)
        .bind(principal_id)
        .bind(key_id)
        .bind(parent_turn_id)
        .fetch_optional(&mut **transaction)
        .await?;
        let by_response = sqlx::query(
            "SELECT o.id, o.cluster_id, o.atom_hashes_json, o.leaf_node_hash, o.explicit_session_id, o.turn_id, o.upstream_response_id, o.branch_id, o.client_name, o.created_at FROM conversation_observations o JOIN conversation_clusters c ON c.id = o.cluster_id WHERE c.tenant_id = $1 AND c.principal_id = $2 AND o.key_id = $3 AND o.upstream_response_id = $4 ORDER BY o.created_at DESC LIMIT 1",
        )
        .bind(tenant_id)
        .bind(principal_id)
        .bind(key_id)
        .bind(parent_turn_id)
        .fetch_optional(&mut **transaction)
        .await?;
        return newest_conversation_candidate(by_turn, by_response);
    }

    if let Some(turn_id) = hints.turn_id.as_deref() {
        let candidate = sqlx::query(
            "SELECT o.id, o.cluster_id, o.atom_hashes_json, o.leaf_node_hash, o.explicit_session_id, o.turn_id, o.upstream_response_id, o.branch_id, o.client_name, o.created_at FROM conversation_observations o JOIN conversation_clusters c ON c.id = o.cluster_id WHERE c.tenant_id = $1 AND c.principal_id = $2 AND o.key_id = $3 AND o.turn_id = $4 AND o.created_at <= $5 ORDER BY o.created_at DESC LIMIT 1",
        )
        .bind(tenant_id)
        .bind(principal_id)
        .bind(key_id)
        .bind(turn_id)
        .bind(observed_at)
        .fetch_optional(&mut **transaction)
        .await?;
        if candidate.is_some() {
            return Ok(candidate);
        }
    }

    if let Some(session_id) = hints.session_id.as_deref() {
        let causal = sqlx::query(
            "SELECT o.id, o.cluster_id, o.atom_hashes_json, o.leaf_node_hash, o.explicit_session_id, o.turn_id, o.parent_turn_id, o.compaction, o.upstream_response_id, o.branch_id, o.client_name, o.created_at FROM conversation_observations o JOIN conversation_clusters c ON c.id = o.cluster_id WHERE c.tenant_id = $1 AND c.principal_id = $2 AND o.key_id = $3 AND o.explicit_session_id = $4 AND o.created_at <= $5 ORDER BY o.created_at DESC LIMIT 1",
        )
        .bind(tenant_id)
        .bind(principal_id)
        .bind(key_id)
        .bind(session_id)
        .bind(observed_at)
        .fetch_optional(&mut **transaction)
        .await?;
        if causal.is_some() {
            return Ok(causal);
        }

        // Archive projection can commit a later source-time observation first.
        let future = sqlx::query(
            "SELECT o.id, o.cluster_id, o.atom_hashes_json, o.leaf_node_hash, o.explicit_session_id, o.turn_id, o.parent_turn_id, o.compaction, o.upstream_response_id, o.branch_id, o.client_name, o.created_at FROM conversation_observations o JOIN conversation_clusters c ON c.id = o.cluster_id WHERE c.tenant_id = $1 AND c.principal_id = $2 AND o.key_id = $3 AND o.explicit_session_id = $4 AND o.created_at > $5 ORDER BY o.created_at DESC LIMIT 1",
        )
        .bind(tenant_id)
        .bind(principal_id)
        .bind(key_id)
        .bind(session_id)
        .bind(observed_at)
        .fetch_optional(&mut **transaction)
        .await?;
        return Ok(future);
    }

    Ok(None)
}

fn newest_conversation_candidate(
    left: Option<AnyRow>,
    right: Option<AnyRow>,
) -> Result<Option<AnyRow>, AppError> {
    match (left, right) {
        (Some(left), Some(right)) => {
            let left_created_at: i64 = left.try_get("created_at")?;
            let right_created_at: i64 = right.try_get("created_at")?;
            Ok(Some(if left_created_at >= right_created_at {
                left
            } else {
                right
            }))
        }
        (left @ Some(_), None) => Ok(left),
        (None, right @ Some(_)) => Ok(right),
        (None, None) => Ok(None),
    }
}

async fn emit_conversation_projected_event_in_transaction(
    transaction: &mut Transaction<'_, Any>,
    request_id: Uuid,
    tenant_id: Uuid,
    key_id: Uuid,
) -> Result<(), AppError> {
    // Take the cursor timestamp only after semantic materialization and the
    // outbox acknowledgement have succeeded. Reusing the projector's lease
    // timestamp would let a live cursor advance past this still-uncommitted
    // event while the more expensive projection writes are running.
    let request_id = request_id.to_string();
    let tenant_id = tenant_id.to_string();
    let key_id = key_id.to_string();
    let event =
        allocate_request_event_cursor(transaction, unix_millis(), &tenant_id, &key_id, &request_id)
            .await?;
    // Semantic completion does not imply financial settlement. An authenticated
    // operational receipt may finish a request whose usage remains unknown.
    // Keep the event's accounting fields sourced exclusively from the request;
    // use the receipt only for runtime outcome and eligibility.
    let inserted = sqlx::query(
        r#"INSERT INTO request_events (
               event_id, tenant_id, key_id, request_id, event_at, event_kind,
               protocol, model, status_code, duration_ms, input_tokens,
               output_tokens, cost_micros, error_code)
           SELECT $1, r.tenant_id, r.key_id, r.id, $2, 'projected',
                  r.protocol, r.model,
                  COALESCE(o.status_code, r.status_code),
                  COALESCE(o.duration_ms, r.duration_ms),
                  r.input_tokens, r.output_tokens, r.cost_micros,
                  CASE WHEN o.request_id IS NOT NULL THEN o.error_code ELSE r.error_code END
             FROM request_records r
             LEFT JOIN proxy_operational_terminals o
               ON o.request_id = r.id AND o.request_created_at = r.created_at
              AND o.tenant_id = r.tenant_id AND o.key_id = r.key_id
              AND o.reservation_id = r.reservation_id
            WHERE r.id = $3 AND r.tenant_id = $4 AND r.key_id = $5
              AND (r.completed_at IS NOT NULL OR o.request_id IS NOT NULL)
           UNION ALL
           SELECT $1, tenant_id, key_id, request_id, $2, 'projected',
                  protocol, model, status_code, duration_ms, input_tokens,
                  output_tokens, cost_micros, NULLIF(error_code, '')
             FROM terminal_projection_outbox
            WHERE request_id = $3 AND tenant_id = $4 AND key_id = $5
              AND NOT EXISTS (SELECT 1 FROM request_records WHERE id = $3)"#,
    )
    .bind(event.event_id)
    .bind(event.event_at)
    .bind(request_id)
    .bind(tenant_id)
    .bind(key_id)
    .execute(&mut **transaction)
    .await?;
    if inserted.rows_affected() != 1 {
        return Err(AppError::internal(
            "conversations_emit_conversation_projected_event_in_transaction_1",
        ));
    }
    Ok(())
}

async fn lock_conversation_references_in_transaction(
    transaction: &mut Transaction<'_, Any>,
    backend: DatabaseBackend,
    tenant_id: &str,
    principal_id: &str,
    key_id: &str,
    references: &[Option<&str>],
) -> Result<(), AppError> {
    if matches!(backend, DatabaseBackend::PostgreSql) {
        let mut references = references
            .iter()
            .filter_map(|value| *value)
            .collect::<Vec<_>>();
        references.sort_unstable();
        references.dedup();
        for reference in references {
            sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, $2))")
                .bind(format!("{tenant_id}:{principal_id}:{key_id}:{reference}"))
                .bind(CONVERSATION_REFERENCE_LOCK_SEED)
                .execute(&mut **transaction)
                .await?;
        }
    }
    Ok(())
}

async fn lock_selected_conversation_cluster_in_transaction(
    transaction: &mut Transaction<'_, Any>,
    backend: DatabaseBackend,
    observation_id: &str,
) -> Result<Uuid, AppError> {
    let statement = if matches!(backend, DatabaseBackend::PostgreSql) {
        "SELECT c.id FROM conversation_observations o JOIN conversation_clusters c ON c.id = o.cluster_id WHERE o.id = $1 FOR UPDATE OF o, c"
    } else {
        "SELECT c.id FROM conversation_observations o JOIN conversation_clusters c ON c.id = o.cluster_id WHERE o.id = $1"
    };
    let cluster_id: String = sqlx::query_scalar(statement)
        .bind(observation_id)
        .fetch_one(&mut **transaction)
        .await?;
    parse_uuid(cluster_id)
}

async fn lock_reconciliation_clusters_in_transaction(
    transaction: &mut Transaction<'_, Any>,
    backend: DatabaseBackend,
    parent_observation_id: &str,
    child_observation_id: &str,
) -> Result<(String, String), AppError> {
    let target = lock_selected_conversation_cluster_in_transaction(
        transaction,
        backend,
        parent_observation_id,
    )
    .await?
    .to_string();
    let source = lock_selected_conversation_cluster_in_transaction(
        transaction,
        backend,
        child_observation_id,
    )
    .await?
    .to_string();
    Ok((target, source))
}

async fn merge_conversation_clusters_in_transaction(
    transaction: &mut Transaction<'_, Any>,
    backend: DatabaseBackend,
    tenant_id: &str,
    principal_id: &str,
    key_id: &str,
    parent_observation_id: &str,
    child_observation_id: &str,
) -> Result<String, AppError> {
    // An unresolved explicit-parent observation is born in a fresh cluster,
    // so no other reconciliation can use its source in the opposite direction.
    // Normal descendants lock that source row before insertion; locking target
    // then source therefore either moves them too or waits for their commit.
    let (target_cluster_id, source_cluster_id) = lock_reconciliation_clusters_in_transaction(
        transaction,
        backend,
        parent_observation_id,
        child_observation_id,
    )
    .await?;
    let owned_clusters: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM conversation_clusters WHERE (id = $1 OR id = $2) AND tenant_id = $3 AND principal_id = $4",
    )
    .bind(&target_cluster_id)
    .bind(&source_cluster_id)
    .bind(tenant_id)
    .bind(principal_id)
    .fetch_one(&mut **transaction)
    .await?;
    let expected = if target_cluster_id == source_cluster_id {
        1
    } else {
        2
    };
    if owned_clusters != expected {
        return Err(AppError::NotFound);
    }
    if target_cluster_id == source_cluster_id {
        return Ok(target_cluster_id);
    }

    let source_projection = sqlx::query(
        "SELECT explicit_session_id, updated_at, request_count, candidate_edge_count FROM conversation_key_clusters WHERE key_id = $1 AND cluster_id = $2",
    )
    .bind(key_id)
    .bind(&source_cluster_id)
    .fetch_one(&mut **transaction)
    .await?;
    let source_explicit_session_id: Option<String> =
        source_projection.try_get("explicit_session_id")?;
    let source_updated_at: i64 = source_projection.try_get("updated_at")?;
    let source_request_count: i64 = source_projection.try_get("request_count")?;
    let source_candidate_edge_count: i64 = source_projection.try_get("candidate_edge_count")?;
    let target_projection = sqlx::query(
        "UPDATE conversation_key_clusters SET explicit_session_id = COALESCE(explicit_session_id, $1), updated_at = CASE WHEN updated_at < $2 THEN $2 ELSE updated_at END, request_count = request_count + $3, candidate_edge_count = candidate_edge_count + $4 WHERE key_id = $5 AND cluster_id = $6",
    )
    .bind(source_explicit_session_id.as_deref())
    .bind(source_updated_at)
    .bind(source_request_count)
    .bind(source_candidate_edge_count)
    .bind(key_id)
    .bind(&target_cluster_id)
    .execute(&mut **transaction)
    .await?;
    if target_projection.rows_affected() != 1 {
        return Err(AppError::internal(
            "conversations_merge_conversation_clusters_in_transaction_1",
        ));
    }

    // Migration 0031 intentionally retained inert historical cross-key edges.
    // Move only edges whose endpoints both belong to this stable key; foreign
    // evidence remains attached to the legacy source cluster.
    sqlx::query(
        "UPDATE conversation_edges SET cluster_id = $1 WHERE cluster_id = $2 AND to_observation_id IN (SELECT id FROM conversation_observations WHERE cluster_id = $2 AND key_id = $3) AND (from_observation_id IS NULL OR from_observation_id IN (SELECT id FROM conversation_observations WHERE cluster_id = $2 AND key_id = $3))",
    )
    .bind(&target_cluster_id)
    .bind(&source_cluster_id)
    .bind(key_id)
    .execute(&mut **transaction)
    .await?;
    let moved_observations = sqlx::query(
        "UPDATE conversation_observations SET cluster_id = $1 WHERE cluster_id = $2 AND key_id = $3",
    )
    .bind(&target_cluster_id)
    .bind(&source_cluster_id)
    .bind(key_id)
    .execute(&mut **transaction)
    .await?;
    if moved_observations.rows_affected() == 0 {
        return Err(AppError::internal(
            "conversations_merge_conversation_clusters_in_transaction_2",
        ));
    }
    sqlx::query(
        "UPDATE request_records SET conversation_cluster_id = $1 WHERE tenant_id = $2 AND key_id = $3 AND conversation_cluster_id = $4",
    )
    .bind(&target_cluster_id)
    .bind(tenant_id)
    .bind(key_id)
    .bind(&source_cluster_id)
    .execute(&mut **transaction)
    .await?;
    sqlx::query("UPDATE terminal_projection_outbox SET session_id = $1 WHERE tenant_id = $2 AND key_id = $3 AND session_id = $4")
        .bind(&target_cluster_id).bind(tenant_id).bind(key_id).bind(&source_cluster_id)
        .execute(&mut **transaction).await?;
    let moved_archive_requests = sqlx::query(
        "UPDATE session_archive_unlinked_requests SET conversation_cluster_id = $1 WHERE tenant_id = $2 AND key_id = $3 AND conversation_cluster_id = $4",
    )
    .bind(&target_cluster_id)
    .bind(tenant_id)
    .bind(key_id)
    .bind(&source_cluster_id)
    .execute(&mut **transaction)
    .await?;
    if moved_archive_requests.rows_affected() != 0 {
        sqlx::query(
            "DELETE FROM session_archive_totals WHERE tenant_id = $1 AND key_id = $2 AND (session_id = $3 OR session_id = $4)",
        )
        .bind(tenant_id)
        .bind(key_id)
        .bind(&source_cluster_id)
        .bind(&target_cluster_id)
        .execute(&mut **transaction)
        .await?;
        sqlx::query(
            "INSERT INTO session_archive_totals (tenant_id, key_id, session_id, last_activity_at, requests, errors, input_tokens, output_tokens, duration_count, duration_sum_ms) SELECT tenant_id, key_id, conversation_cluster_id, MAX(source_started_at), COUNT(*), SUM(CASE WHEN status_code IS NOT NULL AND (status_code < 200 OR status_code >= 400) THEN 1 ELSE 0 END), SUM(input_tokens), SUM(output_tokens), SUM(CASE WHEN duration_ms IS NULL THEN 0 ELSE 1 END), SUM(COALESCE(duration_ms, 0)) FROM session_archive_unlinked_requests WHERE tenant_id = $1 AND key_id = $2 AND conversation_cluster_id = $3 GROUP BY tenant_id, key_id, conversation_cluster_id",
        )
        .bind(tenant_id)
        .bind(key_id)
        .bind(&target_cluster_id)
        .execute(&mut **transaction)
        .await?;
    }
    merge_request_session_projection_in_transaction(
        transaction,
        tenant_id,
        key_id,
        &source_cluster_id,
        &target_cluster_id,
    )
    .await?;
    let target_cluster = sqlx::query(
        "UPDATE conversation_clusters SET created_at = (SELECT MIN(created_at) FROM conversation_observations WHERE cluster_id = $1), updated_at = (SELECT MAX(created_at) FROM conversation_observations WHERE cluster_id = $1), explicit_session_id = COALESCE(explicit_session_id, $2) WHERE id = $1",
    )
    .bind(&target_cluster_id)
    .bind(source_explicit_session_id.as_deref())
    .execute(&mut **transaction)
    .await?;
    if target_cluster.rows_affected() != 1 {
        return Err(AppError::internal("conversations_site_2"));
    }
    merge_session_reading_in_transaction(
        transaction,
        tenant_id,
        key_id,
        &source_cluster_id,
        &target_cluster_id,
    )
    .await?;
    sqlx::query("DELETE FROM conversation_key_clusters WHERE key_id = $1 AND cluster_id = $2")
        .bind(key_id)
        .bind(&source_cluster_id)
        .execute(&mut **transaction)
        .await?;
    let deleted_cluster = sqlx::query(
        "DELETE FROM conversation_clusters WHERE id = $1 AND NOT EXISTS (SELECT 1 FROM conversation_observations WHERE cluster_id = $1)",
    )
        .bind(&source_cluster_id)
        .execute(&mut **transaction)
        .await?;
    if deleted_cluster.rows_affected() == 0 {
        let retained_source = sqlx::query(
            "UPDATE conversation_clusters SET created_at = (SELECT MIN(created_at) FROM conversation_observations WHERE cluster_id = $1), updated_at = (SELECT MAX(created_at) FROM conversation_observations WHERE cluster_id = $1), explicit_session_id = (SELECT MIN(explicit_session_id) FROM conversation_observations WHERE cluster_id = $1) WHERE id = $1",
        )
        .bind(&source_cluster_id)
        .execute(&mut **transaction)
        .await?;
        if retained_source.rows_affected() != 1 {
            return Err(AppError::internal("conversations_site_3"));
        }
    }
    Ok(target_cluster_id)
}

async fn reconcile_unresolved_explicit_parents_in_transaction(
    transaction: &mut Transaction<'_, Any>,
    backend: DatabaseBackend,
    parent_observation_id: &str,
    parent_reference: &str,
) -> Result<(), AppError> {
    let parent = sqlx::query(
        "SELECT o.key_id, o.branch_id, c.tenant_id, c.principal_id FROM conversation_observations o JOIN conversation_clusters c ON c.id = o.cluster_id WHERE o.id = $1",
    )
    .bind(parent_observation_id)
    .fetch_one(&mut **transaction)
    .await?;
    let tenant_id: String = parent.try_get("tenant_id")?;
    let principal_id: String = parent.try_get("principal_id")?;
    let key_id: String = parent.try_get("key_id")?;
    let parent_branch_id: Option<String> = parent.try_get("branch_id")?;
    let unresolved = sqlx::query(
        "SELECT u.child_observation_id, u.subagent, child.explicit_session_id, child.branch_id, child.compaction FROM conversation_unresolved_explicit_parents u JOIN conversation_observations child ON child.id = u.child_observation_id AND child.key_id = u.key_id WHERE u.tenant_id = $1 AND u.principal_id = $2 AND u.key_id = $3 AND u.parent_reference = $4 ORDER BY u.created_at ASC, u.child_observation_id ASC",
    )
    .bind(&tenant_id)
    .bind(&principal_id)
    .bind(&key_id)
    .bind(parent_reference)
    .fetch_all(&mut **transaction)
    .await?;
    for child in unresolved {
        let child_observation_id: String = child.try_get("child_observation_id")?;
        if child_observation_id == parent_observation_id {
            sqlx::query(
                "DELETE FROM conversation_unresolved_explicit_parents WHERE child_observation_id = $1",
            )
            .bind(&child_observation_id)
            .execute(&mut **transaction)
            .await?;
            continue;
        }
        let child_branch_id: Option<String> = child.try_get("branch_id")?;
        let child_explicit_session_id: Option<String> = child.try_get("explicit_session_id")?;
        let subagent = child.try_get::<i64, _>("subagent")? != 0;
        let compaction = child.try_get::<i64, _>("compaction")? != 0;
        let cluster_id = merge_conversation_clusters_in_transaction(
            transaction,
            backend,
            &tenant_id,
            &principal_id,
            &key_id,
            parent_observation_id,
            &child_observation_id,
        )
        .await?;
        let relation = if subagent {
            RelationKind::Subagent
        } else if compaction {
            RelationKind::Compacts
        } else if child_branch_id.is_some()
            && parent_branch_id.is_some()
            && child_branch_id != parent_branch_id
        {
            RelationKind::Branch
        } else {
            RelationKind::Continues
        };
        sqlx::query(
            "INSERT INTO conversation_edges (id, cluster_id, from_observation_id, to_observation_id, relation_kind, confidence_millis, evidence_json, pinned, inference_version, created_at) VALUES ($1, $2, $3, $4, $5, 995, $6, 0, 2, $7)",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(cluster_id)
        .bind(parent_observation_id)
        .bind(&child_observation_id)
        .bind(relation_name(relation))
        .bind(serde_json::json!({
            "explicit_session": child_explicit_session_id.is_some(),
            "explicit_parent": true,
            "same_turn": false,
            "branch": child_branch_id.is_some(),
            "compaction": compaction,
            "subagent": subagent,
            "semantic_prefix": false,
            "compaction_overlap": false,
            "client_match": false,
            "reconciled": true,
            "inference_version": 2
        }).to_string())
        .bind(unix_millis())
        .execute(&mut **transaction)
        .await?;
        sqlx::query(
            "DELETE FROM conversation_unresolved_explicit_parents WHERE child_observation_id = $1 AND tenant_id = $2 AND principal_id = $3 AND key_id = $4 AND parent_reference = $5",
        )
        .bind(&child_observation_id)
        .bind(&tenant_id)
        .bind(&principal_id)
        .bind(&key_id)
        .bind(parent_reference)
        .execute(&mut **transaction)
        .await?;
    }
    Ok(())
}

pub(crate) async fn attach_conversation_upstream_response_in_transaction(
    transaction: &mut Transaction<'_, Any>,
    backend: DatabaseBackend,
    request_id: Uuid,
    upstream_response_id: &str,
) -> Result<(), AppError> {
    // This path can reconcile clusters and merge their session projections. Acquire the shared
    // statistics lock before reading or locking conversation state to preserve the global order.
    lock_request_stats_projection_writer_in_transaction(transaction).await?;
    let upstream_response_id = upstream_response_id.trim();
    if upstream_response_id.is_empty()
        || upstream_response_id.len() > 256
        || upstream_response_id.chars().any(char::is_control)
    {
        return Err(AppError::BadRequest(
            "upstream response id must contain at most 256 non-control characters".into(),
        ));
    }
    let parent = sqlx::query(
        "SELECT o.id, o.key_id, c.tenant_id, c.principal_id FROM conversation_observations o JOIN conversation_clusters c ON c.id = o.cluster_id WHERE o.request_id = $1",
    )
    .bind(request_id.to_string())
    .fetch_optional(&mut **transaction)
    .await?
    .ok_or(AppError::internal("conversations_attach_conversation_upstream_response_in_transacti"))?;
    let parent_observation_id: String = parent.try_get("id")?;
    let key_id: String = parent.try_get("key_id")?;
    let tenant_id: String = parent.try_get("tenant_id")?;
    let principal_id: String = parent.try_get("principal_id")?;
    lock_conversation_references_in_transaction(
        transaction,
        backend,
        &tenant_id,
        &principal_id,
        &key_id,
        &[Some(upstream_response_id)],
    )
    .await?;
    let updated = sqlx::query(
        "UPDATE conversation_observations SET upstream_response_id = $1 WHERE request_id = $2",
    )
    .bind(upstream_response_id)
    .bind(request_id.to_string())
    .execute(&mut **transaction)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(AppError::internal(
            "conversations_attach_conversation_upstream_response_in_transacti",
        ));
    }
    reconcile_unresolved_explicit_parents_in_transaction(
        transaction,
        backend,
        &parent_observation_id,
        upstream_response_id,
    )
    .await?;
    Ok(())
}

fn conversation_request_views(rows: Vec<AnyRow>) -> Result<Vec<ConversationRequestView>, AppError> {
    rows.into_iter()
        .map(|row| {
            let unlinked = row.try_get::<i64, _>("unlinked")? != 0;
            let source: String = row.try_get("source_kind")?;
            let billable = source == "live";
            let raw_input_tokens: i64 = row.try_get("input_tokens")?;
            let raw_cached_input_tokens: i64 = row.try_get("cached_input_tokens")?;
            let raw_cache_write_tokens: i64 = row.try_get("cache_write_tokens")?;
            let raw_output_tokens: i64 = row.try_get("output_tokens")?;
            let raw_cost_micros: i64 = row.try_get("cost_micros")?;
            let status_code: Option<i64> = row.try_get("status_code")?;
            let error_code: Option<String> = row.try_get("error_code")?;
            let usage_basis = super::queries::request_usage_basis_from_row(&row)?;
            let cost_micros = super::super::effective_cost::effective_displayed_cost_micros(
                raw_cost_micros,
                status_code,
                error_code.as_deref(),
                usage_basis.as_ref().map(|value| value.as_str()),
            );
            let completed_at = row.try_get("completed_at")?;
            let protocol: String = row.try_get("protocol")?;
            let (mut usage, billing, currency) = super::request_detail_accounting_projection(
                billable,
                completed_at,
                [
                    raw_input_tokens,
                    raw_cached_input_tokens,
                    raw_cache_write_tokens,
                    raw_output_tokens,
                ],
                cost_micros,
                row.try_get("currency")?,
            );
            if protocol == "audio-transcription" {
                usage.tokens = None;
                usage.generation = Some(RequestGenerationUsageView {
                    billed_units: completed_at
                        .is_some()
                        .then(|| row.try_get::<i64, _>("billed_units"))
                        .transpose()?,
                    billing_unit: completed_at
                        .is_some()
                        .then(|| row.try_get::<String, _>("billing_unit"))
                        .transpose()?
                        .filter(|value| !value.is_empty()),
                });
            }
            Ok(ConversationRequestView {
                request: RequestView {
                    operational_terminal: None,
                    egress: None,
                    usage_basis,
                    compaction: row
                        .try_get::<Option<i64>, _>("compaction")?
                        .filter(|value| *value == 1)
                        .map(|_| true),
                    first_output_ms: row.try_get("first_output_ms")?,
                    generation_duration_ms: row.try_get("generation_duration_ms")?,
                    request_id: parse_uuid(row.try_get("id")?)?,
                    created_at: row.try_get("created_at")?,
                    completed_at,
                    source_completed_at: row.try_get("source_completed_at")?,
                    lifecycle_state: match status_code {
                        None => crate::model::RequestLifecycleState::Pending,
                        Some(499) => crate::model::RequestLifecycleState::Cancelled,
                        Some(code)
                            if (200..400).contains(&code)
                                && error_code.as_deref()
                                    == Some("upstream_incomplete_response") =>
                        {
                            crate::model::RequestLifecycleState::Failed
                        }
                        Some(code) if (200..400).contains(&code) => {
                            crate::model::RequestLifecycleState::Succeeded
                        }
                        Some(_) => crate::model::RequestLifecycleState::Failed,
                    },
                    protocol,
                    model: row.try_get("model")?,
                    upstream_model: row.try_get("upstream_model")?,
                    upstream_account_id: row
                        .try_get::<Option<String>, _>("upstream_account_id")?
                        .map(parse_uuid)
                        .transpose()?,
                    route_id: row
                        .try_get::<Option<String>, _>("route_id")?
                        .map(parse_uuid)
                        .transpose()?,
                    status_code,
                    duration_ms: row.try_get("duration_ms")?,
                    input_tokens: raw_input_tokens,
                    cached_input_tokens: raw_cached_input_tokens,
                    cache_write_tokens: raw_cache_write_tokens,
                    output_tokens: raw_output_tokens,
                    cost: micros_to_decimal_string(cost_micros),
                    currency: currency.clone(),
                    usage,
                    billing,
                    error_code,
                    terminal_cause_code: row.try_get("terminal_cause_code")?,
                    in_flight_stage: crate::model::recorded_in_flight_stage(
                        row.try_get("in_flight_stage")?,
                    ),
                    supplier_error: None,
                    archive_state: crate::model::RequestArchiveState::from_storage(
                        row.try_get::<String, _>("archive_state")?.as_str(),
                    )
                    .ok_or(AppError::internal(
                        "conversations_conversation_request_views_1",
                    ))?,
                    credential_identity: None,
                    session_context: Some(RequestSessionContext {
                        session_id: row.try_get("session_id")?,
                        association: if unlinked {
                            RequestSessionAssociation::Unlinked
                        } else {
                            RequestSessionAssociation::Confirmed
                        },
                        session_name: row.try_get("session_name")?,
                        task_kind: row.try_get("task_kind")?,
                        agent_id: row.try_get("agent_id")?,
                        semantics_source: row.try_get("metadata_source")?,
                    }),
                },
                source,
                provenance: row.try_get("provenance_kind")?,
                unlinked,
                currency,
                archive_source: row.try_get("archive_source")?,
                external_request_id: row.try_get("external_request_id")?,
                execution: execution_metadata_from_row(&row)?,
                structure: execution_structure_from_row(&row)?,
            })
        })
        .collect()
}

fn execution_structure_from_row(
    row: &AnyRow,
) -> Result<Option<crate::conversation::ExecutionStructure>, AppError> {
    let structure = crate::conversation::ExecutionStructure {
        session_id: row.try_get("explicit_session_id")?,
        turn_id: row.try_get("turn_id")?,
        parent_turn_id: row.try_get("parent_turn_id")?,
        response_id: row.try_get("upstream_response_id")?,
        branch_id: row.try_get("branch_id")?,
        compaction: row
            .try_get::<Option<i64>, _>("compaction")?
            .unwrap_or_default()
            != 0,
        client_name: row.try_get("client_name")?,
        source: "client_protocol".to_owned(),
    };
    Ok(structure.has_values().then_some(structure))
}

fn execution_metadata_from_row(
    row: &AnyRow,
) -> Result<Option<crate::conversation::ExecutionMetadata>, AppError> {
    let source: Option<String> = row.try_get("metadata_source")?;
    let Some(source) = source else {
        return Ok(None);
    };
    let labels_json: String = row.try_get("labels_json")?;
    Ok(Some(crate::conversation::ExecutionMetadata {
        session_name: row.try_get("session_name")?,
        trace_id: row.try_get("trace_id")?,
        span_id: row.try_get("span_id")?,
        parent_span_id: row.try_get("parent_span_id")?,
        agent_id: row.try_get("agent_id")?,
        parent_agent_id: row.try_get("parent_agent_id")?,
        task_kind: row.try_get("task_kind")?,
        labels: serde_json::from_str(&labels_json).unwrap_or_default(),
        source,
    }))
}

fn infer_hash_relation(previous: &[String], current: &[String]) -> (RelationKind, i64) {
    let shared = previous
        .iter()
        .zip(current)
        .take_while(|(left, right)| left == right)
        .count();
    if shared == previous.len() && shared == current.len() {
        (RelationKind::Retry, 980)
    } else if shared == previous.len() && current.len() > previous.len() {
        (RelationKind::Continues, 950)
    // A single shared leading atom is commonly just the client's standard
    // system prompt. It is not sufficient ancestry evidence on its own.
    } else if shared >= 2 && shared + 1 >= previous.len().min(current.len()) {
        (RelationKind::Edit, 820)
    } else if shared >= 2 {
        (RelationKind::Branch, 720)
    } else {
        (RelationKind::Candidate, 350)
    }
}

/// Compaction replaces the beginning of a prompt, so a Merkle-prefix match is
/// impossible even when the client retained a real turn from the old context.
/// One exact atom outside the first/first position is useful evidence when it
/// is combined with the explicit compaction marker, the stable key, the same
/// client and the bounded recency window. Ignoring the first/first match avoids
/// merging unrelated requests merely because they share a standard system
/// prompt. With no such evidence the caller records only a candidate edge.
fn meaningful_atom_overlap(previous: &[String], current: &[String]) -> bool {
    if previous.is_empty() || current.is_empty() {
        return false;
    }
    let previous_positions: std::collections::HashMap<&str, usize> = previous
        .iter()
        .enumerate()
        .map(|(index, hash)| (hash.as_str(), index))
        .collect();
    current.iter().enumerate().any(|(current_index, hash)| {
        previous_positions
            .get(hash.as_str())
            .is_some_and(|previous_index| *previous_index > 0 || current_index > 0)
    })
}

fn conversation_batch_insert_statement(
    prefix: &'static str,
    row_count: usize,
    bind_count_per_row: usize,
    suffix: &'static str,
) -> String {
    debug_assert!(row_count > 0);
    debug_assert!(bind_count_per_row > 0);
    debug_assert!(row_count * bind_count_per_row <= CONVERSATION_INSERT_BATCH_BIND_LIMIT);

    let mut statement =
        String::with_capacity(prefix.len() + suffix.len() + row_count * bind_count_per_row * 6);
    statement.push_str(prefix);
    let mut bind_index = 1;
    for row_index in 0..row_count {
        if row_index > 0 {
            statement.push(',');
        }
        statement.push('(');
        for column_index in 0..bind_count_per_row {
            if column_index > 0 {
                statement.push(',');
            }
            // Only monotonically generated placeholders enter the statement;
            // every persisted value is supplied separately through bind().
            write!(statement, "${bind_index}").expect("writing to String cannot fail");
            bind_index += 1;
        }
        statement.push(')');
    }
    statement.push_str(suffix);
    statement
}

struct PendingAtom {
    role: String,
    kind: String,
    content_json: String,
    content_hash: String,
    instance_hash: String,
}

struct PendingNode {
    node_hash: String,
    parent_hash: Option<String>,
    atom_hash: String,
    depth: i64,
}

fn conversation_prefix_from_request_json(
    request_json: &[u8],
) -> Result<(Option<String>, Vec<String>), serde_json::Error> {
    let mut parent = None;
    let mut leaf = None;
    let mut fingerprint = Vec::new();
    crate::conversation::try_for_each_request_atom(request_json, |atom| {
        if fingerprint.len() < MAX_CONVERSATION_FINGERPRINT_ATOMS {
            fingerprint.push(atom.content_hash.clone());
        }
        let node_hash = crate::conversation::digest_parts(parent.as_deref(), &atom.content_hash);
        parent = Some(node_hash.clone());
        leaf = Some(node_hash);
        Ok::<(), serde_json::Error>(())
    })
    .map_err(|error| match error {
        crate::conversation::RequestAtomError::Json(error)
        | crate::conversation::RequestAtomError::Visitor(error) => error,
    })?;
    Ok((leaf, fingerprint))
}

impl Database {
    /// Rebuild one session from its stored request without holding every message.
    ///
    /// The worker is limited to 384Mi. Parsing the whole session into one JSON
    /// value, then cloning every message into atoms, exceeds that while the raw
    /// body is still live. Atoms and prefix nodes written here match
    /// `extract_atoms` and `build_prefix`.
    async fn materialize_streamed_conversation_content(
        &self,
        request_id: Uuid,
        lease_owner: Uuid,
        tenant_id: &str,
        request_json: String,
        observed_at: i64,
        snapshot: Option<&ConversationSemanticSnapshot>,
    ) -> Result<(), AppError> {
        let (leaf, fingerprint) = conversation_prefix_from_request_json(request_json.as_bytes())
            .map_err(|_| {
                AppError::internal("conversations_materialize_streamed_conversation_content_1")
            })?;
        if let Some(snapshot) = snapshot
            && (snapshot.leaf != leaf || snapshot.atom_hashes != fingerprint)
        {
            return Err(AppError::Conflict(
                "conversation semantic snapshot integrity mismatch".into(),
            ));
        }
        let mut transaction = self.begin_write_transaction().await?;
        let result = insert_streamed_conversation_content(
            &mut transaction,
            tenant_id,
            request_json,
            observed_at,
        )
        .await;
        if let Err(error) = result {
            let retryable_sqlstate = error.retryable_sqlstate();
            let rollback_confirmed = transaction.rollback().await.is_ok();
            tracing::warn!(
                %request_id,
                rollback_confirmed,
                sqlstate = retryable_sqlstate.unwrap_or("unclassified"),
                "conversation content transaction failed"
            );
            if rollback_confirmed && retryable_sqlstate.is_some() {
                self.try_schedule_rolled_back_conversation_retry(request_id, lease_owner)
                    .await;
            }
            return Err(error.into());
        }
        transaction.commit().await?;
        Ok(())
    }
}

async fn insert_streamed_conversation_content(
    transaction: &mut Transaction<'_, Any>,
    tenant_id: &str,
    request_json: String,
    observed_at: i64,
) -> Result<(), ConversationContentError> {
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let walk = tokio::task::spawn_blocking(move || {
        crate::conversation::try_for_each_request_atom(request_json.as_bytes(), |atom| {
            tx.blocking_send(atom).map_err(|_| ())
        })
    });
    let mut parent = None;
    let mut depth = 0_i64;
    let mut atoms = Vec::new();
    let mut nodes = Vec::new();
    let mut batch_bytes = 0_usize;
    let max_rows = CONVERSATION_INSERT_BATCH_BIND_LIMIT / SEMANTIC_ATOM_INSERT_BIND_COUNT;
    let mut failed: Option<ConversationContentError> = None;
    while let Some(atom) = rx.recv().await {
        let crate::conversation::SemanticAtom {
            role,
            kind,
            content,
            content_hash,
            instance_hash,
        } = atom;
        let Ok(content_json) = serde_json::to_string(&content) else {
            failed = Some(
                AppError::internal("conversations_insert_streamed_conversation_content_1").into(),
            );
            break;
        };
        drop(content);
        depth += 1;
        let node_hash = crate::conversation::digest_parts(parent.as_deref(), &content_hash);
        nodes.push(PendingNode {
            node_hash: node_hash.clone(),
            parent_hash: parent.clone(),
            atom_hash: content_hash.clone(),
            depth,
        });
        parent = Some(node_hash);
        batch_bytes = batch_bytes.saturating_add(content_json.len());
        atoms.push(PendingAtom {
            role,
            kind,
            content_json,
            content_hash,
            instance_hash,
        });
        if atoms.len() >= max_rows || batch_bytes >= CONVERSATION_INSERT_BATCH_BYTES {
            if let Err(error) =
                flush_conversation_batches(transaction, tenant_id, observed_at, &atoms, &nodes)
                    .await
            {
                failed = Some(error);
                break;
            }
            atoms.clear();
            nodes.clear();
            batch_bytes = 0;
        }
    }
    drop(rx);
    let walk_result = match walk.await {
        Ok(result) => result,
        Err(_) => {
            return Err(
                AppError::internal("conversations_insert_streamed_conversation_content_2").into(),
            );
        }
    };
    if let Some(error) = failed {
        return Err(error);
    }
    if !atoms.is_empty() {
        flush_conversation_batches(transaction, tenant_id, observed_at, &atoms, &nodes).await?;
    }
    match walk_result {
        Ok(()) => Ok(()),
        Err(crate::conversation::RequestAtomError::Json(_)) => {
            Err(AppError::internal("conversations_insert_streamed_conversation_content_3").into())
        }
        // The sender only fails when this task drops the receiver after its own error.
        Err(crate::conversation::RequestAtomError::Visitor(())) => {
            Err(AppError::internal("conversations_insert_streamed_conversation_content_4").into())
        }
    }
}

async fn flush_conversation_batches(
    transaction: &mut Transaction<'_, Any>,
    tenant_id: &str,
    observed_at: i64,
    atoms: &[PendingAtom],
    nodes: &[PendingNode],
) -> Result<(), ConversationContentError> {
    if !atoms.is_empty() {
        let statement = conversation_batch_insert_statement(
            "INSERT INTO semantic_atoms (tenant_id, content_hash, instance_hash, role, kind, content_json, created_at) VALUES ",
            atoms.len(),
            SEMANTIC_ATOM_INSERT_BIND_COUNT,
            " ON CONFLICT(tenant_id, content_hash) DO NOTHING",
        );
        let mut query = sqlx::query(sqlx::AssertSqlSafe(statement));
        for atom in atoms {
            query = query
                .bind(tenant_id)
                .bind(&atom.content_hash)
                .bind(&atom.instance_hash)
                .bind(&atom.role)
                .bind(&atom.kind)
                .bind(&atom.content_json)
                .bind(observed_at);
        }
        query.execute(&mut **transaction).await?;
    }
    if !nodes.is_empty() {
        let statement = conversation_batch_insert_statement(
            "INSERT INTO context_nodes (tenant_id, node_hash, parent_hash, atom_hash, depth, created_at) VALUES ",
            nodes.len(),
            CONTEXT_NODE_INSERT_BIND_COUNT,
            " ON CONFLICT(tenant_id, node_hash) DO NOTHING",
        );
        let mut query = sqlx::query(sqlx::AssertSqlSafe(statement));
        for node in nodes {
            query = query
                .bind(tenant_id)
                .bind(&node.node_hash)
                .bind(&node.parent_hash)
                .bind(&node.atom_hash)
                .bind(node.depth)
                .bind(observed_at);
        }
        query.execute(&mut **transaction).await?;
    }
    Ok(())
}

pub(crate) async fn materialize_conversation_content_in_transaction(
    transaction: &mut Transaction<'_, Any>,
    tenant_id: &str,
    atoms: &[SemanticAtom],
    nodes: &[PrefixNode],
    observed_at: i64,
) -> Result<(), AppError> {
    materialize_conversation_content_with_statement_error(
        transaction,
        tenant_id,
        atoms,
        nodes,
        observed_at,
    )
    .await
    .map_err(Into::into)
}

async fn materialize_conversation_content_with_statement_error(
    transaction: &mut Transaction<'_, Any>,
    tenant_id: &str,
    atoms: &[SemanticAtom],
    nodes: &[PrefixNode],
    observed_at: i64,
) -> Result<(), ConversationContentError> {
    for atom_batch in
        atoms.chunks(CONVERSATION_INSERT_BATCH_BIND_LIMIT / SEMANTIC_ATOM_INSERT_BIND_COUNT)
    {
        let statement = conversation_batch_insert_statement(
            "INSERT INTO semantic_atoms (tenant_id, content_hash, instance_hash, role, kind, content_json, created_at) VALUES ",
            atom_batch.len(),
            SEMANTIC_ATOM_INSERT_BIND_COUNT,
            " ON CONFLICT(tenant_id, content_hash) DO NOTHING",
        );
        let mut query = sqlx::query(sqlx::AssertSqlSafe(statement));
        for atom in atom_batch {
            query = query
                .bind(tenant_id)
                .bind(&atom.content_hash)
                .bind(&atom.instance_hash)
                .bind(&atom.role)
                .bind(&atom.kind)
                .bind(serde_json::to_string(&atom.content).map_err(|_| {
                    AppError::internal(
                        "conversations_materialize_conversation_content_with_statement_er",
                    )
                })?)
                .bind(observed_at);
        }
        query.execute(&mut **transaction).await?;
    }
    for node_batch in
        nodes.chunks(CONVERSATION_INSERT_BATCH_BIND_LIMIT / CONTEXT_NODE_INSERT_BIND_COUNT)
    {
        let statement = conversation_batch_insert_statement(
            "INSERT INTO context_nodes (tenant_id, node_hash, parent_hash, atom_hash, depth, created_at) VALUES ",
            node_batch.len(),
            CONTEXT_NODE_INSERT_BIND_COUNT,
            " ON CONFLICT(tenant_id, node_hash) DO NOTHING",
        );
        let mut query = sqlx::query(sqlx::AssertSqlSafe(statement));
        for node in node_batch {
            query = query
                .bind(tenant_id)
                .bind(&node.node_hash)
                .bind(&node.parent_hash)
                .bind(&node.atom_hash)
                .bind(node.depth as i64)
                .bind(observed_at);
        }
        query.execute(&mut **transaction).await?;
    }
    Ok(())
}

fn bounded_atom_hashes(atoms: &[crate::conversation::SemanticAtom]) -> Vec<String> {
    atoms
        .iter()
        .take(MAX_CONVERSATION_FINGERPRINT_ATOMS)
        .map(|atom| atom.content_hash.clone())
        .collect()
}

fn relation_name(relation: RelationKind) -> &'static str {
    match relation {
        RelationKind::Continues => "continues",
        RelationKind::Retry => "retry",
        RelationKind::Edit => "edit",
        RelationKind::Branch => "branch",
        RelationKind::Compacts => "compacts",
        RelationKind::Subagent => "subagent",
        RelationKind::Candidate => "candidate",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_fingerprint_has_a_fixed_serialized_memory_bound() {
        let request = serde_json::json!({
            "messages": (0..(MAX_CONVERSATION_FINGERPRINT_ATOMS + 100))
                .map(|index| serde_json::json!({"role": "user", "content": index.to_string()}))
                .collect::<Vec<_>>()
        });
        let atoms = extract_atoms(&request);
        let fingerprint = bounded_atom_hashes(&atoms);
        let encoded = serde_json::to_vec(&fingerprint).unwrap();

        assert_eq!(fingerprint.len(), MAX_CONVERSATION_FINGERPRINT_ATOMS);
        assert!(encoded.len() <= MAX_CONVERSATION_FINGERPRINT_JSON_BYTES);
    }

    #[test]
    fn compaction_overlap_ignores_a_shared_leading_system_prompt() {
        let shared_system = "system".to_owned();
        let retained_turn = "retained".to_owned();
        assert!(!meaningful_atom_overlap(
            &[shared_system.clone(), "old".into()],
            &[shared_system.clone(), "new".into()]
        ));
        assert!(meaningful_atom_overlap(
            &[shared_system, retained_turn.clone()],
            &["summary".into(), retained_turn]
        ));
    }

    #[test]
    fn a_single_shared_leading_atom_never_becomes_a_merge_relation() {
        assert_eq!(
            infer_hash_relation(
                &["shared-system".into(), "old-user".into()],
                &["shared-system".into(), "unrelated-user".into()]
            ),
            (RelationKind::Candidate, 350)
        );
    }

    #[test]
    fn conversation_batch_insert_uses_ordered_bound_placeholders() {
        assert_eq!(
            conversation_batch_insert_statement("VALUES ", 2, 3, " ON CONFLICT DO NOTHING"),
            "VALUES ($1,$2,$3),($4,$5,$6) ON CONFLICT DO NOTHING"
        );
    }

    #[tokio::test]
    async fn streamed_session_rebuild_matches_extracted_prefix() {
        let directory = tempfile::tempdir().unwrap();
        let db = Database::connect(&format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("session-rebuild.db").display()
        ))
        .await
        .unwrap();
        db.migrate().await.unwrap();
        let mut messages = vec![
            serde_json::json!({"role": "system", "content": "rules", "id": "s"}),
            serde_json::json!({"role": "user", "content": "keep [}{] \"quotes\"", "created_at": 3}),
            serde_json::json!({"role": "assistant", "content": [{"type": "text", "text": "answer", "call_id": "c"}], "tool_call_id": "t"}),
        ];
        messages.extend((0..200).map(|index| {
            serde_json::json!({"role": "user", "content": format!("turn-{index}"), "id": format!("id-{index}")})
        }));
        messages.push(serde_json::json!({"role": "user", "content": "tail"}));
        let body = serde_json::json!({
            "model": "session-rebuild",
            "tools": [{"name": "unused", "description": "x".repeat(2048)}],
            "messages": messages
        });
        let text = serde_json::to_string(&body).unwrap();
        let tenant = Uuid::now_v7().to_string();
        let (leaf, fingerprint) = conversation_prefix_from_request_json(text.as_bytes()).unwrap();
        let mismatch = db
            .materialize_streamed_conversation_content(
                Uuid::now_v7(),
                Uuid::now_v7(),
                &tenant,
                text.clone(),
                7,
                Some(&ConversationSemanticSnapshot {
                    leaf: Some("not-the-leaf".into()),
                    atom_hashes: fingerprint.clone(),
                    payload_digest: None,
                }),
            )
            .await;
        assert!(mismatch.is_err());
        let untouched: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM semantic_atoms WHERE tenant_id = $1")
                .bind(&tenant)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(untouched, 0);
        db.materialize_streamed_conversation_content(
            Uuid::now_v7(),
            Uuid::now_v7(),
            &tenant,
            text.clone(),
            7,
            Some(&ConversationSemanticSnapshot {
                leaf: leaf.clone(),
                atom_hashes: fingerprint.clone(),
                payload_digest: None,
            }),
        )
        .await
        .unwrap();
        let atoms = extract_atoms(&body);
        let nodes = build_prefix(&atoms);
        let rows = sqlx::query(
            "SELECT n.depth, n.node_hash, n.parent_hash, n.atom_hash, a.role, a.kind, a.instance_hash, a.content_json FROM context_nodes n JOIN semantic_atoms a ON a.tenant_id = n.tenant_id AND a.content_hash = n.atom_hash WHERE n.tenant_id = $1 ORDER BY n.depth",
        )
        .bind(&tenant)
        .fetch_all(&db.pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), atoms.len());
        assert!(
            atoms.len() > CONVERSATION_INSERT_BATCH_BIND_LIMIT / SEMANTIC_ATOM_INSERT_BIND_COUNT
        );
        for (row, (atom, node)) in rows.iter().zip(atoms.iter().zip(&nodes)) {
            let depth: i64 = row.try_get("depth").unwrap();
            let node_hash: String = row.try_get("node_hash").unwrap();
            let parent_hash: Option<String> = row.try_get("parent_hash").unwrap();
            let atom_hash: String = row.try_get("atom_hash").unwrap();
            let role: String = row.try_get("role").unwrap();
            let kind: String = row.try_get("kind").unwrap();
            let instance_hash: String = row.try_get("instance_hash").unwrap();
            let content_json: String = row.try_get("content_json").unwrap();
            assert_eq!(depth, node.depth as i64);
            assert_eq!(node_hash, node.node_hash);
            assert_eq!(parent_hash, node.parent_hash);
            assert_eq!(atom_hash, atom.content_hash);
            assert_eq!(role, atom.role);
            assert_eq!(kind, atom.kind);
            assert_eq!(instance_hash, atom.instance_hash);
            assert_eq!(content_json, serde_json::to_string(&atom.content).unwrap());
        }
        assert_eq!(
            leaf.as_deref(),
            nodes.last().map(|node| node.node_hash.as_str())
        );
        assert_eq!(
            fingerprint,
            atoms
                .iter()
                .take(MAX_CONVERSATION_FINGERPRINT_ATOMS)
                .map(|atom| atom.content_hash.clone())
                .collect::<Vec<_>>()
        );
        db.pool.close().await;
    }

    #[test]
    fn conversation_insert_batches_stay_below_sqlite_bind_limit() {
        const { assert!(CONVERSATION_INSERT_BATCH_BIND_LIMIT < 999) };
        for bind_count_per_row in [
            SEMANTIC_ATOM_INSERT_BIND_COUNT,
            CONTEXT_NODE_INSERT_BIND_COUNT,
        ] {
            let rows = CONVERSATION_INSERT_BATCH_BIND_LIMIT / bind_count_per_row;
            assert!(rows * bind_count_per_row <= CONVERSATION_INSERT_BATCH_BIND_LIMIT);
            assert!((rows + 1) * bind_count_per_row > CONVERSATION_INSERT_BATCH_BIND_LIMIT);
        }
    }
}
