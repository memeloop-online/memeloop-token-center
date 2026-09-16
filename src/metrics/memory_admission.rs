use std::{
    fmt::Write,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

#[derive(Clone, Copy)]
pub(crate) enum Stage {
    Retained,
    Response,
}

#[derive(Default)]
struct StageCounters {
    waiting: AtomicU64,
    admitted: AtomicU64,
    timed_out: AtomicU64,
    cancelled: AtomicU64,
    wait_micros: AtomicU64,
}

#[derive(Default)]
pub(super) struct Counters([Arc<StageCounters>; 2]);

impl Counters {
    pub(super) fn wait(&self, stage: Stage) -> WaitGuard {
        let counters = self.0[stage as usize].clone();
        counters.waiting.fetch_add(1, Ordering::Relaxed);
        WaitGuard {
            counters,
            started: Instant::now(),
            result: None,
        }
    }

    pub(super) fn render(&self, output: &mut String) {
        output.push_str("# HELP memeloop_token_center_proxy_memory_waiting Requests currently queued for memory capacity.\n# TYPE memeloop_token_center_proxy_memory_waiting gauge\n");
        output.push_str("# HELP memeloop_token_center_proxy_memory_waits_total Completed memory queue waits by stage and outcome.\n# TYPE memeloop_token_center_proxy_memory_waits_total counter\n");
        output.push_str("# HELP memeloop_token_center_proxy_memory_wait_seconds_total Time spent in completed memory queue waits.\n# TYPE memeloop_token_center_proxy_memory_wait_seconds_total counter\n");
        for (stage, counters) in ["retained", "response"].into_iter().zip(&self.0) {
            let _ = writeln!(
                output,
                "memeloop_token_center_proxy_memory_waiting{{stage=\"{stage}\"}} {}",
                counters.waiting.load(Ordering::Relaxed)
            );
            for (outcome, count) in [
                ("admitted", &counters.admitted),
                ("timeout", &counters.timed_out),
                ("cancelled", &counters.cancelled),
            ] {
                let _ = writeln!(
                    output,
                    "memeloop_token_center_proxy_memory_waits_total{{stage=\"{stage}\",outcome=\"{outcome}\"}} {}",
                    count.load(Ordering::Relaxed)
                );
            }
            let _ = writeln!(
                output,
                "memeloop_token_center_proxy_memory_wait_seconds_total{{stage=\"{stage}\"}} {}",
                counters.wait_micros.load(Ordering::Relaxed) as f64 / 1_000_000.0
            );
        }
    }
}

pub(crate) struct WaitGuard {
    counters: Arc<StageCounters>,
    started: Instant,
    result: Option<bool>,
}

impl WaitGuard {
    pub(crate) fn finish(mut self, admitted: bool) {
        self.result = Some(admitted);
    }
}

impl Drop for WaitGuard {
    fn drop(&mut self) {
        self.counters.waiting.fetch_sub(1, Ordering::Relaxed);
        let outcome = match self.result {
            Some(true) => &self.counters.admitted,
            Some(false) => &self.counters.timed_out,
            None => &self.counters.cancelled,
        };
        outcome.fetch_add(1, Ordering::Relaxed);
        self.counters.wait_micros.fetch_add(
            self.started.elapsed().as_micros().min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
    }
}
