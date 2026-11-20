# SQLx core maintenance patch

Source: sqlx-core 0.9.0, crates.io SHA-256 `05b44e85bf579a8eeb4ceaa77a3a523baf2bf0e9bac7e40f405d537b5d2d5ccb`.

`Floating<Live>::return_to_pool`: the existing pool acquire timeout also bounds the complete returned-connection health/close future. Expiry drops the owned future, raw connection and pool size guard. Borrowed queries and transactions retain their existing lifetimes. SQL is never replayed.

Upstream Apache-2.0 and MIT licenses are retained.

Pooled `Transaction::begin` additionally preserves close-on-drop until BEGIN confirmation. Cancelled or failed BEGIN closes its owned socket; confirmed BEGIN restores the original caller flag. Borrowed connections and savepoints retain their existing handling.

All five pooled socket close entry points (explicit PoolConnection close and Live/Idle graceful/hard close) share the existing acquire-timeout maintenance budget. Idle close returns its size guard for the caller's replacement connection. The pool exposes a count of confirmed owned pooled Transactions through their Drop boundary; borrowed transactions are excluded.

The upstream standalone Cargo.lock is omitted. This library is built through the root workspace lock; no release entry point builds its standalone manifest. The original crate checksum above records the source snapshot.
