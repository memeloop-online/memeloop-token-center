# MemeLoop Token Center architecture

This document is the architecture entry point for the product requirements in
[Product requirements](product-requirements.md). It defines ownership and trust
boundaries; detailed schemas, APIs and runbooks remain authoritative for their
own mechanics.

## System boundary

```text
AI clients / MemeLoop Web
          |
          v
  gateway and control APIs
          |
          +--> authorization, quota, routing and accounting --> PostgreSQL
          |
          +--> provider account / OAuth / plugin adapter --> AI upstream
          |
          +--> immutable bodies and generated assets -------> S3 / MinIO
          |
          `--> request events and session projections ------> operator / portal
```

Token Center owns client authentication, tenant isolation, routing decisions,
quota, billing facts and observable history. It does not delegate these to an
ingress, provider plugin or legacy CPA process. Higress/Ingress owns generic edge
bandwidth and connection controls; Kubernetes and backing-service operations are
external operational responsibilities.

## Runtime roles

One release image exposes three production roles:

- **gateway** accepts public model and self-service traffic, performs admission,
  routing, streaming and synchronous accounting;
- **control** exposes operator, catalog, entitlement, statistics and management
  APIs; and
- **worker** claims asynchronous generation and bounded maintenance work.

The Helm chart deploys these roles separately. `all` exists only for local or
temporary test use. Gateway and control route sets are intentionally disjoint so
an ingress mistake cannot turn a gateway into an operator endpoint.

See [Production deployment](operations/production-deployment.md) for probes,
network policy, migration ownership and rollout requirements.

## Data ownership

PostgreSQL owns all mutable truth that requires transactions, joins or tenant
constraints:

- tenants, principals, stable credentials and credential generations;
- upstream provider accounts and encrypted connection generations;
- model routes, provider groups, route groups and routing grants;
- balances, reservations, ledger entries, prices and entitlement versions;
- request/generation facts, event streams and hourly/daily aggregates;
- archive references and import provenance; and
- logical-session observations, reliable edges, candidates and projections.

S3-compatible storage owns immutable large values: request/response bodies,
generation results and assets. PostgreSQL stores typed references and content
digests. A database row without an object is reported as an archive gap; the
service never fabricates content. Filesystem and memory stores are test
implementations.

SQLite mirrors product semantics for bounded local tests but does not model
PostgreSQL partition pruning, locks, concurrency or operational scale. The
storage split, archive staging fences and SlateDB decision are documented in
[Archive storage and SlateDB decision](architecture/archive-storage.md).

Backups cover only these Token Center data planes and their recovery material;
see [Backup and restore](operations/backup-and-restore.md).

## Identity and authorization model

Client secrets and service tokens are generations beneath stable UUID
identities. Every policy, ledger, request, statistic and conversation reference
uses the stable identity. Rotation changes authentication material, not data
ownership.

Operator service tokens and client credentials have separate authenticators and
route trees. Service-token scopes are least privilege and may be tenant-bound.
Client credentials can access only authorized public routes plus their own
self-service projection.

Model authorization is relational rather than embedded in a model-name hack or
legacy `allowed_models` list:

```text
stable key
  -> direct route grants + route-group grants
  -> enabled public-model routes
  -> explicit accounts + included provider groups - excluded provider groups
  -> active, catalog-compatible provider candidates
