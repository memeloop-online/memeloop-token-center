use std::time::Instant;

use uuid::Uuid;

/// Observes ownership only. Dropping this guard neither commits nor cancels SQL.
pub(crate) struct BudgetHold {
    started: Instant,
    phase_started: Instant,
    phase: &'static str,
    operation: &'static str,
    request_id: Option<Uuid>,
    backend_pid: Option<i64>,
    slowest_phase: &'static str,
    slowest_ms: u64,
    outcome: &'static str,
    late_budget: bool,
}

impl BudgetHold {
    pub(super) fn new(
        operation: &'static str,
        request_id: Option<Uuid>,
        backend_pid: Option<i64>,
    ) -> Self {
        Self::at(operation, request_id, backend_pid, Instant::now())
    }

    fn at(
        operation: &'static str,
        request_id: Option<Uuid>,
        backend_pid: Option<i64>,
        now: Instant,
    ) -> Self {
        Self {
            started: now,
            phase_started: now,
            phase: "archive_clock",
            operation,
            request_id,
            backend_pid,
            slowest_phase: "archive_clock",
            slowest_ms: 0,
            outcome: "abandoned",
            late_budget: false,
        }
    }

    /// Observes a transaction which takes the singleton budget only in its
    /// final phase. Its timing must not be described as budget ownership.
    pub(super) fn late(operation: &'static str, request_id: Option<Uuid>) -> Self {
        let mut hold = Self::new(operation, request_id, None);
        hold.late_budget = true;
        hold
    }

    pub(crate) fn phase(&mut self, phase: &'static str) {
        self.phase_at(phase, Instant::now());
    }

    fn phase_at(&mut self, phase: &'static str, now: Instant) {
        let elapsed = now
            .saturating_duration_since(self.phase_started)
            .as_millis() as u64;
        if elapsed > self.slowest_ms {
            self.slowest_ms = elapsed;
            self.slowest_phase = self.phase;
        }
        self.phase = phase;
        self.phase_started = now;
    }

    #[cfg(test)]
    fn completed(&mut self) {
        self.outcome = "committed";
    }

    pub(crate) fn set_phase(hold: &mut Option<Self>, phase: &'static str) {
        if let Some(hold) = hold {
            hold.phase(phase);
        }
    }

    pub(crate) async fn commit_optional(
        transaction: sqlx::Transaction<'_, sqlx::Any>,
        hold: Option<Self>,
    ) -> Result<(), sqlx::Error> {
        match hold {
            Some(hold) => hold.commit(transaction).await,
            None => transaction.commit().await,
        }
    }

    pub(crate) async fn rollback_optional(
        transaction: sqlx::Transaction<'_, sqlx::Any>,
        mut hold: Option<Self>,
    ) -> Result<(), sqlx::Error> {
        Self::set_phase(&mut hold, "rollback");
        let result = transaction.rollback().await;
        if let Some(hold) = hold.as_mut() {
            hold.outcome = if result.is_ok() {
                "rolled_back"
            } else {
                "rollback_failed"
            };
        }
        result
    }

    pub(crate) async fn commit(
        mut self,
        transaction: sqlx::Transaction<'_, sqlx::Any>,
    ) -> Result<(), sqlx::Error> {
        self.phase("commit");
        let result = transaction.commit().await;
        self.outcome = if result.is_ok() {
            "committed"
        } else {
            "commit_failed"
        };
        result
    }

    pub(crate) fn backend_pid(&self) -> Option<i64> {
        self.backend_pid
    }

    fn report_at(&mut self, now: Instant) -> Option<(u64, &'static str, u64)> {
        let hold_ms = now.saturating_duration_since(self.started).as_millis() as u64;
        self.phase_at(self.phase, now);
        (hold_ms >= 250 || matches!(self.outcome, "commit_failed" | "rollback_failed")).then_some((
            hold_ms,
            self.slowest_phase,
            self.slowest_ms,
        ))
    }
}

impl Drop for BudgetHold {
    fn drop(&mut self) {
        if let Some((hold_ms, slowest_phase, slowest_phase_ms)) = self.report_at(Instant::now()) {
            if self.late_budget {
                tracing::warn!(phase = "archive_transaction", operation = self.operation,
                    request_id = ?self.request_id, outcome = self.outcome,
                    last_phase = self.phase, elapsed_ms = hold_ms,
                    slowest_phase, slowest_phase_ms,
                    "slow archive transaction");
            } else {
                tracing::warn!(phase = "archive_budget_hold", operation = self.operation,
                    request_id = ?self.request_id, backend_pid = ?self.backend_pid,
                    outcome = self.outcome, last_phase = self.phase, hold_ms,
                    slowest_phase, slowest_phase_ms,
                    "slow archive budget ownership; abandoned does not assert rollback acknowledgement");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn hold_clock_excludes_acquisition_and_identifies_the_slowest_owned_phase() {
        let acquired = Instant::now();
        let mut hold = BudgetHold::at("request_admission", Some(Uuid::nil()), Some(42), acquired);
        hold.phase_at(
            "key_account_reservation",
            acquired + Duration::from_millis(2),
        );
        hold.phase_at("capture", acquired + Duration::from_millis(902));
        hold.phase_at("commit", acquired + Duration::from_millis(950));
        hold.completed();
        assert_eq!(
            hold.report_at(acquired + Duration::from_millis(1000)),
            Some((1000, "key_account_reservation", 900))
        );
        assert_eq!(hold.outcome, "committed");
    }

    #[test]
    fn fast_success_is_quiet_and_early_exit_never_claims_rollback() {
        let acquired = Instant::now();
        let mut hold = BudgetHold::at("append", None, None, acquired);
        assert_eq!(hold.report_at(acquired + Duration::from_millis(249)), None);
        assert_eq!(hold.outcome, "abandoned");
        hold.phase_at("commit", acquired + Duration::from_millis(250));
        assert_eq!(
            hold.report_at(acquired + Duration::from_millis(700)),
            Some((700, "commit", 450))
        );
        assert_eq!(hold.outcome, "abandoned");
    }

    #[test]
    fn failed_commit_is_reportable_without_waiting_for_a_slow_threshold() {
        let acquired = Instant::now();
        let mut hold = BudgetHold::at("request_admission", None, Some(42), acquired);
        hold.phase_at("commit", acquired);
        hold.outcome = "commit_failed";
        assert_eq!(
            hold.report_at(acquired + Duration::from_millis(1)),
            Some((1, "commit", 1))
        );
    }
}
