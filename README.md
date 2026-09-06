# Memeloop Token Center

Memeloop Token Center is a high-performance gateway for AI text, image and
video requests with credential management, usage accounting, archives and
operational analytics. Production uses PostgreSQL and S3-compatible storage.

Clients use open protocols rather than product-specific forks: Responses-capable
clients use `/v1/responses`, Anthropic clients use `/v1/messages`, and OpenAI
compatible clients use `/v1/*`. The service records request usage and can use
explicit client session metadata plus bounded prefix evidence to organize
logical conversations.

The same image supports `serve --role gateway|control|worker|all`. Production
Helm configuration separates gateway, control and worker. Gateway does not
register administrative routes; control does not register model or self-service
routes.

## Documentation

- [Product requirements](docs/product-requirements.md)
- [Architecture](docs/architecture.md)
- [Development handoff](docs/development-handoff.md)
- [HTTP API contract](openapi/openapi.yaml)
- [Deployment readiness](docs/deployment-readiness.md)
- [Acceptance matrix](docs/acceptance-matrix.md)
- [Semantic execution metadata](docs/semantic-execution-metadata.md)
- [Production deployment](docs/operations/production-deployment.md)
- [Disaster recovery](docs/operations/disaster-recovery.md)

`docs/product-requirements.md` is the authority for product scope. Update and
review it before changing a stated product boundary.

## Credential boundary

A logical client credential has an immutable UUIDv7 `key_id`; its secret string
is one credential generation. Rotation creates a new generation while policy,
balance account, request history, statistics and conversation data continue to
refer to the same `key_id`. Plaintext values are never stored.

Client credentials can call permitted `/v1/*` models and their own `/self/v1/*`
views. They cannot call `/internal/v1/*`. Administrative APIs require a distinct
service credential; balance grants also require `Idempotency-Key`.

## API surface

- OpenAI: `/v1/models`, `/v1/chat/completions`, `/v1/responses`,
  `/v1/embeddings`.
- Anthropic: `/v1/messages`, `/v1/messages/count_tokens`.
- Generation: `/v1/generations`, `/v1/videos/generations`,
  `/v1/images/generations`.
- Self-service: key information, requests, statistics, generation state and
  conversations bound to the authenticated client credential.
- Operator: tenant-aware credentials, provider accounts, routes, pricing,
  request events, archives and usage analytics.

Requests reserve applicable quota, balance and price bounds before outbound
execution, settle reported usage afterwards, and archive results under the
configured retention policy. Generation workers use durable database leases and
idempotent settlement.

## Configuration

Core configuration, client credentials, service credentials, provider accounts,
model routes and price definitions have JSON Schemas under `schemas/` and are
discoverable through protected administrative APIs. Provider credentials use
authenticated encryption at rest. The operator UI returns only redacted metadata.

Native OAuth flows and API-key accounts use the same stable provider-account
identity and enter the same routing, authorization, accounting and archive
pipeline. Plugin ABI details are in `wit/token-center.wit` and `plugins/README.md`.

## Delivery

Application code is Rust; repository automation is TypeScript. Python scripts
are not permitted. GitHub Actions builds and verifies `master`, producing exactly
the service and plugin-installer images. Deploy immutable digests through GitOps;
do not use mutable tags for a release.

See the deployment and recovery documents above for required secrets, database,
archive, network-policy, monitoring and rollback controls.
