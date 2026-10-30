# wreq-util 3.0.0-rc.14: manifest-only zstd security upgrade

Official archive: https://static.crates.io/crates/wreq-util/wreq-util-3.0.0-rc.14.crate
Archive SHA-256: `1d8d73c75be86fbde675988dbe5cb15f02567025c13f6ffab910361ad1ba6c89`.

All upstream files are retained verbatim except Cargo.toml and the unused
upstream Cargo.lock (the application uses the root lockfile). The only patch
raises zstd from 0.13.3 to 0.14.3 so the application and browser emulation use
one repaired zstd/zstd-safe/zstd-sys graph. No runtime source changes.

MEMELOOP-MANIFEST.patch reverses to the authenticated upstream manifest;
UPSTREAM-SHA256.json inventories every other retained archive file. CI pins
the inventory and manifest digests and rejects unreviewed runtime files.
The original Apache-2.0 LICENSE and VCS receipt are preserved.

This addresses RUSTSEC-2026-0391, RUSTSEC-2026-0398, RUSTSEC-2026-0403
and RUSTSEC-2026-0410 without suppressing the dependency security gate.
