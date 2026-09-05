use super::UpstreamFailureKind;

#[derive(Clone, Copy)]
struct PositiveMillis(i64);

impl PositiveMillis {
    const fn new(value: i64) -> Self {
        assert!(value > 0, "breaker durations must be positive");
        Self(value)
    }

    const fn get(self) -> i64 {
        self.0
    }
}

#[derive(Clone, Copy)]
pub(super) struct UpstreamHealthPolicy {
    probe_lease: PositiveMillis,
    probe_heartbeat: PositiveMillis,
    rate_limited_cooldown: PositiveMillis,
    unavailable_cooldown: PositiveMillis,
    invalid_response_cooldown: PositiveMillis,
    connection_cooldown: PositiveMillis,
}

impl UpstreamHealthPolicy {
    const fn new(
        probe_lease: PositiveMillis,
        probe_heartbeat: PositiveMillis,
        rate_limited_cooldown: PositiveMillis,
        unavailable_cooldown: PositiveMillis,
        invalid_response_cooldown: PositiveMillis,
        connection_cooldown: PositiveMillis,
    ) -> Self {
        assert!(
            probe_heartbeat.get() < probe_lease.get(),
            "probe heartbeat must be shorter than its lease"
        );
        Self {
            probe_lease,
            probe_heartbeat,
            rate_limited_cooldown,
            unavailable_cooldown,
            invalid_response_cooldown,
            connection_cooldown,
        }
    }

    pub(super) const fn base_cooldown_millis(self, kind: UpstreamFailureKind) -> i64 {
        match kind {
            UpstreamFailureKind::RateLimited => self.rate_limited_cooldown.get(),
            UpstreamFailureKind::Unavailable => self.unavailable_cooldown.get(),
            UpstreamFailureKind::InvalidResponse => self.invalid_response_cooldown.get(),
            UpstreamFailureKind::Connection => self.connection_cooldown.get(),
        }
    }

    pub(super) const fn probe_lease_millis(self) -> i64 {
        self.probe_lease.get()
    }

    fn probe_heartbeat_interval(self) -> std::time::Duration {
        std::time::Duration::from_millis(self.probe_heartbeat.get().unsigned_abs())
    }
}

// Threading operator configuration through every request guard would
// substantially widen this breaker-only change. Keeping construction private
// still makes an invalid lease / heartbeat relationship unrepresentable.
pub(super) const UPSTREAM_HEALTH_POLICY: UpstreamHealthPolicy = UpstreamHealthPolicy::new(
    PositiveMillis::new(30_000),
    PositiveMillis::new(10_000),
    PositiveMillis::new(30_000),
    PositiveMillis::new(15_000),
    PositiveMillis::new(15_000),
    PositiveMillis::new(5_000),
);

pub(crate) fn upstream_probe_heartbeat_interval() -> std::time::Duration {
    UPSTREAM_HEALTH_POLICY.probe_heartbeat_interval()
}
