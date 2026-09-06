# Development handoff

## Current product boundary

Memeloop Token Center is a standalone, multi-provider API gateway, credential
and usage-control product. Its source of truth is this repository's
`master` branch. The supported runtime is the gateway, control and worker
roles described in [production deployment](operations/production-deployment.md).

The product accepts native client credentials, records request and usage
history, keeps response assets in the configured archive, and provides
operator and self-service interfaces. Historical records are retained and
read through the normal request, conversation and archive APIs; do not remove
their schema or reader path during ordinary feature work.

## Release contract

1. Merge reviewed changes to `master`.
2. GitHub Actions is the only build and verification environment. Do not
   substitute local builds for its gates.
3. A release creates exactly two runtime artifacts: the service image and the
   plugin-installer image. Deploy immutable digests, never tags.
4. Apply digests through the approved GitOps change. A rollout is not complete
   until readiness, authenticated request, streaming, image-generation,
   archive and operator checks succeed.
5. Roll back only by returning to a previously verified immutable digest. For
   an incompatible database change, use the recovery procedure rather than
   reverse DDL.

The release workflows, package scripts and image verification contracts are
kept aligned on this two-image surface. Any proposed third image needs an
explicit product and operations decision.

## Data safety and recovery

Production data comprises PostgreSQL records, archive objects, referenced
secrets, image digests and chart values. Follow
[disaster recovery](operations/disaster-recovery.md) for backup, isolated
restore validation and traffic recovery. That document is product-native:
it intentionally describes durable recovery rather than one-time data
conversion procedures.

Before deleting a schema, archive reader, credential representation or
provider adapter, establish that it has no live rows and that retained history
does not depend on it. Record the evidence and ship a forward cleanup
migration; never make a destructive schema change as a documentation cleanup.

## Engineering constraints

- Application code is Rust. Repository automation is TypeScript; do not add
  Python scripts.
- Do not place credentials, keys, account payloads or archive objects in Git,
  logs, fixtures or documentation.
- Preserve tenant isolation, idempotency and stable request attribution.
- Keep provider traffic bounded, protected by network policy and routed through
  explicitly configured egress where required.
- New public listener types, NodePorts and hostPorts require an explicit
  operations decision.
- Keep operational metrics low-cardinality and diagnostics protected by the
  control-plane authentication boundary.

## Reference documents

- [Product requirements](product-requirements.md)
- [Architecture](architecture.md)
- [API contract](api-contract.md)
- [Deployment readiness](deployment-readiness.md)
- [Production deployment](operations/production-deployment.md)
- [Disaster recovery](operations/disaster-recovery.md)
- [Security audit](security-audit.md)
- [Performance contract](performance.md)

## Handoff checklist

- Confirm the working tree does not overwrite concurrent work.
- Keep the durable rollout record in the GitOps repository current.
- Classify defects by impact; fix availability, authorization, accounting,
  data-integrity and privacy faults before release.
- Verify UI changes at wide, tablet and phone widths with keyboard access and
  localized number/date formatting.
- Attach GitHub Actions evidence and immutable artifact digests to the release
  record. Do not declare a rollout complete from local-only checks.
