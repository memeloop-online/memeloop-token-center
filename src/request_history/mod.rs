mod clickhouse;
mod contract;
pub mod migration;
mod query;
mod statistics;

pub use clickhouse::ClickHouse;
pub use contract::*;
pub use query::*;

pub const SCHEMA: &str = include_str!("schema.sql");

#[cfg(test)]
mod tests;
