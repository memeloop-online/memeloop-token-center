# Development handoff

This is the resume point for the next Token Center development agent. Read
[Product requirements](product-requirements.md) first, then
[Architecture](architecture.md). Those documents preserve the agreed product
scope and rejected designs; this document preserves the implementation state and
remaining acceptance gates, updated on 2026-08-24.

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
  on Node.js. Remove Python scripts, Python-only test harnesses and their runtime
  dependencies while preserving their CLI contracts and security properties.
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
deployment. The next candidate changes both Debian-based Dockerfiles to the
verified HTTPS mirror for build-only stages and adds a packaging regression
contract that rejects an HTTP default. Their final runtime stages use the
supported distroless Debian 13 C runtime: the locally built release binaries
depend only on glibc/libgcc, and the checksum-verified Cosign 3.1.3 asset is
statically linked. This removes the package manager and unneeded Debian utility
packages from both final images; the real Docker contracts and final-image
Trivy scans remain mandatory CI gates.
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

1. Validate the pinned Alpine importer runtime contracts and TypeScript operator
   suite locally. Do not change security or memory thresholds.
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
