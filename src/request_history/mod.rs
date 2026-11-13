pub(crate) mod allocation;
mod clickhouse;
mod contract;
pub(crate) mod event_original;
pub mod migration;
mod monitoring;
mod query;
mod session_tasks;
mod sessions;
mod statistics;
mod usage;

pub use clickhouse::ClickHouse;
pub use contract::*;
pub use query::*;

pub(crate) const READ_MODEL_VERSION: i64 = 3;

pub const SCHEMA: &str = include_str!("schema.sql");

#[cfg(test)]
mod tests;

pub(crate) use sessions::SessionDirectory;
