# MemeLoop Token Center product requirements

This document is the authoritative product-scope entry point. It records the
final requirements and rejected designs agreed for Token Center. Detailed API,
migration, security and operations procedures remain in the linked documents;
they must not silently redefine this scope.

## Product position

Token Center is an open-source, high-performance relay and usage analytics
service for AI text, image and video workloads. It is the API provider for
MemeLoop Web and is independently deployable; it is not an extension of CPA,
CPAMP, or a compatibility bridge that must keep another gateway running.

The service must support Codex, Claude Code, Copilot, Cursor, WorkBuddy and
other clients through public OpenAI-compatible, Responses and Anthropic
protocols. Client brands do not create separate authorization or accounting
systems.

The core product comprises:

- traffic admission, upstream routing and protocol adaptation;
- stable client credentials, authorization, quota, rate limiting and billing;
- unified upstream-provider accounts with API and OAuth connection methods;
- text, image and video generation with the same authorization and accounting;
- realtime request monitoring, request statistics and logical-session views;
- immutable request/response and generated-asset archives;
- a constrained plugin system for provider, OAuth, configuration and traffic
  policy extensions; and
- signed, idempotent subscription-entitlement synchronization from MemeLoop
  Web.

## Storage and runtime requirements

- PostgreSQL is the production system of record for identities, authorization,
  balances, accounting, request facts, pre-aggregates, archive references and
  conversation projections.
- S3-compatible object storage is the production store for request bodies,
  responses, images and videos. MinIO is a supported S3-compatible target.
- SQLite plus memory/filesystem or simplified S3-compatible fixtures remain
  supported for local and automated tests; they do not replace PostgreSQL/S3
  release evidence.
- Large time-range statistics must use hourly/daily pre-aggregates and only
  consult bounded boundary facts. They must not scan the complete raw request
  table.
- Cost facts and aggregates are partitioned by currency. Different currencies
  must never be summed into one amount.
- Bodies and generated assets are streamed with bounded buffers. The gateway
  must meet the documented memory acceptance gate rather than regress toward
  the former approximately 1 GiB CPA footprint.

The archive/relational split and the decision not to make SlateDB a source of
truth are described in [Archive storage and SlateDB decision](architecture/archive-storage.md).

## Stable credentials, quota and entitlements

A client credential has an immutable stable `key_id`; the printable secret is
only one credential generation. Rotation must invalidate the old generation
without changing the stable identity, principal, routes, policy, credit account,
quota, request history, statistics or conversation ownership.

Credential capabilities include:

- a user-facing alias;
- authorization to explicit model routes and/or route groups;
- an available balance and lifetime credit ledger;
- RPM, TPM and maximum-concurrency limits;
- daily, rolling-weekly and lifetime budgets; and
- suspend, reactivate, revoke and idempotent rotation operations.

A client credential may use `/v1/*` and read only its own `/self/v1/*` history,
statistics, generation and conversation data. It has no operator authority and
cannot select another credential identity.

MemeLoop Web uses a separate least-privilege service credential to apply signed,
versioned, idempotent entitlement snapshots for registration, renewal, upgrade,
downgrade, cancellation and reactivation. Subscription-cycle identity and
already-consumed credit must survive retries and plan changes. The precise
contract is [MemeLoop Cloud entitlement synchronization](integrations/memeloop-cloud.md).

## Unified providers, models and routing

An upstream provider account is one stable resource. API keys, native OAuth,
plugin-provided OAuth and no-credential private endpoints are connection methods
of that resource, not separate product concepts or separate management tabs.
Changing or refreshing the connection method advances its credential generation
without changing routes or historical attribution.

Built-in onboarding must include native OpenAI Codex, Anthropic Claude, GitHub
Copilot and Cursor authorization flows alongside direct API credentials. The UI
uses operational product language and must not expose CPA migration terminology
as a normal connection method.

The routing authorization chain is:

```text
client credential
  -> explicitly granted model routes and route groups
  -> public models declared by those routes
  -> explicit upstream accounts plus included/excluded provider groups
  -> eligible upstream candidates
```

The three group kinds have deliberately different semantics:

- **Provider group** organizes upstream accounts and participates in route
  candidate inclusion or exclusion. Exclusion wins.
- **Route group** organizes model routes and participates in credential route
  authorization. A route may belong to multiple route groups.
- **Credential group** is presentation-only grouping for UI filtering and bulk
  viewing. It must never grant or remove model access.

Route creation supports exact upstream accounts, included/excluded provider
groups, direct credential grants and multiple route groups. A missing route
group may be created from the route editor through a search/create combobox.
Credential route and route-group pickers may search existing values but may not
create authorization objects implicitly.

The upstream-model field is backed by a synchronized model catalog and provides
search/autocomplete. A reviewed custom model escape hatch may exist, but an
empty catalog or empty route grant must fail closed. Model-price management
supports one-click synchronization from reviewed sources, visible source and
freshness metadata, deterministic conflict handling and explicit manual
overrides.

## Multimodal generation and billing

