# MemeLoop manifest-only fork

This directory is the unmodified source release of `rust_decimal` 1.42.1 from
crates.io, checksum
`be2a24f50780bc85f09cc6ac299bdf1424302742d77221106859c9d8b102126a`.
Its upstream repository is <https://github.com/paupino/rust-decimal> and its
license is MIT; the upstream `LICENSE`, `Cargo.toml.orig`, and provenance files
are retained.

The upstream archive's nested `Cargo.lock` is intentionally not retained. A
dependency's nested lockfile is ignored by Cargo when this crate is patched
into the workspace, and retaining it would create a second, inactive scanner
surface containing dependencies that are not in the product graph. The
published archive checksum still authenticates that upstream artifact.

MemeLoop changes only the normalized `Cargo.toml`: the unused optional `rkyv`
feature/dependency, its validation companion, its example target, and its
development-only `rkyv` dependency were removed. Runtime source files are
byte-for-byte identical to the crates.io archive. This prevents the otherwise
unreachable optional archived-serialization dependency from remaining in the
resolved lockfile and RustSec audit surface.

To audit an update, download the exact crates.io archive, verify its published
SHA-256 checksum, compare every file other than normalized `Cargo.toml` and this
notice, then repeat the same manifest-only deletion.

`tests/ops/rust-decimal-vendor-contract.sh` enforces the published archive
checksum, the complete unmodified upstream tree digest (excluding only the
normalized manifest, nested lock, and MemeLoop audit files), the exact
manifest-only patch against the authenticated upstream manifest, and the
absence of `rkyv` from the fork manifest and product lockfile.
