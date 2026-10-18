# Session loading investigation — 2026-10-18

Session: `01a068f3-ed93-7522-a63c-95534355ee8f`.

## Production evidence

- The session exists; its credential-scoped projection reports 30,013 requests.
- The original first-page detail SQL was executed in a read-only transaction
  with a 12-second statement timeout. It timed out.
- Its execution plan joins the full matching request history to
  `request_archive_spools` and `response_archive_spools`, including parallel
  sequential scans of both spool tables, before sorting and applying the page
  limit.
- Applying the ordered limit inside **both** live and imported-archive sources
  before these joins took 667.903 ms execution time (217.213 ms planning time)
  on the same production session. This is one measurement, not a latency SLO.
- The frontend has a 15-second total detail-read deadline. This explains a
  reproducible database path capable of exhausting that deadline; it is not a
  captured distributed trace of the user's original failed HTTP request.

All production database operations in this investigation were read-only and
statement-timeout bounded. No production data was removed or changed.

## Local Kubernetes access

The Windows machine `dongwu-gaming-windows` uses API server `100.64.0.6:6443`.
The local configuration previously used `100.64.0.7:6443`, which refused
connections. The `.6` server could list resources from this environment but
returned 502 errors when forwarding logs/exec to the production node.

The `.9:6443` API server successfully served resource listing and production
logs/exec. The local cluster endpoint now uses `.9`, retaining the original
certificate credentials and context. No Windows configuration was modified.

## Changes in this worktree

- Both session-detail SQL branches page each ordered source before spool
  joins. Older-page cursors are applied within both sources as well as after
  merging, preserving equal-timestamp ID ordering.
- Session retry targets the failed list/detail operation rather than always
  reloading the list. Native browser cancellation/timeout wording is replaced
  by localized load-failure copy.
- User messages wrap in full on the right. AI responses appear on the left,
  preferring retained `final` / `final_answer` markers and otherwise showing
  the last paragraph of the last unmarked response message.
- Full conversation details, tool lists, individual tools, arguments and
  results have separate disclosure levels. Collapsed expensive children are
  not mounted.
- Fluent theme tokens now override the legacy light-theme replay palette.
- SQL-shape, final-response projection, disclosure and browser assertions were
  added/updated. Automated validation has **not** run locally, in accordance
  with `web/AGENTS.md`; CI results are still required.

## Storage findings and remaining work

### Resumed work

- Kubernetes access through `.9:6443` was rechecked after the conversation
  service restart; all five application/database roles were running.
- Added a bounded prefix-context API with 32-node pages and separately signed,
  tenant/key/session/request-scoped content continuations. Tool bodies are
  excluded from overview reads. One production read-only measurement of this
  page query took 9.593 ms execution and 34.384 ms planning.
- Context results explicitly identify their semantic representation: canonical
  atoms are not a claim of byte-for-byte original archive fidelity. Tests were
  added for >1,024 messages, repeated messages, resets, Unicode chunks,
  ownership boundaries and retired credentials; they still require CI.
- Added migration 131 and a credential-scoped title queue. It stores one title
  plus job state, no copied conversation. Initial unnamed observations enqueue
  idempotently; explicit regeneration preserves the prior title. Leases fence
  stale completions, three attempts bound execution retries (including worker
  crashes), and missing model configuration waits without consuming retries.
- Added title GET/POST endpoints and a separate worker lane. Execution reuses
  system assistant route/billing validation and normal proxy accounting.
  Input samples two existing user atoms, at most 1,024 Unicode characters each;
  the output must be a final, single-line title of at most 80 characters.
  Internal title requests do not recursively enqueue more title jobs.
- Added an operator title action with localized billing/configuration/permission
  copy and scope-isolated polling. Queue fencing/retry and model output tests
  were added. These have not run in CI or been deployed.
- Added the frontend prefix-reader transport with strict session/request/page
  identity validation, contiguous depth checks, and Unicode-scalar content
  offsets. Fetching a page never implicitly follows its continuation or reads
  tool content. Repeated text at distinct node positions is preserved.
- Added a content disclosure that mounts its HTTP reader only when opened,
  reads at most one 16,384-character chunk per action, aborts on unmount, and
  validates node/offset/total-length continuity before publishing results.
- Wired the prefix reader into operator and self-service session surfaces.
  Default linked-session reads now load one context page; full archive readers
  are mounted only after the original-archives disclosure opens. Context
  history pages and per-observation selection retain access to pre-reset paths.
  Added a browser fixture asserting zero archive/tool reads at initial render
  and through intermediate disclosure levels, then one tool-content read at
  the innermost expansion. This test still requires CI.
