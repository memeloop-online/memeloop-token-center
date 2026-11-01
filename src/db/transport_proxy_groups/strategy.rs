use super::*;
use crate::plugin::network_proxy_strategy::ProxyCandidate;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct AutoConfig {
    pub minimum_samples: i64,
    pub minimum_dwell_ms: i64,
    pub health_max_age_ms: i64,
    pub recovery_successes: i64,
    pub improvement_threshold: f64,
    pub signal_window_ms: i64,
}

impl Default for AutoConfig {
    fn default() -> Self {
        Self {
            minimum_samples: 8,
            minimum_dwell_ms: 120_000,
            health_max_age_ms: 180_000,
            recovery_successes: 2,
            improvement_threshold: 0.1,
            signal_window_ms: 300_000,
        }
    }
}

impl AutoConfig {
    pub(crate) fn validate(&self) -> Result<(), AppError> {
        if !(1..=10000).contains(&self.minimum_samples)
            || !(0..=3_600_000).contains(&self.minimum_dwell_ms)
            || !(10_000..=3_600_000).contains(&self.health_max_age_ms)
            || !(1..=8).contains(&self.recovery_successes)
            || !self.improvement_threshold.is_finite()
            || !(0.0..=1.0).contains(&self.improvement_threshold)
            || !(60_000..=86_400_000).contains(&self.signal_window_ms)
        {
            return Err(AppError::BadRequest(
                "invalid automatic proxy selection configuration".into(),
            ));
        }
        Ok(())
    }
}

fn health<'candidate>(
    candidate: &'candidate ProxyCandidate,
    now: i64,
    config: &AutoConfig,
) -> &'candidate str {
    if candidate
        .checked_at
        .is_some_and(|at| at <= now && now.saturating_sub(at) <= config.health_max_age_ms)
    {
        candidate.active_health.as_str()
    } else {
        "unknown"
    }
}

fn score(candidate: &ProxyCandidate, now: i64, config: &AutoConfig) -> f64 {
    let active = match health(candidate, now, config) {
        "healthy" => 100.0,
        "unhealthy" => -1000.0,
        _ => 0.0,
    };
    let observed = if candidate.sample_count >= config.minimum_samples {
        candidate.transport_success_rate.unwrap_or(0.5) * 100.0
    } else {
        50.0
    };
    active + observed - candidate.latency_ms.unwrap_or(1000).clamp(0, 10000) as f64 / 10000.0
}

pub(crate) fn choose_auto(
    candidates: &[ProxyCandidate],
    current: Option<Uuid>,
    now: i64,
    last_change: i64,
    config: &AutoConfig,
) -> Option<Uuid> {
    let retained = candidates
        .iter()
        .find(|candidate| Some(candidate.proxy_id) == current);
    let best = candidates
        .iter()
        .enumerate()
        .max_by(|(left_index, left), (right_index, right)| {
            score(left, now, config)
                .total_cmp(&score(right, now, config))
                .then_with(|| right_index.cmp(left_index))
        })
        .map(|(_, candidate)| candidate)?;
    let Some(previous) = retained else {
        return Some(best.proxy_id);
    };
    if previous.proxy_id == best.proxy_id {
        return Some(previous.proxy_id);
    }
    if candidates
        .iter()
        .all(|candidate| health(candidate, now, config) == "unhealthy")
    {
        return Some(previous.proxy_id);
    }
    if health(best, now, config) == "healthy"
        && best.consecutive_successes < config.recovery_successes
    {
        return Some(previous.proxy_id);
    }
    let current_failed = health(previous, now, config) == "unhealthy";
    if !current_failed && now.saturating_sub(last_change) < config.minimum_dwell_ms {
        return Some(previous.proxy_id);
    }
    if score(best, now, config) - score(previous, now, config)
        < config.improvement_threshold * 100.0
    {
        return Some(previous.proxy_id);
    }
    Some(best.proxy_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(status: &str, successes: i64, rate: Option<f64>) -> ProxyCandidate {
        ProxyCandidate {
            proxy_id: Uuid::now_v7(),
            proxy_version: 1,
            active_health: status.into(),
            checked_at: Some(1_000_000),
            latency_ms: Some(10),
            consecutive_successes: successes,
            sample_count: if rate.is_some() { 20 } else { 0 },
            transport_success_rate: rate,
        }
    }

    #[test]
    fn poor_candidates_keep_an_attemptable_exit_and_recovery_is_fenced() {
        let config = AutoConfig::default();
        let primary = candidate("unhealthy", 0, Some(0.2));
        let mut backup = candidate("unhealthy", 0, Some(0.1));
        assert_eq!(
            choose_auto(
                &[primary.clone(), backup.clone()],
                Some(primary.proxy_id),
                1_000_000,
                0,
                &config
            ),
            Some(primary.proxy_id)
        );
        assert_eq!(
            choose_auto(
                &[primary.clone(), backup.clone()],
                None,
                1_000_000,
                0,
                &config
            ),
            Some(primary.proxy_id)
        );
        backup.active_health = "healthy".into();
        backup.consecutive_successes = 1;
        assert_eq!(
            choose_auto(
                &[primary.clone(), backup.clone()],
                Some(primary.proxy_id),
                1_000_000,
                0,
                &config
            ),
            Some(primary.proxy_id)
        );
        backup.consecutive_successes = 2;
        assert_eq!(
            choose_auto(
                &[primary.clone(), backup.clone()],
                Some(primary.proxy_id),
                1_000_000,
                999999,
                &config
            ),
            Some(backup.proxy_id)
        );
    }

    #[test]
    fn no_samples_are_not_perfect_health_and_dwell_prevents_flapping() {
        let config = AutoConfig::default();
        let primary = candidate("unknown", 0, None);
        let backup = candidate("healthy", 2, Some(1.0));
        assert_eq!(
            choose_auto(
                &[primary.clone(), backup.clone()],
                Some(primary.proxy_id),
                1_000_000,
                999999,
                &config
            ),
            Some(primary.proxy_id)
        );
        assert_eq!(
            choose_auto(
                &[primary.clone(), backup.clone()],
                Some(primary.proxy_id),
                1_000_000,
                0,
                &config
            ),
            Some(backup.proxy_id)
        );
        assert_eq!(
            choose_auto(
                &[primary.clone(), backup.clone()],
                Some(primary.proxy_id),
                2_000_000,
                0,
                &config
            ),
            Some(backup.proxy_id)
        );
    }
}