```

Credential groups have no edge to the grant graph. The relational constraints
are tenant-qualified so valid foreign IDs from different tenants cannot be
combined.

## Unified upstream-provider model

An upstream account has a stable ID, provider driver, configuration and one
current encrypted credential generation. `api_key`, `oauth` and `none` are
connection methods, not separate account types. Reauthorization or method
replacement uses generation compare-and-swap and keeps the account ID, routes
and request attribution stable.

Native and plugin OAuth flows persist bounded login state in PostgreSQL so any
control replica can continue them. Provider endpoints are server/catalog owned;
user-supplied destinations pass the same URL, DNS, IP and network-scope policy as
normal upstream traffic.

The model catalog is synchronized per account and credential generation. Route
creation resolves public/upstream model compatibility against that catalog;
catalog changes cannot silently widen a client grant.

CPA is only a migration source. Importable direct accounts and reviewed managed
OAuth documents converge into the unified account model. Opaque Copilot/Cursor
handles cannot become credentials and are reported for native reauthorization.
Historical retired connection rows may remain readable for attribution, but are
never routable or refreshable.

Imported direct targets default to the public destination policy. Migration may
select `network_scope: private` only through a separate versioned owner-only
policy listing exact normalized base URLs. This decision is independent of
whether the account uses a proxy; it remains subject to the server's global
authority check and independent DNS/IP validation. Stable source identity does
not include scope, so changing an approved scope conflicts with the existing
account instead of creating a duplicate.

An imported API-key account may carry one operator-approved private SOCKS5
proxy. The proxy URL—including optional proxy authentication and private
topology—is sealed inside the credential envelope, never copied to account
configuration or response views. The control plane independently resolves and
classifies the final target and proxy endpoint, disables environment proxy
inheritance, pins both resolutions for the operation and requires a global
service authority to create or change it. Only local-DNS `socks5` is accepted,
so the SOCKS handshake receives the already pinned target address; `socks5h`,
HTTP(S) proxies and public SOCKS endpoints fail closed. A tenant-scoped key-only
rotation may preserve an already-approved proxy but cannot replace or remove its
transport boundary.

## Request lifecycle and accounting

Before contacting an upstream, the gateway:

1. authenticates the stable client credential;
2. validates and, where allowed, applies a constrained traffic rewrite;
3. rechecks the rewritten model against route grants and provider candidates;
4. resolves an immutable price snapshot;
5. atomically enforces rate, concurrency, budget and balance limits; and
6. creates one pending request/generation record and one reservation.

The final response or worker completion settles actual usage once, releases the
remainder and writes the final attribution fact. Candidate failover is bounded
and may occur only before downstream output under the explicitly tested safe
conditions. A partial stream never returns to routing.

Asynchronous generation separates admission from provider polling. Worker claims
are fenced; cancellation and retry cannot settle or refund twice. Large assets
are streamed into object storage rather than materialized in gateway memory.
Streaming proxy responses preserve the 16-request/upstream lifecycle budget,
while at most four gateway responses may simultaneously own a 5 MiB multipart
archive buffer. Additional streams apply bounded backpressure until a buffer is
released; this prevents S3 multipart memory from multiplying by every admitted
lifecycle without reducing upstream routing concurrency.

## Observability and logical conversations

Request records and append-only started/finished events are committed with the
request lifecycle. Realtime monitoring resumes through PostgreSQL cursors rather
than process-local fanout. Request lists and statistics use bounded keyset
pagination, facts and rollups.

Session construction has two evidence levels:

- explicit session, turn, verified parent, branch, compaction, subagent and
  upstream-response identifiers can form reliable relations; and
- bounded semantic/Merkle-prefix similarity can form candidate evidence.

Schema v55 keeps explicitly declared session name, W3C trace/span context,
agent ancestry, task kind and bounded non-secret metadata on the observation.
These fields are a visualization/audit projection, not identity evidence. The
session detail additionally projects explicit session/turn/parent/response,
branch, compaction and client-name protocol evidence as a separate `structure`
object. The request timeline, duration flame view, task distribution and
currency-separated agent/task cost views can therefore include older Codex
traffic while visually distinguishing inferred structure from declared human
semantics. Missing names and types remain missing rather than being generated
from prompts. See
[Semantic execution metadata](semantic-execution-metadata.md).

Candidate evidence is visible for operator review but cannot silently merge
identities. Conversation queries remain scoped by tenant, principal and stable
key. Imported archive records with stable ownership but ambiguous request
correlation become explicit unlinked observations outside normal billing facts.

Detailed import semantics are in [Session archive import](session-archive-import.md).

## Plugin boundary

Plugins are versioned Wasmtime components, not dynamically loaded native
libraries. A manifest declares provider, OAuth, schema, traffic and host
capabilities. The host supplies bounded fuel, memory, deadlines and an exact
HTTP-origin allowlist.

Core code retains credential injection, destination validation, authorization,
pricing, quota, accounting, archive ownership and error redaction. Provider
components see only the bounded request/response contract required by their ABI,
not upstream credential material. Unsupported streaming or generation behavior
fails closed until a new explicit ABI version defines it.

Packaging and runtime constraints are documented in `plugins/README.md` and the
WIT contract in `wit/token-center.wit`.

## Configuration and API boundary

JSON Schema is authoritative at the server write boundary and also drives the
operator forms. Browser validation improves usability but never replaces server
validation. Plugin schemas are restricted to the supported declarative subset;
remote references and executable schema features fail closed.

The OpenAPI contract in `openapi/openapi.yaml` documents versioned public,
self-service and internal endpoints. Route registration and OpenAPI path sets are
checked together. Pagination, idempotency, tenant selection and error behavior
must remain explicit; see [API contract](api-contract.md).

## Deployment and release boundary

Source is canonical only on the private GitHub organization repository and the
development branch is `master`. GitHub Actions builds and tests the exact source
revision, publishes immutable GHCR digests and retains release evidence. A
deployed chart references those digests; it does not rebuild source in-cluster.

The migration Job is the only production schema migrator. Application roles do
not migrate on startup. Non-backward-compatible write barriers require all old
writers to drain before the new schema and binary start.

The 2026-08-23 release-order override makes API3 the production target. The
candidate's exact SHA and three immutable digests first run in the CPA/API2
trial slot while the old CPA revision, backup and routing configuration remain
ready for immediate rollback. The trial must pass cluster smoke, the full live
browser matrix and real Codex CLI text/image requests. Only those same digests
may then be promoted to API3, and only after the user explicitly declares a
production window open. The 2026-08-23 state is outside that window, so API3 is
an immutable boundary even if the reversible CPA/API2 trial passes. Formal
migration barriers and destructive traffic movement still follow
[CPA to Token Center cutover](operations/cutover-runbook.md) and require their
own explicit maintenance approval.

All cluster, GitOps, migration execution, storage and rollout work belongs to the
designated infrastructure task. Product work supplies immutable inputs and
acceptance criteria but does not mutate the cluster directly.

## Security invariants

- Authenticate before buffering or parsing protected request bodies.
- Bind every resource lookup, list filter and relation to tenant and/or stable
  credential ownership.
- Bind SQL values; generated SQL accepts only closed internal enums/literals.
- Validate destinations after DNS resolution and again on connection; block
  metadata, loopback and unapproved private networks.
- Never return, log or metric-label client secrets, OAuth tokens, provider
  credentials, signed asset URLs or archived bodies.
- Bound request bodies, archive reads, plugin execution, event streams, list
  pages, database pools and generation concurrency.
- Keep public gateway and operator ingress origins and credentials separate.

Current evidence and outstanding gates are tracked in
[Development handoff](development-handoff.md),
[Acceptance matrix](acceptance-matrix.md) and
[Security audit](security-audit.md).
