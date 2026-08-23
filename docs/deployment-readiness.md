# Deployment readiness

This document defines the release gates for MemeLoop Token Center. It describes
the product deployment itself and does not depend on another gateway service.

## Release identity

- Deploy an immutable image digest built from the exact `master` commit.
- The image, Helm chart, OpenAPI document, web assets, and database migrations
  must come from the same commit.
- The current schema generation is **v54**. The chart renders
  `memeloop.io/schema-generation: "v54"` on its migration Job, and the Helm
  packaging contract rejects drift from the greatest file in
  `migrations/common/`.
- v49 adds MemeLoop Cloud event-query indexes, v50 adds generation modality and
  billing dimensions, v51 adds logical-session usage rollups, v52 retires
  legacy model allowlists, v53 persists distinct OAuth flow kinds, and v54
  completes cache-token and asynchronous-generation session projections.

## Required pre-deployment gates

1. Run Rust formatting, Clippy, all-target tests, Cucumber-rs, Cucumber.js,
   TypeScript type checking, localization checks, the web build, OpenAPI
   validation, JSON Schema validation, and Helm packaging checks.
2. Rehearse the complete migration sequence against a fresh PostgreSQL database
   and the supported SQLite test database. For an upgrade, restore a recent
   production snapshot into an isolated PostgreSQL database and apply every
   pending migration through v54 while recording lock duration.
3. Run the PostgreSQL query-plan gates for request, error, usage, and session
   aggregation at imported-data scale. SQLite evidence does not replace this.
4. Exercise the S3-compatible archive contract against a disposable bucket and
   verify that credentials, OAuth tokens, request bodies, and signed asset URLs
   are absent from logs and metrics.
5. Confirm that gateway, control, and worker Pods use non-root users, a read-only
   root filesystem, dropped Linux capabilities, no service-account token, and
   bounded CPU/memory resources.

## Upgrade sequence

1. Record the exact old CPA image, database/archive backup identifiers and
   routing configuration. Keep that revision healthy and immediately routable
   while the same-SHA candidate images and chart are staged in CPA/API2.
2. Back up PostgreSQL and the archive bucket. Record the exact backup identifiers
   and target image digest.
3. Run the Helm migration Job once. Do not allow application Pods to run schema
   migrations on startup.
4. Require the Job to finish at v54 before rolling gateway, control, or worker
   Pods. A failed or timed-out Job stops the rollout.
5. Roll the CPA/API2 trial control and worker first, then gateway replicas with
   readiness gates. Keep API3 unchanged.
   Verify `/version`, request forwarding, credential authorization, live request
   monitoring, usage analysis, and session-level monitoring before increasing
   traffic.
6. Complete the required browser matrix and real Codex CLI text/image requests.
   Preserve the prior image digest and routing configuration for rollback. Do
   not start an older binary against a schema version it does not support.
7. Promote the exact same immutable digests to production API3 only after every
   CPA/API2 trial check is green; never rebuild or substitute a tag.

## Credential access

Operator and tenant service credentials are one-time secrets. Keep the concrete
namespace and Secret name out of source control. An operator may retrieve the
issued key locally with this single-line template; the value must not be pasted
into tickets, logs, shell history, or documentation:

```sh
kubectl -n '<namespace>' get secret '<issued-credential-secret>' -o jsonpath='{.data.issued\.json}' | base64 -d | jq -r '.key'
```

Prefer a short-lived shell, disable command tracing, and rotate the credential
after shared troubleshooting.

## Rollback boundary

Traffic routing is the rollback mechanism for application defects. Database
rollback is restore-based: never reverse DDL in place after new writers have
committed data. If post-deployment verification fails, stop the new writers,
route traffic to the last compatible revision, retain failure evidence, and
restore only from the recorded backup when schema compatibility requires it.
