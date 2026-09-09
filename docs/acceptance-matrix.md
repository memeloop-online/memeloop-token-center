# Acceptance matrix

This matrix records the product acceptance surface. A check is complete only when
its current GitHub Actions evidence and the release record identify the exact commit
and immutable image digest.

| Area | Required acceptance |
| --- | --- |
| Authentication | Client and service credentials are isolated. Existing credentials are not rotated during migration. Authorized repeat copying returns only the matching encrypted original for the current generation, with no-store responses, scope checks and no secret logging; unavailable originals are explicit. |
| Tenant policy | Default-tenant actions are clear; tenant CRUD, scope boundaries and cross-tenant denial are verified. |
| Routing | Model grants, health gates, bounded round-robin failover and provider egress policy are verified. |
| Accounting | Reservation, settlement, price persistence, cache dimensions, balance, budget and idempotency are verified. |
| Text protocols | Chat, Responses HTTP, Responses WebSocket, embeddings and Messages preserve authorization, accounting and stream semantics. |
| Multimodal | Image, video and other generation jobs use durable lease, cancellation, asset archive and correct billing behavior. |
| History | Request filtering, keyset pagination, archive authorization, conversation evidence and session metadata remain bounded. |
| Operator UI | Overview, credentials, tenants, routes, pricing, requests, sessions and analytics are keyboard accessible and responsive. |
| Portal UI | Entered client credential is remembered until manual clearing; self-service data is correctly scoped and localized. |
| Observability | Liveness, readiness, metrics, low-cardinality labels and protected CPU/memory diagnostics are present. |
| Resilience | Database failure, archive degradation, provider errors, rollout interruption and recovery behavior are exercised. |
| Delivery | GitHub Actions builds two images, verifies their digests, validates chart/OpenAPI/schema contracts and releases from master. |
| Security | Input bounds, secret redaction, tenant isolation, plugin capability limits, network policy and administrative ingress protection are checked. |

## Evidence rules

- Record the GitHub Actions run URL and exact immutable digests in the GitOps release
  record, not in mutable prose.
- Browser acceptance covers desktop, tablet and phone widths; it includes keyboard
  navigation, Chinese and English localization, light/dark mode and tabular metric
  readability.
- Performance checks use representative production-shaped data and bounded query
  plans. Do not describe a fixture result as live traffic proof.
- Recovery acceptance restores an isolated database and archive copy, verifies a
  credential, history, accounting aggregate and asset read, then documents the
  recovery time and decision point.
- New provider types, transports, storage operations and UI sections add a row before
  release; they do not inherit acceptance solely from a similar existing feature.
