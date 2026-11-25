pub(crate) mod allocation;
mod clickhouse;
mod contract;
pub(crate) mod event_original;
pub mod migration;
mod monitoring;
mod native_collections;
pub(crate) mod native_original;
mod query;
mod session_tasks;
mod sessions;
mod statistics;
mod target;
pub(crate) use target::HistoryTargetIdentity;
pub(crate) use statistics::StatisticsAccumulator;
mod usage;

pub use clickhouse::ClickHouse;
pub use contract::*;
pub use query::*;

pub(crate) const READ_MODEL_VERSION: i64 = 6;

pub const SCHEMA: &str = include_str!("schema.sql");

#[cfg(test)]
mod tests;

pub(crate) use sessions::SessionDirectory;

/// An operator-selected existing event revision; never allocates a revision.
#[derive(Clone, Debug)]
pub(crate) struct ExactEventExport {
    pub identity: HistoryIdentity,
    pub revision: i64,
}
impl ExactEventExport {
    pub(crate) fn matches(&self, row: &HistorySnapshot) -> bool {
        row.identity == self.identity && row.revision == self.revision
    }
}
