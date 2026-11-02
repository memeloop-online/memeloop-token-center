use std::{
    fmt::Write,
    sync::atomic::{AtomicU64, Ordering},
};

#[derive(Default)]
struct StageCounters {
    waiting: AtomicU64,
    admitted: AtomicU64,
    timed_out: AtomicU64,
    cancelled: AtomicU64,
    wait_micros: AtomicU64,
}

#[derive(Default)]
pub(super) struct Counters([StageCounters; 3]);

impl Counters {
    pub(super) fn render(&self, output: &mut String) {
        output.push_str("# HELP memeloop_token_center_proxy_memory_waiting Requests currently queued for memory capacity.\n# TYPE memeloop_token_center_proxy_memory_waiting gauge\n");
        output.push_str("# HELP memeloop_token_center_proxy_memory_waits_total Completed memory queue waits by stage and outcome.\n# TYPE memeloop_token_center_proxy_memory_waits_total counter\n");
        output.push_str("# HELP memeloop_token_center_proxy_memory_wait_seconds_total Time spent in completed memory queue waits.\n# TYPE memeloop_token_center_proxy_memory_wait_seconds_total counter\n");
        for (stage, counters) in ["retained", "response", "projection"]
            .into_iter()
            .zip(&self.0)
        {
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
