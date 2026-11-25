//! Operations performed by the schema capability's lifecycle publishers and
//! readers. These checks do not claim to audit unrelated application features.
use super::CapabilityRole;
#[derive(Clone, Debug, serde::Serialize)]
pub struct RelationOperations {
    pub table: &'static str,
    pub operations: &'static [&'static str],
}
pub fn requirements(role: CapabilityRole) -> Vec<RelationOperations> {
    let read = &["SELECT"][..];
    let append = &["SELECT", "INSERT"][..];
    let update = &["SELECT", "UPDATE"][..];
    let publish = &["SELECT", "INSERT", "UPDATE"][..];
    let queue = &["SELECT", "INSERT", "UPDATE", "DELETE"][..];
    let rows = match role {
        CapabilityRole::Gateway => vec![
            ("request_records", publish),
            ("request_record_locators", append),
            ("usage_reservations", publish),
            ("request_events", append),
            ("terminal_projection_outbox", publish),
            ("request_history_state", read),
        ],
        CapabilityRole::Worker => vec![
            ("request_records", update),
            ("request_record_locators", read),
            ("usage_reservations", update),
            ("request_events", append),
            ("terminal_projection_outbox", queue),
            ("request_history_state", read),
        ],
        CapabilityRole::Control => vec![
            ("request_records", read),
            ("request_record_locators", read),
            ("usage_reservations", read),
            ("request_events", read),
            ("terminal_projection_outbox", read),
            ("request_history_state", read),
        ],
        CapabilityRole::HistoryReader => vec![
            ("request_records", read),
            ("request_events", read),
            ("request_history_state", read),
            ("request_history_baseline_runs", read),
        ],
        CapabilityRole::Maintenance => vec![
            ("schema_migrations", read),
            ("request_records", read),
            ("request_history_state", read),
            ("request_history_baseline_runs", read),
        ],
    };
    rows.into_iter()
        .map(|(table, operations)| RelationOperations { table, operations })
        .collect()
}
