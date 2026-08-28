# Development handoff

This is the resume point for the next Token Center development agent. Read
[Product requirements](product-requirements.md) first, then
[Architecture](architecture.md). Those documents preserve the agreed product
scope and rejected designs; this document preserves the implementation state and
remaining acceptance gates, updated on 2026-08-28.

## Current 2026-08-28 local convergence evidence

This section supersedes older local-development status below. It does not yet
authorize a rollout: one exact-SHA GitHub Actions release must still pass and
publish the complete immutable three-image set before API2 can be updated. API3
remains forbidden until the user explicitly opens the production window.

- This convergence commit is based on clean remote-parity master
  `aae6d0fbd651251036a2c1588f84f95fb881f74e`. No GitHub Actions run for the
  continuation existed when this local evidence was recorded.
- Schema generations v56-v58 implement the archive schema-v2 stable snapshot,
  tombstone, immutable quarantine-version and bounded staging contracts. Exact,
  unlinked and quarantine reconciliation, semantic projection changes,
  tombstones, legacy and stable checkpoints, and staging cleanup are committed
  in one target transaction. The importer seals the same input and manifest file
  descriptors, verifies size plus SHA-256/BLAKE3, enforces the delta chain and
  offline baseline, handles replacement/move/delete-recreate lifecycles, and
  processes up to the one-million-session protocol limit in bounded batches.
- An isolated PostgreSQL 17 schema-only copy of the live API2 v55 database was
  migrated through v58. The real PostgreSQL archive advisory-lock and locator
  CAS integration gate passed. The temporary database and port-forward were
  removed afterward; the live API2 database remained read-only, and its legacy
  exact, unlinked and quarantine blocker counts were all zero.
- All repository-owned automation and operational scripts are TypeScript on the
  fixed Node.js 24 runtime. No tracked Python, shell or CommonJS script remains;
  importer packaging builds seven ESM entries plus their SQL assets. The root
  operator suite passed 21 tests with three explicit environment-only skips, and
  the targeted exporter, CPA transport, importer-image, legacy and memory
  contract suites passed 36/36 before the final GHCR workflow-contract rerun.
- Fixed Rust 1.95 passed `cargo check --locked --all-targets --all-features`,
  strict all-target/all-feature Clippy and the complete test suite: 386 library
  tests, all integration binaries, and 70/70 Cucumber scenarios with 379/379
  steps. OpenAPI tests passed 18/18 with 106 paths and 126 operations. Web
  typecheck, production build and localization/contracts passed; real Chromium
  passed all 19/19 scenarios and 140/140 steps. The browser run exposed and
  verified the fix for an optional empty `video_models` schema blocking ordinary
  HTTP upstream edits while the enabled SiliconFlow driver remains fail-closed.
- A fixed-toolchain optimized binary passed the unchanged full memory acceptance
  profile, 35/35 checks. The 15-minute soak had zero request failures, a 0.038
  MiB/min RSS slope against the 2.0 ceiling and a 9.148 MiB retained delta. Twelve
  concurrent 16 MiB streams used a 56.687 MiB gateway RSS delta; standard Images
  and Codex Responses-tool images used 88.894 and 57.332 MiB respectively against
  the unchanged 128 MiB ceiling. A 500 MiB asset was completely archived and
  downloaded, and peak gateway RSS was 124.883 MiB against the 224 MiB deployment
  budget. The binary SHA-256 remained
  `04b0718d80cd4412715b2c5559651877cf6a198e6901c9f2bc3910ea09718dd7`.
- The final TypeScript GHCR publication/verification review passed locally.
  Both publishing jobs install fixed Node.js 24.18.0 before running repository
  logic. Tested TypeScript binds each SHA tag to its build digest, validates the
  BuildKit SBOM/provenance and OCI source/revision labels, rejects incomplete or
  tampered evidence, verifies the complete three-image set by immutable digest,
  and writes one no-overwrite release manifest. Typecheck, 21/21 runnable ops
  tests, four focused release/language contracts, structured workflow policy and
  `git diff --check` passed; three Docker/PostgreSQL environment tests remain
  explicit CI-required skips locally. The next gate is one GitHub Actions run
  for this exact convergence commit. Do not spend CI minutes on an intermediate
  revision and do not tag archive v0.8.0 before that exact MTC release is green.

