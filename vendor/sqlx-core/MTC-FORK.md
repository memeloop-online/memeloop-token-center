SQLx core 0.9.0, copied from crates.io. MTC adds an optional PostgreSQL
connection cancellation-key accessor to the Any connection backend trait.
The key is sensitive and must never be logged or exposed to API clients.
The companion sqlx-postgres fork provides the implementation. Remove both
patches when upstream exposes an equivalent connection-scoped cancellation API.
