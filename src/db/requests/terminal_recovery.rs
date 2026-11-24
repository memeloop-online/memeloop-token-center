use super::super::*;
use super::terminal_projection::{TERMINAL_CLAIM_TTL_MILLIS, terminal_claim_eligibility};

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct TerminalRecoveryIdentity {
    pub request_id: Uuid,
    pub tenant_id: Uuid,
    pub key_id: Uuid,
    pub account_id: Uuid,
    pub reservation_id: Uuid,
    pub created_at: i64,
}

#[derive(Debug, serde::Serialize)]
pub struct TerminalRecoveryReport {
    pub state: &'static str,
    pub account_acknowledged: bool,
    pub statistics_acknowledged: bool,
    pub fact_exists: bool,
    pub applied: bool,
    pub replay_applied: bool,
}

impl Database {
    /// One atomic, read-only inspection. A missing identity is never widened.
    pub async fn inspect_terminal_recovery(
        &self,
        identity: TerminalRecoveryIdentity,
    ) -> Result<TerminalRecoveryReport, AppError> {
        let row = sqlx::query("SELECT t.account_projected_at,t.projected_at,t.lease_owner,t.lease_expires_at,t.cost_micros,
            CAST(CASE WHEN t.usage_basis='provider_reported' AND t.status_code BETWEEN 200 AND 399
              AND t.error_code='' AND r.status_code=t.status_code AND r.input_tokens=t.input_tokens
              AND r.output_tokens=t.output_tokens AND r.cost_micros=t.cost_micros AND r.usage_basis=t.usage_basis
              AND r.cached_input_tokens=t.cached_input_tokens AND r.cache_write_tokens=t.cache_write_tokens
              AND COALESCE(r.cache_known_read_tokens,-1)=COALESCE(t.cache_known_read_tokens,-1)
              AND COALESCE(r.cache_known_input_tokens,-1)=COALESCE(t.cache_known_input_tokens,-1)
              AND r.model=t.model AND r.protocol=t.protocol AND r.currency=t.currency
              AND o.status_code=t.status_code AND COALESCE(o.error_code,'')='' AND u.status='settled' AND u.actual_micros=t.cost_micros
              THEN 1 ELSE 0 END AS BIGINT) AS trusted,
            (SELECT COUNT(*) FROM ledger_entries l WHERE l.account_id=t.account_id AND l.key_id=t.key_id AND l.source=t.reservation_id AND l.kind='usage') AS ledger_count,
            (SELECT CAST(COALESCE(SUM(l.amount_micros),0) AS BIGINT) FROM ledger_entries l WHERE l.account_id=t.account_id AND l.key_id=t.key_id AND l.source=t.reservation_id AND l.kind='usage') AS ledger_amount,
            (SELECT COUNT(*) FROM request_stats_facts f WHERE f.request_id=t.request_id AND f.tenant_id=t.tenant_id AND f.key_id=t.key_id AND f.created_at=t.created_at) AS facts,
            (SELECT COUNT(*) FROM request_stats_facts f WHERE f.request_id=t.request_id AND f.tenant_id=t.tenant_id AND f.key_id=t.key_id AND f.created_at=t.created_at AND f.model=t.model AND f.protocol=t.protocol AND f.currency=t.currency AND f.status_class='success' AND f.input_tokens=t.input_tokens AND f.output_tokens=t.output_tokens AND f.cost_micros=t.cost_micros AND f.cached_input_tokens=t.cached_input_tokens AND f.cache_write_tokens=t.cache_write_tokens) AS matching_facts,
            (SELECT COUNT(*) FROM metered_usage_projection_outbox m WHERE m.reservation_id=t.reservation_id AND m.account_id=t.account_id AND m.key_id=t.key_id AND m.actual_micros=t.cost_micros AND m.projected_at IS NULL AND m.lease_owner='terminal-v118') AS receipts,
            (SELECT COUNT(*) FROM pending_proxy_settlements p WHERE p.request_id=t.request_id AND p.unknown_reason IS NOT NULL) AS unknowns,
            (SELECT COUNT(*) FROM observability_prune_boundaries b WHERE b.scope='global' AND b.before_day > t.created_at / 86400000) AS pruned
          FROM terminal_projection_outbox t
          JOIN request_records r ON r.id=t.request_id AND r.created_at=t.created_at AND r.tenant_id=t.tenant_id AND r.key_id=t.key_id AND r.reservation_id=t.reservation_id
          JOIN usage_reservations u ON u.id=t.reservation_id AND u.account_id=t.account_id AND u.key_id=t.key_id
          JOIN proxy_operational_terminals o ON o.request_id=t.request_id AND o.request_created_at=t.created_at AND o.tenant_id=t.tenant_id AND o.key_id=t.key_id AND o.account_id=t.account_id AND o.reservation_id=t.reservation_id
          WHERE t.request_id=$1 AND t.tenant_id=$2 AND t.key_id=$3 AND t.account_id=$4 AND t.reservation_id=$5 AND t.created_at=$6")
            .bind(identity.request_id.to_string()).bind(identity.tenant_id.to_string())
            .bind(identity.key_id.to_string()).bind(identity.account_id.to_string())
            .bind(identity.reservation_id.to_string()).bind(identity.created_at)
            .fetch_optional(&self.pool).await?;
        let Some(row) = row else {
            return Err(AppError::Conflict(
                "terminal recovery identity or source is missing".into(),
            ));
        };
        let cost: i64 = row.try_get("cost_micros")?;
        if row.try_get::<i64, _>("trusted")? != 1
            || row.try_get::<i64, _>("ledger_count")? != 1
            || cost.checked_neg() != Some(row.try_get::<i64, _>("ledger_amount")?)
            || row.try_get::<i64, _>("unknowns")? != 0
        {
            return Err(AppError::Conflict(
                "terminal recovery source or ledger does not match".into(),
            ));
        }
        let account_acknowledged = row
            .try_get::<Option<i64>, _>("account_projected_at")?
            .is_some();
        let statistics_acknowledged = row.try_get::<Option<i64>, _>("projected_at")?.is_some();
        let fact_count = row.try_get::<i64, _>("facts")?;
        if fact_count != row.try_get::<i64, _>("matching_facts")?
            || fact_count > 1
            || (!account_acknowledged && row.try_get::<i64, _>("receipts")? != 1)
        {
            return Err(AppError::Conflict(
                "terminal recovery receipt or fact identity conflicts".into(),
            ));
        }
        let fact_exists = fact_count == 1;
        let pruned = row.try_get::<i64, _>("pruned")? != 0;
        let busy = row.try_get::<Option<String>, _>("lease_owner")?.is_some()
            && row
                .try_get::<Option<i64>, _>("lease_expires_at")?
                .is_some_and(|until| until > unix_millis());
        let state = if statistics_acknowledged {
            "already_done"
        } else if pruned {
            "pruned"
        } else if busy {
            "busy"
        } else {
            "eligible"
        };
        if fact_exists && !statistics_acknowledged {
            return Err(AppError::Conflict(
                "terminal recovery has an unacknowledged existing fact".into(),
            ));
        }
        if statistics_acknowledged && !fact_exists && !pruned {
            return Err(AppError::Conflict(
                "terminal recovery acknowledged fact is missing".into(),
            ));
        }
        if statistics_acknowledged && !account_acknowledged {
            return Err(AppError::Conflict(
                "terminal recovery acknowledgements conflict".into(),
            ));
        }
        Ok(TerminalRecoveryReport {
            state,
            account_acknowledged,
            statistics_acknowledged,
            fact_exists,
            applied: false,
            replay_applied: false,
        })
    }

    pub async fn recover_one_terminal_projection(
        &self,
        identity: TerminalRecoveryIdentity,
        apply: bool,
    ) -> Result<TerminalRecoveryReport, AppError> {
        let before = self.inspect_terminal_recovery(identity).await?;
        if !apply || before.state != "eligible" {
            return Ok(before);
        }
        let owner = Uuid::now_v7();
        let now = unix_millis();
        let mut tx = self.begin_write_transaction().await?;
        let suffix = match self.backend {
            DatabaseBackend::PostgreSql => " FOR UPDATE SKIP LOCKED",
            DatabaseBackend::Sqlite => "",
        };
        let eligibility = terminal_claim_eligibility(7, 9);
        let statement = format!(
            "UPDATE terminal_projection_outbox SET lease_owner=$7,lease_expires_at=$8,attempts=attempts+1 WHERE request_id IN (SELECT request_id FROM terminal_projection_outbox WHERE request_id=$1 AND tenant_id=$2 AND key_id=$3 AND account_id=$4 AND reservation_id=$5 AND created_at=$6 AND {eligibility}{suffix}) RETURNING request_id"
        );
        let claimed = sqlx::query(sqlx::AssertSqlSafe(statement))
            .bind(identity.request_id.to_string())
            .bind(identity.tenant_id.to_string())
            .bind(identity.key_id.to_string())
            .bind(identity.account_id.to_string())
            .bind(identity.reservation_id.to_string())
            .bind(identity.created_at)
            .bind(owner.to_string())
            .bind(now.saturating_add(TERMINAL_CLAIM_TTL_MILLIS))
            .bind(now)
            .fetch_optional(&mut *tx)
            .await?
            .is_some();
        tx.commit().await?;
        if !claimed {
            let mut current = self.inspect_terminal_recovery(identity).await?;
            if current.state == "eligible" {
                current.state = "not_claimed";
            }
            return Ok(current);
        }
        // Recheck current authoritative source after claiming, before the
        // original writer performs any account projection.
        let current = self.inspect_terminal_recovery(identity).await?;
        // A changed/pruned source is read-only. The original writer still
        // independently validates ownership and expiry in its transaction.
        let owned = sqlx::query("SELECT request_id FROM terminal_projection_outbox WHERE request_id=$1 AND lease_owner=$2 AND lease_expires_at>$3 AND projected_at IS NULL")
            .bind(identity.request_id.to_string()).bind(owner.to_string()).bind(unix_millis())
            .fetch_optional(&self.pool).await?.is_some();
        if current.state != "busy" || !owned {
            // No manual ACK or lease rewrite: the claimed lease expires after
            // the shared five-minute TTL if the source changed after claim.
            return Ok(current);
        }
        let applied = self
            .project_claimed_terminal_projection_task(owner, identity.request_id)
            .await?;
        let replay_applied = self
            .project_claimed_terminal_projection_task(owner, identity.request_id)
            .await?;
        let mut after = self.inspect_terminal_recovery(identity).await?;
        after.applied = applied;
        after.replay_applied = replay_applied;
        if replay_applied {
            return Err(AppError::Conflict(
                "terminal recovery replay applied twice".into(),
            ));
        }
        Ok(after)
    }
}