OpenAI Images, Codex Responses image generation, Volcengine Seedance video and
ComfyUI image/video workflows are first-class routes. The same credential,
route, provider-group, quota, rate-limit, archive and tenant boundaries apply to
text and generation requests.

Pricing must support token dimensions (including cached input and cache write)
and generation units such as image, second, job and megapixel. Admission reserves
the maximum authorized amount; completion settles actual usage; cancellation or
failure releases the unused reservation exactly once. Statistics preserve the
route, upstream account, model, modality, billing unit, currency and immutable
price snapshot used for the charge.

## Requests, statistics, archives and logical sessions

Request monitoring records started and finished events and exposes a resumable,
cursor-backed realtime stream. Request detail and error investigation load
archived bodies only on demand and always enforce tenant or stable-key ownership.

Operator request statistics are independent from the realtime request table and
must provide these views:

1. overview;
2. trend;
3. model;
4. client credential;
5. upstream provider account; and
6. weekday/hour heatmap.

Logical-session analysis is an additional first-class view. It provides realtime
session refresh, bounded session aggregates and keyset-paginated request/edge
detail. Session identity prefers explicit session, turn, parent, branch,
compaction and subagent metadata. Exact relations are stored only after their
tenant, principal and stable-key parent evidence is verified. Merkle-prefix and
semantic evidence may infer bounded continuation/retry/edit/branch candidates;
low-confidence candidates remain visible without being guessed into a reliable
cluster. Compression remains explicitly related to the conversation it replaces.

Downstream AI applications may additionally declare a session name, W3C
trace/span context, agent and parent-agent identities, task kind, and bounded
non-secret string metadata. These declarations are stored on the same
conversation observations and exposed for request timelines, agent flame views,
task-type distributions and per-task cost analysis. They are diagnostic only:
they cannot grant access, change routing or billing identity, or turn candidate
evidence into a confirmed relationship. When Codex or another client does not
report them, the service uses only its existing protocol and Merkle-prefix
evidence and must not fabricate a name or classification from prompt contents.
Session detail must expose that structural evidence separately from declared
semantics, including its provenance and relationship confidence. Visualizations
must show wall-clock timing, relationship/agent depth, task request share and
currency-separated agent/task cost; they must label elapsed-request flame views
as such rather than implying CPU samples. Codex should be integrated through
native session/Responses parent evidence plus environment-backed custom-provider
headers. OTLP tool/turn telemetry is a later non-billing event stream and may be
joined only through stable opaque identifiers, never prompt-content similarity.

Historical archive rows that cannot be matched uniquely must be marked
`unlinked`. They must not be attached to an arbitrary nearby request and must
not become duplicate billable requests.

## Plugins and configuration

Core, credential, route, provider and generation configuration is JSON validated
by server-side JSON Schema and rendered by a safe schema-driven UI. Configuration
must stay intentionally smaller than CPA and avoid a large collection of rarely
used switches.

Plugins run as constrained Wasmtime components with explicit versions and
capabilities. They may contribute:

- provider definitions and configuration schemas;
- OAuth adapters;
- bounded provider request/response adapters;
- traffic deny/rewrite policy; and
- scoped declarative configuration.

Plugins may not bypass core authentication, model-route authorization, pricing,
quota, tenant isolation, SSRF policy, archive ownership or resource bounds.
Credential material is injected by the core after destination validation and is
not exposed to provider components. New streaming or generation adapter ABIs
require an explicit bounded version; they must not be presented as supported by
the existing buffered-only ABI.

## UI, API and localization

- Operator and self-service interfaces support Chinese and English, light and
  dark themes, keyboard use and mobile widths.
- Chinese large numbers use `万`, `亿` and `万亿` conventions; English uses full
  three-digit grouping rather than `K`/`M` abbreviations. Exact values remain
  available to the user.
- Product copy is mature user-facing language, not implementation notes or task
  reports. Terms such as “create key” are presented as “create credential”.
- The application ships appropriately sized website icons.
- HTTP APIs have a versioned OpenAPI contract, bounded pagination, explicit
  idempotency behavior and generic secret-safe error envelopes.

## Deployment, security and quality gates

The default production deployment is Kubernetes through the maintained Helm
chart, with separate gateway, control and worker roles. PostgreSQL and S3 are
external production dependencies. The target topology exposes one
cluster-internal address and two separately controlled external addresses.

**2026-08-23 release-order override:** API3 is the production target. One exact
source SHA and its immutable service, importer and plugin-installer digests must
first be deployed to the CPA/API2 trial slot for the user and release agent to
exercise. The previous CPA revision, data backup and routing configuration must
remain an immediately usable rollback point. Only the exact same digests may be
promoted to API3, and only after the CPA/API2 trial evidence below is complete.
This explicitly supersedes the earlier API3-first trial sequence.

The public service must be checked for injection, privilege escalation, IDOR,
tenant isolation, SSRF/DNS rebinding, credential or body leakage in logs,
malicious plugins and resource exhaustion. Application-level AI quota and route
policy remain in Token Center; generic download bandwidth/rate limiting belongs
at Higress/Ingress.

