//! Fixed-size host invocation counters; no guest- or request-derived labels.
use std::{
    fmt::Write,
    sync::atomic::{AtomicU64, Ordering},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    PostAuth,
    Prepare,
    Normalize,
    WireShimFinalize,
    GroupRoutingPlan,
    GroupRoutingObserve,
}
impl Phase {
    const ALL: [Self; 6] = [
        Self::PostAuth,
        Self::Prepare,
        Self::Normalize,
        Self::WireShimFinalize,
        Self::GroupRoutingPlan,
        Self::GroupRoutingObserve,
    ];
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::PostAuth => "post_auth",
            Self::Prepare => "prepare",
            Self::Normalize => "normalize",
            Self::WireShimFinalize => "wire_shim_finalize",
            Self::GroupRoutingPlan => "group_routing_plan",
            Self::GroupRoutingObserve => "group_routing_observe",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Returned,
    HookError,
    CapacityTimeout,
    CapacityClosed,
    ExecutionTimeout,
    TaskFailed,
    CallerCancelled,
}
impl Outcome {
    const ALL: [Self; 7] = [
        Self::Returned,
        Self::HookError,
        Self::CapacityTimeout,
        Self::CapacityClosed,
        Self::ExecutionTimeout,
        Self::TaskFailed,
        Self::CallerCancelled,
    ];
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Returned => "returned",
            Self::HookError => "hook_error",
            Self::CapacityTimeout => "capacity_timeout",
            Self::CapacityClosed => "capacity_closed",
            Self::ExecutionTimeout => "execution_timeout",
            Self::TaskFailed => "task_failed",
            Self::CallerCancelled => "caller_cancelled",
        }
    }
}

pub(super) struct Counters([AtomicU64; 42]);
impl Default for Counters {
    fn default() -> Self {
        Self(std::array::from_fn(|_| AtomicU64::new(0)))
    }
}
impl Counters {
    pub(super) fn observe(&self, phase: Phase, outcome: Outcome) {
        self.0[phase as usize * 7 + outcome as usize].fetch_add(1, Ordering::Relaxed);
    }
    pub(super) fn render(&self, output: &mut String) {
        output.push_str("# HELP memeloop_token_center_plugin_execution_observations_total Host invocation wait outcomes, not delivery or policy acceptance.\n# TYPE memeloop_token_center_plugin_execution_observations_total counter\n");
        for phase in Phase::ALL {
            for outcome in Outcome::ALL {
                let _ = writeln!(
                    output,
                    "memeloop_token_center_plugin_execution_observations_total{{phase=\"{}\",outcome=\"{}\"}} {}",
                    phase.as_str(),
                    outcome.as_str(),
                    self.0[phase as usize * 7 + outcome as usize].load(Ordering::Relaxed)
                );
            }
        }
    }
}
