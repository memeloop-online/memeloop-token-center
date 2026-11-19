# wreq-util 3.0.0-rc.14: zstd security update

Official archive:
https://static.crates.io/crates/wreq-util/wreq-util-3.0.0-rc.14.crate

Archive SHA-256 (the original registry checksum):
`1d8d73c75be86fbde675988dbe5cb15f02567025c13f6ffab910361ad1ba6c89`.
Upstream VCS revision: `1f8fe6ea957271b07ebeb949123fc3aa6cd25222`.

All 55 upstream archive files are retained byte-for-byte except the normalized
Cargo.toml dependency constraint: zstd `0.13.3` becomes `0.14.3`.
MEMELOOP-MANIFEST.patch is the complete, reversible local delta;
UPSTREAM-SHA256.json inventories the authenticated archive. All Rust source,
Cargo.toml.orig, upstream Cargo.lock, examples, tests, metadata and Apache-2.0
LICENSE remain unchanged. The retained upstream lockfile documents that release;
the application's root Cargo.lock selects the upgraded dependency graph.

Upstream made the same zstd constraint change in:
https://github.com/0x676e67/wreq-util/commit/4d21e3b65d6846187e02802172073438944e6399

Using that newer wreq-util source wholesale would require Rust 1.98, whereas
this compatible release declares Rust 1.85. This patch keeps the existing
wreq-util version and API without changing the project's compiler requirement.
The root dependency and this manifest now share zstd 0.14.3, zstd-safe 8.2.0,
and zstd-sys 2.1.2+zstd.1.5.7. Their registry checksums in the root lockfile are
from the official crates.io sparse index. No advisory or source-policy exception
is introduced.

The CI provenance contract authenticates the official archive checksum,
compares the full file inventory, reverses only the pinned one-line manifest
patch without fuzz, and requires exact equality with the official archive.
It also checks the selected root lock entries and the distributed license.
Normal remote CI validates the locked graph, compilation, and security policy;
local provenance editing is not evidence that those checks have passed.
