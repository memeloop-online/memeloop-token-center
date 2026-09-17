//! Shared request-cost presentation semantics.
//!
//! Gross request cost remains immutable for settlement and audit. User-facing
//! request and session projections use the effective cost: a terminal failed
//! request marked not_observed is displayed as zero. Successful requests,
//! provider-reported usage, and pending requests retain their stored amount.

pub(crate) fn effective_displayed_cost_micros(
    cost_micros: i64,
    status_code: Option<i64>,
    usage_basis: Option<&str>,
) -> i64 {
    if status_code.is_some_and(|code| !(200..400).contains(&code))
        && usage_basis == Some("not_observed")
    {
        0
    } else {
        cost_micros
    }
}

#[cfg(test)]
mod tests {
    use super::effective_displayed_cost_micros;

    #[test]
    fn only_terminal_not_observed_requests_are_zeroed() {
        assert_eq!(
            effective_displayed_cost_micros(594, Some(503), Some("not_observed")),
            0
        );
        assert_eq!(
            effective_displayed_cost_micros(594, Some(502), Some("not_observed")),
            0
        );
        assert_eq!(
            effective_displayed_cost_micros(594, Some(499), Some("not_observed")),
            0
        );
        assert_eq!(
            effective_displayed_cost_micros(594, None, Some("not_observed")),
            594
        );
        assert_eq!(
            effective_displayed_cost_micros(594, Some(200), Some("not_observed")),
            594
        );
        assert_eq!(
            effective_displayed_cost_micros(594, Some(503), Some("provider_reported")),
            594
        );
        assert_eq!(effective_displayed_cost_micros(594, Some(503), None), 594);
    }
}
