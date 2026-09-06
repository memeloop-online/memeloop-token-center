# Memeloop Token Center product requirements

## Purpose

Memeloop Token Center is a standalone gateway for AI-provider access, credential
management, routing, usage accounting, request history, generated assets and
operational visibility. It owns a native data model and operations.

The gateway supports compatible chat, Responses, embeddings, images, asynchronous
generation and video generation where configured providers support them. Each
request has stable tenant, credential, route, account and price-snapshot attribution.

## Principles

- Never silently bypass authorization, quota, price or archive policy.
- Preserve request and accounting history after configuration changes.
- Make destructive changes explicit, idempotent and recoverable.
- Store binary assets once in object storage and reference them from durable records.
- Use bounded, keyset-paginated reads for large history.
- Operate from immutable image digests and GitOps-managed configuration.

## Tenants and credentials

The normal deployment has one default tenant. Operator actions made while the
all-tenants view is selected act on that default tenant and say so in the
interface; controls must not be disabled without a useful explanation.

Operators can create, rename, pause and retire tenants for segregated workloads.
Creating a tenant does not create a client credential. A client credential belongs
to exactly one tenant and carries model grants, rate/concurrency limits, balance,
budget and audit history. A service credential is an operator or automation
credential with explicit administrative scopes.

Credential values are shown only at issuance or rotation. The portal remembers an
entered credential locally until the user deliberately clears it. Self-service views
are bound to that credential.

## Routing and provider accounts

Provider accounts contain encrypted provider credentials and validated non-secret
configuration. Model routes select eligible accounts by tenant and model, honor
priority and health, and use bounded round-robin selection. A temporary account
outage opens its health gate and selects another eligible account rather than
disabling the account permanently.

Every route and credential edit is optimistic-concurrency protected and idempotent.
Routes retain historical attribution after being disabled. Operators can inspect
route status, health, recent outcomes and permitted models without provider secrets.
Provider egress uses the account's explicit proxy or allowed destination. Network
policy and SSRF controls are mandatory.

## Accounting, archives and conversations

Admission reserves credential rate/concurrency, tenant budget, balance and token or
media price bounds before outbound execution. Settlement records actual usage and
refunds unused reservation. Requests with no safe price bound are rejected when
balance or budget policy requires a bound.

Price synchronization is an explicit operator action. Synchronized prices, manual
overrides, source metadata and timestamps are durable database records, so a pod
restart cannot erase them. Cache-read, cache-write, input, output, image, video and
job pricing use the model price definition; missing cache dimensions conservatively
use input pricing and are marked as estimates.

Each admitted request records status, duration, model, protocol, route, provider
account, usage, price settlement, error class and stable request ID. Response and
generated assets are written to durable object storage and linked to the request.
Asset reads apply credential or tenant authorization and safe range handling.
Object-store degradation is observable and does not make unrelated text traffic
unavailable.

Conversation views use explicit client identifiers and cautiously inferred prefix
relationships. Optional session name, session ID, trace/span identifiers, parent
relationships, agent relationships, task kind and bounded labels support timelines,
hierarchy views, cost slices and future flame or pie visualizations without
cross-credential leaks. Evidence is shown with confidence and absence is never
invented.

Responses over HTTP and native WebSocket share authentication, routing, accounting,
archive and cancellation rules. Socket support is a first-class transport.

## Operator and portal experience

The operator UI is a responsive product interface. It uses action-oriented labels,
avoids unexplained warnings, and keeps forms collapsed until invoked. It provides an
overview with readable tabular-number metrics, balance, request totals, success rate,
latency and recent outcomes; focused credential, tenant, provider-account, route and
model-permission management; request search and archive detail; usage charts for
spend, requests, tokens, latency, errors, models, credentials, accounts and task
labels; separate multimodal generation; and pricing, health and diagnostics.

The UI supports keyboard navigation, light/dark themes, localized text and
locale-aware number/date formatting without hiding precision. It is usable at phone,
tablet and wide desktop widths. Operator and portal use the same vocabulary, visual
system and credential behavior.

## Availability and data protection

The service exposes /livez, /readyz and protected Prometheus metrics. Liveness
covers process health; readiness uses bounded dependency checks. Database failure
makes a role unready. Archive failure is surfaced as a degraded dependency and fails
asset operations closed without restarting otherwise healthy traffic.

Metrics include request volume, latency, errors, active requests and streams,
upstream activity, queue depth, database pool state, RSS, allocator state, buffer
and archive capacity. Labels are bounded. Protected runtime diagnostics can produce
CPU and memory profiles on the control role only; they are never publicly exposed.

Production uses highly available PostgreSQL and S3-compatible object storage with
encrypted transport, backups, restore testing and least-privilege credentials.
Recovery is documented in [operations/disaster-recovery.md](operations/disaster-recovery.md).

## Delivery constraints

- Application code is Rust; automation scripts are TypeScript. Python scripts are not permitted.
- Builds, verification and image publication run in GitHub Actions.
- A release produces only the service and plugin-installer images.
- GitOps deploys immutable image digests from master; tags are not release selectors.
- No NodePort, hostPort or unprotected administrative listener is introduced.
- Secrets never appear in source, fixtures, workflow output, client responses or documentation.

## Non-goals

The product does not contain one-off data conversion tooling, source-specific
credential transfer procedures, temporary acceptance environments or a second
intermediary service. Those artifacts are outside this repository and not part of a
normal release.
