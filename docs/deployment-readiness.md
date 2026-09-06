# Deployment readiness

This document defines release gates for Memeloop Token Center.

## Release identity

- Deploy immutable service and plugin-installer image digests built from the exact
  `master` commit.
- Keep the image, Helm chart, OpenAPI document, web assets and migrations from the
  same commit.
- The chart derives the schema generation from the registered migration set; do not
  maintain a manually copied version number in release tests.

## Required gates

- GitHub Actions passes formatting, lint, Rust tests, TypeScript checks, web build,
  OpenAPI and schema validation, chart packaging and release-image verification.
- Rehearse pending migrations on a fresh supported database and an isolated restored
  production snapshot. Record lock time and capacity; do not hide a lock problem by
  extending a deadline.
- Verify image content and artifact provenance for both runtime images.
- Verify protected control, authenticated gateway, streaming, image generation,
  archive read/write, request history, pricing, routing failover and browser UX.
- Confirm health, readiness, metrics, alerts, ingress policies, egress policy and
  database/object-store capacity.

## Rollout

Apply the release only through the approved GitOps change. Roll gateway replicas
gradually with a disruption budget and a termination grace period longer than the
largest admitted stream. Confirm a new replica is ready before reducing prior
capacity. Monitor request success, upstream health, error classes, queue depth,
database pool wait and archive state throughout the rollout.

A rollout can be stopped at any point before an incompatible database barrier.
Return to the last verified immutable image digest if application acceptance fails.
For an incompatible schema change, recover a validated database copy rather than
attempting reverse DDL.

## Data protection

Before deployment, confirm database backup recency, object-store replication or
versioning, restore credentials and a tested isolated restore path. Preserve request
and archive history when retiring configuration; do not delete records merely because
an account, route or credential is disabled.

See [production deployment](operations/production-deployment.md) for chart
constraints and [disaster recovery](operations/disaster-recovery.md) for recovery.

## Evidence

Attach GitHub Actions run URLs, exact source commit, immutable image digests, chart
revision, migration result, rollout timestamps, acceptance checks, alert state and
rollback point to the GitOps release record. Local-only checks are diagnostic and do
not authorize a release.
