# MemeLoop SQLite dependency security patch

This directory retains 56 of the 57 files from the published `sqlx-sqlite` 0.9.0
archive, SHA-256 `488e99c397a62007e4229aec669a179816339afc6d2620ca6fa420dbee2e982c`.
The upstream repository is https://github.com/launchbadge/sqlx. The MIT and
Apache-2.0 licenses, original manifest, and VCS provenance are retained.
`MEMELOOP-UPSTREAM.json` records the SHA-256 of every original file.

Only the normalized Cargo.toml dependency constraint changes:
`libsqlite3-sys >=0.30.1, <0.38.0` becomes `>=0.39.1, <0.40.0`.
The upper bound follows upstream commit
10462fa4414c6c7f1ed1d39bb31678e3b726878c (PR #4502); the lower bound keeps
resolution on the patched SQLite 3.54.1 release for RUSTSEC-2026-0459.
All Rust sources are byte-for-byte identical to the published crate.
The workspace lockfile pins registry `libsqlite3-sys` 0.39.1 with checksum
`14a5f01b87b92705d07b433c06f96cbc764637d81a0bffb5734ce17cf0f9a2b3`.
Its enabled build dependencies remain cc, pkg-config, and vcpkg, already
satisfied by the workspace lockfile. No SQLCipher or buildtime-bindgen
feature is added. Cargo uses the workspace lockfile for this path dependency.

To review, authenticate the published archive by its SHA-256, compare all
56 retained files, and reverse MEMELOOP-MANIFEST.patch for the normalized manifest.
The inactive upstream Cargo.lock is omitted because Cargo uses the workspace
lockfile; its original hash and omission reason remain in MEMELOOP-UPSTREAM.json.
The checked-in upstream hashes provide the complete 57-file inventory; there are
no runtime-source modifications. Normal locked CI validates dependency
resolution, SQLite integration, and security policy. No local Cargo build,
test, dependency installation, or lockfile generation was performed.