Release acceptance requires, for one exact source SHA:

- Rust formatting, Clippy with warnings denied, all-target/all-feature tests and
  Cucumber-rs;
- TypeScript checks, build and Cucumber.js browser tests with Playwright only as
  the browser driver;
- SQLite, fresh PostgreSQL, mock upstream and MinIO/S3 coverage;
- migration replay, archive replay, permission isolation and security gates;
- imported-scale query plans and bounded-memory/load evidence;
- immutable GitHub Container Registry image digests; and
- deployed browser dogfood covering existing CPA accounts, subscriptions and
  history; unified upstreams and OAuth; provider, route and credential groups;
  credentials and price synchronization; all usage/session views; Chinese,
  English, light and dark presentation; archives; and multimodal accounting;
- Codex CLI requests routed through the CPA/API2 trial endpoint for both text
  and image generation; and
- promotion of the same immutable digests to API3 only after every trial check
  is green.

See [Deployment readiness](deployment-readiness.md),
[Production deployment](operations/production-deployment.md),
[Security audit](security-audit.md) and the current
[Acceptance matrix](acceptance-matrix.md).

## CPA/CPAMP migration and cutover

Migration must support a full baseline followed by repeatable, idempotent
increments while CPA continues serving. It preserves the current legacy client
credential, stable identity, policy, balances and history. CPAMP usage, aliases
and prices are reconciled separately from session-archive bodies; a nonzero usage
checkpoint never proves body migration.

The CPAMP failure flag is authoritative during normalization. Failed rows with
a real 4xx/5xx code preserve it; failed rows carrying zero, missing, 1xx, 2xx,
3xx or invalid codes become sanitized `502`/`upstream_error` failures. Such
rows must never inflate successful-request statistics.

Each import records source identity, digest and checkpoint, replays without
duplicate facts or aggregates, and uses a reviewed overlap window for late
writes. Session archives are dry-run, apply and exact-replay checked. Exact and
unlinked counts, object digests, source totals and target totals must reconcile.

Legacy CPA upstream migration must preserve its private SOCKS5 account proxy
rather than silently bypassing it. Proxy credentials/topology are encrypted
write-only material; dry-run output is count-only, and proxy creation or change
is restricted to a global service credential. The local-DNS SOCKS5 form is
required so the validated target address remains pinned through the handshake.
Legacy CPA archive timestamps with
explicit offsets and up to nanosecond precision may be normalized to canonical
six-digit UTC only in the pre-stable legacy projection. The stable snapshot
protocol remains strict and must reject non-canonical source timestamps.

The CPA/API2 trial deployment must preserve the old CPA revision and a verified
route-back operation. Trial-scoped writes must be identifiable and reversible;
existing accounts, subscriptions, credentials and history must be validated in
place without destructive resets. A complete migration, final write barrier or
irreversible traffic shift remains forbidden outside a user-declared
maintenance window. The 2026-08-23 instruction explicitly records that the
production window is closed: passing trial evidence prepares the same digests
for the API3 production target, but does not authorize API3 mutation until the
user separately declares the next window open. It also does not authorize data
destruction or removal of the CPA rollback point. Detailed steps and rollback
are in [CPA to Token Center cutover](operations/cutover-runbook.md).

## Repository and operational ownership

- The canonical private repository is
  `github.com/memeloop-online/memeloop-token-center`.
- Development uses `master`, GitHub Actions and GHCR. Releases are immutable
  digest references produced from the exact accepted `master` commit.
- Repository automation and operational helper scripts use TypeScript on Node.js.
  Python scripts, Python-only test harnesses and Python runtime dependencies are
  not part of the supported development, migration or release toolchain.
- There is no maintained Forgejo mirror, Forgejo Actions workflow or Harbor
  release path for this product.
- Rust development and builds must not consume the Westlake physical root disk.
  Source and caches belong in the Longhorn-backed Coder workspace; release
  compilation belongs in GitHub Actions.
- Kubernetes, GitOps, migration execution, storage cleanup, rollout and cluster
  validation are coordinated with the infrastructure task
  `codex://threads/01a00a1e-3a18-7b82-8a36-a663c0ab6adc`. The CPA/API2 trial
  rollout is authorized only with a recorded old-CPA rollback point; API3 must
  remain unchanged until the release agent records complete browser and Codex
  CLI evidence and explicitly releases the same digests.

## Explicitly rejected or removed designs

The following are not requirements and must not be reintroduced:

- CPA Subscription Bridge or any runtime dependency on CPA;
- model-name prefix routing, including client-specific prefixes such as names
  ending or beginning with a user label;
- coupling a client credential directly to provider labels or provider groups;
- application-level archive/download throttling already owned by Higress/Ingress;
- a Forgejo repository, Forgejo Actions workflow or Harbor-specific release
  pipeline;
- presenting SQLite, filesystem storage or in-memory storage as production
  defaults;
- guessing archive/session associations when evidence is ambiguous; and
- an irreversible migration or traffic cutover during the CPA/API2 trial.