## Current 2026-08-27 API2 trial evidence

This section supersedes the older release-truth snapshots below.

- Clean master `495733326a72ff6b97cafa4316f91bdf7eb1508f` passed
  GitHub Actions run
  [`33085491007`](https://github.com/memeloop-online/memeloop-token-center/actions/runs/33085491007),
  including every functional, migration, browser, packaging, security, image
  scan, SBOM/provenance and immutable-release verification gate. The unchanged
  15-minute memory gate completed 18,013 requests with zero failures, a 1.936
  MiB/min RSS slope and 22.653 MiB retained delta. It published verified
  immutable digests: service
  `sha256:17ffc25076d43beea236de3fe7d0af37bd677a90e1e2b50150af43d5aed1b3e4`,
  importer
  `sha256:996a51702ba1231f45222f0327942ce616beaa9cda7384b8f67821a0eda5e37a`
  and plugin-installer
  `sha256:fe88a7eb1994ab577ce1d5ac01b5bcb6647234806880aa7940e55337d8155ec1`.
  Do not promote these interim digests: post-run dogfood exposed the worker
  scheduling defect described below, whose replacement SHA must pass the same
  release workflow.
- Clean master parent `3f648975892e61751bbda44b47053b82a134dff9` passed
  GitHub Actions run
  [`33065339748`](https://github.com/memeloop-online/memeloop-token-center/actions/runs/33065339748),
  including the optimized 15-minute memory gate, and published verified
  immutable digests: service
  `sha256:15398a1d0a722dc38c7cfdc8860260e0aa880020eaea54e8d00fbbc009f3ce5f`,
  importer
  `sha256:d6f1e1bbeb5a7b6d9d78e3e5712be1ce9b8b606d911db364248e792c9da7a25f`
  and plugin-installer
  `sha256:849f3f8ae1d20fbb418c535254616b96e8af933bb970d2d3de9d8864adcf22a0`.
- Those digests are deployed only to the reversible API2 trial. Control,
  gateway and worker report the exact source revision and remain Ready with no
  restarts. The old CPA rollback point and API3 remain untouched.
- Real Chat Completions and Responses requests returned the requested exact
  marker strings. A real Codex CLI request used the Responses wire protocol and
  captured controlled session name, task kind, agent ID and labels in the
  session/request structure. The current dynamic subagent/parent relationship
  still depends on applications sending the documented metadata headers.
- A live standard Images request exposed two configuration defects: `/v1` API
  bases were receiving a duplicated `/v1` path, and the built-in `http-json`
  schema rejected the exact generated-asset origin allowlist already enforced
  by the archive layer. The current source change fixes both and extends the
  versioned CPA transport policy with exact `result_origins_by_base_url`.
  Targeted Rust tests, TypeScript typecheck and all ten CPA importer tests pass
  locally. Do not publish another candidate until the remaining local gates are
  green.
- Follow-up CI run
  [`33077065478`](https://github.com/memeloop-online/memeloop-token-center/actions/runs/33077065478)
  passed every functional, migration, security, packaging and browser job but
  correctly blocked publication because the 15-minute soak slope was 2.038
  MiB/min against the unchanged 2.000 MiB/min ceiling. The same evidence showed
  18,016/18,016 successful requests, 139.648 MiB lifetime gateway high-water,
  24.227 MiB retained delta and every other memory check passing. The follow-up
  removes a newly introduced full URL parse from every inference request while
  retaining the `/v1` de-duplication contract; it does not raise or round the
  threshold. Its URL/proxy tests and strict Clippy pass locally. Do not use the
  failed run as a release source; its GHCR jobs were skipped.
- A client-disconnected synchronous image execution proved that the database
  correctly made an expired image lease eligible for orphan settlement, but
  the worker invoked that settlement only from the six-hour partition
  maintenance timer. This could leave a pending request and reserved balance
  stranded for up to six hours. The current change gives orphan settlement its
  own five-minute timer while preserving the 30-minute age cutoff, active-lease
  exclusion and 100-row batch bound. All six synchronous-image recovery tests,
  the orphan-settlement test, formatting and strict library Clippy pass locally.
  The live stale row must become a terminal `request_expired` record with a
  released reservation after the replacement worker starts; do not repair it
  manually before that rollout proof.
- After applying the exact reviewed asset origin to the trial, a real image
  completed with HTTP 200, produced one 591,296-byte archived asset, replayed
  the same idempotency key without a second generation, and settled exactly
  USD 0.01. The request and result staging attempts are both bound. No wildcard
  origin or private-network relaxation was used.
- Credential acquisition and the distinction between service and client
  credentials are recorded in
  [API2 trial dogfood access](operations/api2-trial-dogfood.md). The browser must
  remember either credential until the user manually clears it.
- Legacy CPA credential attachment, complete session-archive delta import,
  failed OAuth/API upstream remediation, browser UI acceptance and final
  rollback rehearsal remain release gates. API3 is forbidden until the user
  explicitly opens the production window.

## Current 2026-08-27 release truth

- GitHub Actions run
  [`32947649161`](https://github.com/memeloop-online/memeloop-token-center/actions/runs/32947649161)
  passed for clean master SHA
  `312adf5289fea7cf2bc9a8e3548f7dcc07f0f30a`, including publication and
  verification. Its immutable digests are service
  `sha256:ab6f0ce0288b6322af97565d3bb84eb873ab85fa26b2e5e21e0633b2bd574003`,
  importer
  `sha256:ed6cdbec0f9a0820193be14a47fabf81f331a8297474abfbded19702e7f3d022`
  and plugin-installer
  `sha256:66da0edbe7aa56cd5bf6a1cbd70ba1aa5f0c329c2e71e1ed8c6304c5c735359f`.
- Those digests are not a completed trial. The live source dry-run proved the
  Westlake upstream target is private while the importer emitted
  `network_scope: public`; apply correctly stopped and its temporary migration
  credential was revoked. The current continuation adds a separate strict
  owner-only version-1 transport policy for exact private target base URLs.
  Scope is not inferred from proxy presence, but each approved private target
  must carry a private local-DNS SOCKS5 proxy before any target request. URLs
  remain write-only, the server still revalidates DNS/IP and global authority,
  and changing scope on replay conflicts instead of duplicating an account.
- External probes of the claimed trial operator URL alternated between HTTP 403
  and 200 over six consecutive requests. No upstream apply/exact replay,
  archive apply/exact replay, stable three-role ingress, live browser dogfood or
  Codex CLI text/image evidence has been supplied. Therefore the operations
  task's broad “complete” statement is rejected as release evidence.
- The private-target continuation is local until its TypeScript, image and Job
  contracts pass and one new exact-SHA CI publishes replacement immutable
  digests. Conserve CI by completing every local gate before that single push.
  API3 remains forbidden.

## Active 2026-08-24 release override

- The production deployment window remains closed on 2026-08-24. A reversible
  CPA/API2 trial is allowed, but API3 changes are explicitly forbidden until
  the user declares the next production window open. Trial success prepares a
  release; it does not implicitly open that window.
- API3 is the production target. Deploy one exact SHA and the same immutable
  service/importer/plugin-installer digests to CPA/API2 first.
- Record the old CPA image, data backup and route configuration before the
  trial; they are the immediate rollback point. Do not mutate API3 during trial.
- The release agent must personally complete browser validation of existing CPA
  accounts/subscriptions/history, unified upstream/OAuth, provider/route/
  credential groups, credentials, price synchronization, usage/session views,
  light/dark, Chinese/English, archives and multimodal accounting.
- The release agent must also send real text and image requests through the
  CPA/API2 trial endpoint with Codex CLI.
- Only after all evidence is green and the user explicitly opens the production
  window may the same digests be released to API3. This order and window gate
  supersede every API3-first trial instruction later in this file.

## Repository and safety boundary

- Canonical repository: private `memeloop-online/memeloop-token-center` on
  GitHub.
- Default branch: `master`.
- Canonical remote `master` at convergence start:
  `bd9d2e251ece8bbe610998a3d7bb91752d30c84a`.
- Integration branch: `fix/completion-p0-p1` in
  `/home/token-center-dev/worktrees/completion-p0-p1`.
- GitHub Actions and GHCR are the only maintained release path. There is no
  Forgejo mirror, Forgejo Actions or Token Center Harbor publisher.
- All repository automation and operational helper scripts must be TypeScript
  on the exact Node.js 24 runtime. Remove tracked Python, shell and CommonJS
  scripts, their shebangs/subprocess launchers, Python-only test harnesses and
  runtime dependencies while preserving CLI contracts and security properties.
  GitHub Actions `run` blocks are runner orchestration only; repository logic is
  called through reviewed TypeScript entry points.
- Do not build on the Westlake physical root disk. Source, Cargo state, temporary
  files and build cache belong on the Longhorn-backed Coder workspace.
- Coordinate Kubernetes, GitOps, migration execution, storage cleanup, rollout
  and cluster validation with
  `codex://threads/01a00a1e-3a18-7b82-8a36-a663c0ab6adc`; always record target,
  impact, read-only baseline and rollback first.
- The CPA/API2 trial rollout is authorized under the active override above, but
  irreversible migration barriers and removal of the old CPA rollback point are
  not. API3 remains unchanged until both trial approval and a later explicit
  production-window declaration.

No secret, client credential, service token or OAuth material belongs in this
document, commits, logs or test fixtures.

## Historical convergence commits

The following commits are the source changes that must remain in the integration
history:

| Commit | Purpose | Evidence |
| --- | --- | --- |
| `62cab854d7636f1b542797bd97169531ceedc9e3` | Stream synchronous Codex Responses image response segments instead of retaining multiple 15 MiB copies. | Strict Clippy/check and targeted tests passed. Exact short memory harness passed: idle 30.797 MiB, image delta 65.074 MiB, image peak 95.871 MiB, 2/2 responses, 15,379,225 bytes each. |
| `37a50fe0d9a2709c036f02deea961bf2ebf0a9a3` | Source commit for bounded standard OpenAI Images `b64_json` parsing and segmented response/archive. | Format, all-target/all-feature check, 11/11 focused library tests and harness syntax passed. |
| `9eaf4af` | Cherry-pick of `37a50fe` onto the integration branch. | Tree verified identical to the source commit; full memory acceptance has not run. |

The short memory report for `62cab85` is available only as ephemeral Coder
evidence at `/tmp/mtc-memory-short-62cab85.json`; its key measurements are
recorded above so the handoff does not depend on that temporary file. The tested
release binary SHA-256 was recorded in that report. New acceptance evidence must
always record the exact source and complete binary/image digest.

## CI and verification state

GitHub Actions run `32671474947` tested clean source
`69a23aa4d8e72669b97904e9ecc70dce750f7f1b`. Every pre-publication job passed,
including Rust, web/E2E, API contracts, migrations, dependency and repository
security, packaging, and the formal 15-minute memory acceptance. The exact-SHA
memory report passed all functional and resource gates: stream RSS delta
50.891 MiB, synchronous-image delta 77.637 MiB, 500 MiB asset gateway delta
0.223 MiB, soak failures zero, soak slope 0.863 MiB/hour, retained delta
15.395 MiB, and process high-water mark 108.332 MiB.

Publication did not complete. The Debian-based importer candidate
`sha256:cad3c27966324bc92580443fda501648ef9c7497d16f69e12c5202f090f52ea9`
failed its HIGH/CRITICAL Trivy gate, so the service and plugin-installer jobs
were cancelled to conserve CI time and no combined release manifest exists.
That partial digest is forbidden for deployment. The importer runtime is now
pinned to Alpine 3.23.5; equivalent-rootfs validation reported zero
HIGH/CRITICAL findings while preserving Node.js 24, PostgreSQL 18 and SQLite
operator contracts. See
[the importer runtime scan evidence](operations/2026-08-24-importer-runtime-scan.md).
The final image build, contract, scan and three-image manifest still require one
new exact-SHA GitHub Actions run.

Run `32678530047` then tested clean TypeScript-only source `79d2b08`. Every
pre-publication gate passed, including the formal exact-SHA memory acceptance,
and the hardened Alpine importer publish/scan/provenance verification succeeded
at partial digest
`sha256:3584b438e242ebf9d3879c18cea357855e61de64f3d3cc0a8440f3d8d42e1a6b`.
The plugin-installer publish failed before producing an image because its
Debian mirror default used plain HTTP and the runner received HTTP 403 responses
from TUNA; the service publish was cancelled immediately to conserve CI time.
No combined manifest exists and every partial digest remains forbidden for
deployment. Candidate `3144183` removed the vulnerable final Debian utility
layer, but its packaging run `32684971435` exposed a certificate bootstrap
error: Debian slim had no CA bundle before the custom HTTPS apt mirror was
contacted. That run was cancelled after about one minute.

Run `32686030562` then tested clean SHA
`16e46820645c2a8b37ae660fed1fde488ff4b330`. Every source, packaging and formal
memory gate passed, the importer published/scanned successfully, and the plugin
image built. Its unchanged final-image Trivy policy correctly rejected
`libssl3t64` from the distroless `cc` base and fixed HIGH findings in the
official Cosign 3.1.3 binary's Go 1.26.4 standard library, x/mod, x/text and
gRPC. The service build was cancelled immediately. Partial plugin digest
`sha256:c490747f83c926218db357419b193d63b11abcfd43fb3238803374d8c6a41238`
and all other partial outputs remain forbidden.

The current candidate uses the distroless Debian 13 `base-nossl` runtime and
copies only the Rust binaries' required `libgcc_s.so.1`. Because upstream has no
post-3.1.3 release, it reproducibly rebuilds the GitHub-verified v3.1.3 tag
commit as `v3.1.3-mtc.1` using fixed Go 1.26.7 and a checked-in dependency
security patch. A clean-source replay verified the module sums and the resulting
amd64 binary has zero HIGH/CRITICAL findings under the same local Trivy 0.70.0
database. The host has no Docker client, so the real Docker contracts and exact
three final-image Trivy scans remain mandatory in one new exact-SHA CI run.
See [the service and plugin runtime preflight](operations/2026-08-24-release-runtime-preflight.md)
for the exact local linkage and Cosign evidence.

GitHub Actions run `32639275451` tested
`92e23e882771b69be587732eb31eb02ba0a92cc3`. Every job except
`memory-acceptance / optimized-15-minute-harness` passed; GHCR publication and
release verification were correctly skipped. A local exact-SHA full run kept
all memory/resource measurements inside their limits but counted two failed soak
requests. Diagnostic reproduction identified a Node.js 24.18.0 Undici internal
parser assertion under long-lived `fetch` plus `Connection: close`; a subsequent
fully native-HTTP diagnostic also exposed real cross-process SQLite lock waits.
Commit `54fd84f` removed Undici from every harness HTTP phase, waits for
child shutdown, uses WAL snapshots plus immediate SQLite write transactions,
retains a zero-failure gate and records bounded error categories. Its dirty-tree
300-second diagnostic completed 5,538 soak requests with zero failures at
18.449 RPS; every functional and resource check passed, with a 126.359 MiB
gateway lifetime high-water mark against the 224 MiB process budget. This is
root-cause evidence, not exact release evidence. Clean candidate `5cbeaede59b8121165cfcddfecff4ff16774020f`
then passed every functional, 500 MiB asset and 900-second soak check with zero
soak failures, but correctly failed the unchanged 128 MiB stage gates: 12 × 16
MiB streams reached 151.714 MiB and the following two-image phase reached
160.515 MiB. Commit `7210184` bounds simultaneously owned 5 MiB proxy multipart
archive buffers to four without reducing observed upstream concurrency. A
12-stream dirty-tree diagnostic then measured 68.007 MiB for streams, 103.699
MiB for images and 130.945 MiB process high water, all green. Commit `69a23aa`
then passed the final clean 900-second / 500 MiB acceptance profile in run
`32671474947`, as recorded above. No release digests or rollout exist for
`92e23e8` or `5cbeaed`.

The implementation at `92e23e8` already includes the single-pass Responses
visitor, exact HTTP/archive/idempotent-replay image bytes, schema v54 accounting,
all repository scripts in TypeScript, and the converged local Rust/web/operator
gates. The historical status below explains how that state was reached.

GitHub Actions run `32579001708` tested `bd9d2e2`. Every gate except the memory
acceptance job passed. The failing workload was real: two concurrent synchronous
Responses-tool images produced a 152.504 MiB RSS delta against the 128 MiB limit.
Commit `62cab85` fixed that measured path in the short harness. Commit `9eaf4af`
then applied the analogous bounded design to standard OpenAI Images, but no full
CI run has tested either integration commit.

Do not infer release readiness from the targeted results. No new CI, image,
deployment, migration or browser dogfood was started during convergence.

## Historical blocking work completed at `92e23e8`

At the earlier convergence point, the Responses image parser was not ready for
release. It recursively
parses nested `RawValue` subtrees; at the maximum JSON depth a large leaf can be
rescanned repeatedly, turning a bounded 16 MiB body into approximately 2 GiB of
parser work. Replace it with a single-pass root `DeserializeSeed`/`Visitor`:

- process only the normative root `output` recursively and the typed root
  `usage`;
- skip unrelated root fields with `IgnoredAny`;
- keep malformed JSON mapped to `upstream_image_invalid_json` and semantic
  shape errors mapped to `upstream_image_invalid_payload`;
- preserve borrowed byte ranges, duplicate-image rejection, usage allowlisting,
  depth limits and the 16 MiB response bound.

Add a real exact-byte acceptance scenario for both synchronous image protocols.
The first HTTP response body, persisted archive object and same-idempotency replay
must be byte-for-byte equal, and `Content-Length` must equal the shared segment
length. Do not satisfy this only by concatenating segments twice in a unit test.

The standard OpenAI Images path also still needs the short and full concurrent
memory harness. Keep the 128 MiB limit; do not raise it to make the gate pass.

All items in this historical section are now implemented. The 128 MiB image gate
was retained and the final clean acceptance run passed at `69a23aa`; the current
release blocker is the importer runtime image attestation described above.

## Current release resume order

### 2026-08-25 continuation

The working tree after release `84f768e` adds schema v55 semantic execution
metadata and a responsive console pass. Clients can declare session names,
W3C trace/span context, agent ancestry, task kinds and bounded non-secret
metadata; existing Codex session/parent/Merkle evidence remains available when
those declarations are absent. The current continuation additionally projects
session/turn/parent/response/branch/compaction/client evidence as a separate
`structure` object. Session detail renders declared versus inferred request
lanes, an elapsed-request flame view, request-count task distribution and
currency-separated per-agent/per-task billed cost. The Codex custom-provider
configuration and the separate future OTLP event boundary are documented in
`docs/semantic-execution-metadata.md`.
Operator and self-service credentials are remembered in browser-local storage
until the user explicitly clears the corresponding role. Single-tenant service
credentials select their only tenant automatically; creation/onboarding forms
are collapsed until requested and the ordinary-user portal is linked from the
operator header. These changes are local and tested directionally; they are not
yet a pushed release, immutable image, migrated live trial or API2/API3 change.

The isolated API2 candidate built from `84f768e` remains the runtime baseline
while current CPA/CPAMP Longhorn snapshots are imported. API3 is explicitly
outside the current window and must remain unchanged until the user separately
opens the production window. Do not reuse the earlier v54 images for the v55 UI
or schema, and do not trigger CI until the complete local Rust/TypeScript,
fresh-PostgreSQL, browser and migration gates are green.

The importer image now bundles the reviewed TypeScript session-archive exporter
as `/usr/local/bin/export-cpa-session-archive-delta` and installs its `flock`
runtime dependency. The sealed JSONL is still consumed by
`/usr/local/bin/import-cpa-session-archive` from the matching service image.
This closes the observed packaging gap without introducing Python or turning
`audit-cpa-migration` into an importer.

The live scratch dry-run later exposed legacy CPAMP rows whose `failed` flag was
true while `fail_status_code` was 2xx/3xx. The importer must treat the flag as
authoritative: preserve only failed 4xx/5xx codes and normalize every other
failed code to `502`/`upstream_error`. A dedicated PostgreSQL acceptance fixture
is added and must prove the request fact and daily aggregate remain failures
after exact replay. Do not use an older importer digest for the real candidate
apply.

GitHub Actions run
[`32924611225`](https://github.com/memeloop-online/memeloop-token-center/actions/runs/32924611225)
tested clean SHA `888f60fc01780dcddb13738434d32c461ddacede` and completed
successfully. All source, packaging, migration, security, 900-second memory and
three-image publication/verification jobs passed. Its immutable digests are
service `sha256:f2a6fd9f0e228e98e586f333e24028f4b963d458d4847c3e16826d7f5eeb9ffc`,
importer `sha256:d04ef67a310ae490cace21e56079148087011035aeaeade04b56ae2c86dbbec2`
and plugin-installer
`sha256:eef9cedb69ee66db1241d402193ba7853daf2b14cf9e8445684995104a0d35a1`.
The memory gate measured 111.098 MiB lifetime HWM, 70.499 MiB standard Images
delta, 69.719 MiB Responses-tool Images delta, 8.226 MiB for the exact 500 MiB
download and 17,783/17,783 successful soak requests. The packages are
anonymously pullable, but these digests are not deployable: the subsequent real
source dry-run found two lossless-compatibility gaps absent from the synthetic
fixtures.

The real source has one API-key-entry-level private SOCKS5 proxy without proxy
authentication. The current local continuation adds a write-only
`api_key_proxy` credential, encrypted proxy URL, independently pinned target and
proxy endpoints, local-DNS-only SOCKS5 (no `socks5h`/HTTP proxy resolution
bypass), global authority for private-proxy creation/change, ordinary
key rotation that preserves the approved route, OpenAPI/catalog support and a
count-only TypeScript importer migration. The real v0.7.21 archive source also
returns RFC3339 numeric offsets with 5–9 fractional digits, while the later
stable contract requires canonical six-digit UTC `Z`. The exporter now compares
legacy timestamps at nanosecond precision and normalizes only the legacy
projection; stable responses remain byte-form strict.

Current local release evidence after these changes: rustfmt, Clippy with
`-D warnings`, and `cargo test --locked --all-targets --all-features` pass,
including 370 library tests and every integration binary; Rust Cucumber reports
69/69 scenarios and 373/373 steps. Root TypeScript and all five Node files report
45/45 tests, and the OpenAPI route contract remains 106 paths/126 operations.
Web typecheck, 24/24 localization/security contracts, production build and the
Chromium suite (19/19 scenarios, 140/140 steps) pass. The browser suite sends a
real Codex-style semantic request through HTTP and reads it back through the
session API/UI; credential reload/manual clear covers both portals.

A dirty-working-tree short optimized-memory run passed the two unchanged
128 MiB image gates at 95.879 MiB for standard Images and 61.719 MiB for Codex
Responses-tool Images. Both proved exact `Content-Length`, byte-identical replay
and no replay upstream call. The 100 MiB archive download added 0.277 MiB at the
gateway, soak failures were zero, and process lifetime HWM was 123.641 MiB
against the 224 MiB process budget. Root/web npm audits report zero
vulnerabilities and tracked Python remains zero. See
[the retained compatibility evidence](operations/2026-08-26-legacy-source-compatibility.md).
The full test run also exposed a real SQLite deferred-transaction lock-upgrade
race in legacy credential attachment. That path now claims the writer slot at
transaction start, has a deterministic competing-writer regression, and the
formerly intermittent end-to-end scenario passed five focused repetitions plus
both complete Cucumber executions.
This is local regression evidence only. One final exact-SHA CI is still required
before any upstream/archive target write, digest rollout or browser/CLI trial.
The local host has neither Docker/Podman nor PostgreSQL/Helm, so final-image,
fresh PostgreSQL v1→v58 and Helm gates remain for that single run rather than
being represented as local evidence.

Run `32943606008` subsequently passed every pre-publication job and the full
memory acceptance for `f3c342027e52c21f455b7c17d201bd8d133b858e`, but the
importer publish job discovered the newly disclosed/fixed CVE-2026-14456 in the
Alpine 3.23.5 base's `libcrypto3`/`libssl3` `3.5.7-r0`. Its partial importer
digest is forbidden and the run produced no complete release manifest. The
current continuation explicitly upgrades those two packages to the v3.23
repository's fixed `3.5.8-r0` before installing runtime tools; the release
contract requires the upgrade and no scanner waiver was added. One new
exact-SHA run must scan the resulting image and produce a complete manifest.

1. Validate the pinned Alpine importer, base-nossl service/plugin runtime,
   patched Cosign and TypeScript operator contracts locally. Do not change
   security or memory thresholds.
2. Push that exact master SHA once and inspect the one GitHub Actions run. GHCR
   publication is allowed only when every required job and final-image security
   scan is green.
3. Record and verify the three immutable GHCR digests and the combined release
   manifest; never deploy the SHA tag.
4. Coordinate
   a CPA/API2 trial deployment with a recorded old-CPA rollback point. Keep API3
   unchanged.
5. Reconcile imported CPA/CPAMP accounts, subscriptions, legacy credential
   access, usage totals, request archives and unlinked-session counts. Dogfood
   operator and self-service flows in a real browser in Chinese/English and
   light/dark modes, including text and image billing.
6. Complete live browser and Codex CLI text/image validation, then report the
   exact source/image digests, addresses, reconciliation counts and rollback
   evidence. Only then release the same digests to API3. A final delta, write
   barrier, destructive route change or rollback-point removal still requires
   its own explicit maintenance approval.

## Worktree inventory at convergence start

- `/home/token-center-dev/worktrees/completion-p0-p1` — integration branch,
  `9eaf4af` before the documentation convergence commit.
- `/home/token-center-dev/worktrees/openai-b64-memory` — clean source branch at
  `37a50fe0d9a2709c036f02deea961bf2ebf0a9a3`.
- `/home/token-center-dev/worktrees/responses-parser-linear` — clean branch at
  `62cab854d7636f1b542797bd97169531ceedc9e3`; intentionally contains no parser
  fix yet.
- `/home/token-center-dev/worktrees/handoff-docs` — documentation convergence
  branch based on `62cab85`; its commit is cherry-picked onto the integration
  branch after validation.

Run `git worktree list --porcelain` and a status check in every worktree before
starting. Preserve unrelated user changes and do not delete worktrees as part of
handoff.

## Product-source map

- Product scope and rejected designs: [Product requirements](product-requirements.md)
- Architecture and trust boundaries: [Architecture](architecture.md)
- Public API: [OpenAPI](../openapi/openapi.yaml)
- JSON Schema configuration: [`schemas/`](../schemas/)
- Plugin ABI and packaging: [Plugin documentation](../plugins/README.md)
- MemeLoop Web entitlement contract: [MemeLoop Cloud integration](integrations/memeloop-cloud.md)
- Storage decision: [Archive storage and SlateDB decision](architecture/archive-storage.md)
- Migration/cutover: [CPA cutover runbook](operations/cutover-runbook.md)
- Trial/release gates: [Deployment readiness](deployment-readiness.md) and
  [Acceptance matrix](acceptance-matrix.md)
- Security: [Security audit](security-audit.md)
- Token Center recovery scope: [Backup and restore](operations/backup-and-restore.md)

If implementation or an older document contradicts the product requirements,
treat it as a defect or explicitly revise the requirement through review. Do not
silently resurrect rejected features such as model-prefix routing, a CPA bridge,
credential-to-provider coupling, application download throttling or a Forgejo/
Harbor release path.