- Added separately signed text-only continuations for user/assistant messages.
  Both database backends extract only declared text blocks from the existing
  atom and return at most 16,384 Unicode scalars, excluding embedded tool
  results and media from the HTTP response. No duplicate text column/table is
  created. JSON extraction still examines the selected atom in the database;
  it is not an assertion of zero parsing cost for large mixed-content atoms.
- Long user messages now continue these text-only reads automatically and
  display the full accumulated text without a disclosure. Content reads share
  a four-request concurrency bound and abort on view disposal. Representation,
  node identity, offsets and total length are checked across all chunks.
  Added long-Unicode/mixed-tool/media extraction and representation-switch
  regressions; CI and browser verification remain pending.

The code stores shared content-addressed semantic atoms and prefix nodes.
The 1,024-atom inference fingerprint is separate from the full leaf path.
A sampled production observation from this session has a leaf and an
object-shaped fingerprint, consistent with the compact prefix representation.
The semantic-payload table contained five transient entries at the time of
sampling; the code deletes successful projection payloads transactionally.
Neither this sample nor the code inspection proves that every historical
archive is available or that no orphaned row exists.

The following requested work is **not complete**:

- The new default prefix view does not yet satisfy the entire reading contract:
  the selected request's own final AI response is only in the explicit
  archive view unless it appears in a later request's retained context.
  This needs a final-response projection;
  selecting one observation is not proof of complete session-history coverage.
  Unlinked sessions retain the prior archive reader.
- AI title generation: backend queue, generation and operator action now exist
  locally. HTTP authorization/browser contracts, generated-title search,
  lifecycle cleanup, and compatibility verification remain. Normal proxy
  audit records are still created for model execution; no separate prompt
  copy is stored in the title table.
- Full historical verification across all retained pages, protocol formats,
  resets, compactions, missing archives and branch boundaries.
- CI validation and deployment. These local changes have not changed the
  production application.

## CI handoff

Draft PR: https://github.com/memeloop-online/memeloop-token-center/pull/657

The task-only changes were committed using a separate Git index, excluding
unrelated dirty proxy-lifecycle edits. Review/CI work now continues in the
isolated worktree `/home/user/workspace/mtc-session-review`, branch
`fix/session-reading-20261018`. The original workspace remains untouched.
The first PR revision conflicted with newer master migrations 128–130; the
review branch merges master and assigns the title migration version 131.
This supersedes the earlier migration number in the original worktree.

CI run `39158604560` is live for `796ea30f`. Migration smoke, dependency
security, repository security and docs passed. Initial failures:

- Web typecheck: missing browser-test Window declarations for context counters;
  fixed locally in review worktree.
- Packaging: Helm schemaVersion still 130; adjusted to 131 in review worktree.
- API contract: new context/title routes need canonical OpenAPI entries and
  regenerated runtime discovery. This remains pending.
- Rust and memory compilation were still running at the last check.

That run subsequently finished: memory compilation/acceptance passed; Rust
stopped on three Clippy diagnostics (two manual range checks and test-module
ordering), now corrected. OpenAPI paths, schemas and generated discovery were
added; contract assertions now cover bounded ranges, text representation and
the title endpoint's additional execution authority. CI runs on the updated
branch; no test-suite success or deployment is implied by compilation alone.

The final-response projection has not been implemented. Do not infer that the
PR or partial green checks establish completion of the reading requirements.

## Latest integration evidence

- The temporary filesystem filled during a new parser-file write. The review
  worktree was copied to persistent storage and Git's worktree pointer repaired;
  task work now continues at `/home/user/workspace/mtc-session-review`. The
  incomplete parser file was repaired before committing. The old `/tmp` copy
  is not the active worktree.
- CI `39166590869` completed. API contracts, migration smoke and security passed.
  Rust passed compilation/Clippy and reached tests: failures were the historical
  migration fixture's fixed version list and missing title-table downgrade.
  These are updated for migration 131.
- Browser failures were traced to fixture routing counting new context/title
  reads as detail refreshes, and disclosure-test nth locators shifting as
  opened items left the selector. Fixtures/selectors are corrected without
  weakening refresh or lazy-read assertions.
- Helm values.schema.json also pins the database schema version; updated it
  to 131 alongside values.yaml.
- Added a final-answer parser and protocol tests for buffered Responses, Chat,
  Anthropic, plus corresponding SSE forms. It preserves explicit final text,
  otherwise takes the last paragraph, ignores tool fields, and rejects missing
  terminal/incomplete responses. It is test-only until the background
  projection/cache is connected; the selected-response feature remains pending.
