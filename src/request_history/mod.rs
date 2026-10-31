mod clickhouse;
mod contract;
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

pub const SCHEMA: &str = include_str!("schema.sql");

#[cfg(test)]
mod tests;
