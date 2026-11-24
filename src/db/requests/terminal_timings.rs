use std::time::Instant;

pub(crate) const STAGE_NAMES: [&str; 10] = [
    "account_receipt",
    "tx_begin",
    "stats_gate",
    "outbox_claim",
    "fact_projection",
    "ack_update",
    "native_capture",
    "event_prepare",
    "event_publish",
    "commit",
];
#[derive(Clone, Copy)]
pub(crate) enum ProjectionStage {
    AccountReceipt,
    TxBegin,
    StatsGate,
    OutboxClaim,
    FactProjection,
    AckUpdate,
    NativeCapture,
    EventPrepare,
    EventPublish,
    Commit,
}

pub(crate) struct TerminalProjectionTimings {
    pub micros: [u64; 10],
    pub failure_stage: Option<usize>,
    current: ProjectionStage,
    started: Instant,
}
impl TerminalProjectionTimings {
    pub fn new() -> Self {
        Self {
            micros: [0; 10],
            failure_stage: None,
            current: ProjectionStage::AccountReceipt,
            started: Instant::now(),
        }
    }
    fn record(&mut self) {
        self.micros[self.current as usize] = self.micros[self.current as usize]
            .saturating_add(self.started.elapsed().as_micros().min(u64::MAX as u128) as u64);
    }
    pub fn enter(&mut self, stage: ProjectionStage) {
        self.record();
        self.current = stage;
        self.started = Instant::now();
    }
    pub fn finish(&mut self, failed: bool) {
        self.record();
        if failed {
            self.failure_stage = Some(self.current as usize);
        }
    }
}

#[derive(Default)]
pub(crate) struct TerminalProjectionBatchTimings {
    pub attempted: u64,
    pub projected: u64,
    pub deferred: u64,
    pub failed: u64,
    pub stage_sum_us: [u64; 10],
    pub stage_max_us: [u64; 10],
    pub stage_failures: [u64; 10],
}
impl TerminalProjectionBatchTimings {
    pub fn observe(&mut self, outcome: Option<bool>, timings: &TerminalProjectionTimings) {
        self.attempted += 1;
        match outcome {
            Some(true) => self.projected += 1,
            Some(false) => self.deferred += 1,
            None => self.failed += 1,
        }
        for i in 0..10 {
            self.stage_sum_us[i] = self.stage_sum_us[i].saturating_add(timings.micros[i]);
            self.stage_max_us[i] = self.stage_max_us[i].max(timings.micros[i]);
        }
        if let Some(stage) = timings.failure_stage {
            self.stage_failures[stage] += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sums_maxima_and_failures_preserve_outcomes() {
        let mut batch = TerminalProjectionBatchTimings::default();
        let mut t = TerminalProjectionTimings::new();
        t.micros = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
        batch.observe(Some(true), &t);
        batch.observe(Some(false), &t);
        t.failure_stage = Some(ProjectionStage::StatsGate as usize);
        batch.observe(None, &t);
        assert_eq!(
            (
                batch.attempted,
                batch.projected,
                batch.deferred,
                batch.failed
            ),
            (3, 1, 1, 1)
        );
        assert_eq!(batch.stage_sum_us, [3, 6, 9, 12, 15, 18, 21, 24, 27, 30]);
        assert_eq!(batch.stage_max_us, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        assert_eq!(batch.stage_failures, [0, 0, 1, 0, 0, 0, 0, 0, 0, 0]);
    }
    #[tokio::test]
    async fn failed_account_receipt_keeps_failure_stage() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::Database::connect_with_max(
            &format!(
                "sqlite://{}?mode=rwc",
                dir.path().join("timings.db").display()
            ),
            2,
        )
        .await
        .unwrap();
        db.migrate().await.unwrap();
        sqlx::query("DROP TABLE terminal_projection_outbox")
            .execute(&db.pool)
            .await
            .unwrap();
        let (result, t) = db
            .project_claimed_terminal_projection_with_timings(
                uuid::Uuid::new_v4(),
                uuid::Uuid::new_v4(),
            )
            .await;
        assert!(result.is_err());
        assert_eq!(t.failure_stage, Some(0));
        assert!(t.micros[0] > 0);
        assert_eq!(t.micros[1..], [0; 9]);
    }
}
