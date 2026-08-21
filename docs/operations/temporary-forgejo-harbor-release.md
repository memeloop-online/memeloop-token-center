# Temporary Forgejo Actions to Harbor release path

This is a temporary continuity path, not a second canonical release system.
GitHub remains the canonical repository and `master` history. The private
Forgejo mirror must reproduce the exact GitHub commit SHA before its workflow
is allowed to publish. GitHub Actions and GHCR remain the target long-term
release path.

The temporary path must be removed no later than **2026-09-30**, after GitHub
Actions/GHCR publication has been stable for two consecutive releases.

## Fixed endpoints and repositories

- External Forgejo: `https://git.k3s.onetwo.website`
- Runner-only Forgejo: `http://forgejo-http.forgejo.svc.cluster.local:3000`
- Private mirror: `mtc-ci/memeloop-token-center`
- Harbor service repository prefix:
  `harbor.k3s.onetwo.website/mtc-ci/memeloop-token-center`
- Companion repositories append `-importer` and `-plugin-installer`.

Only `.forgejo/workflows/harbor-release.yml` is valid. The retired
`.forgejo/workflows/build.yaml` must remain absent.

## Trust and trigger boundary

The workflow accepts trusted pushes to `master` only. It has no pull-request
or manual-dispatch trigger, checks the external Forgejo URL, repository name,
branch ref, exact lowercase 40-character commit SHA, clean checkout, and
absence of Docker/containerd host sockets. Checkout credentials are not
persisted.

A mirror operator first pushes a commit already present on canonical GitHub.
If the resulting Forgejo commit differs by even one byte/SHA, the quality job
fails before tests. Never amend, merge, or generate a release-only commit in
Forgejo.

## Runner Pod contract

`mtc-quality-pod` is a Forgejo host-mode label bound exclusively to a
short-lived, dedicated runner **Pod**. "Host mode" means commands run in the
runner container; it does not mean a physical-node runner. The Pod must use a
non-root, non-privileged security context and must not use `hostPath`,
`hostNetwork`, `hostPID`, `hostIPC`, DinD, a Docker socket, or a containerd
socket. Workspace and caches use a dedicated `longhorn-large-single` PVC;
nothing large is written to the Kubernetes node root disk.

The quality Pod has two localhost-only sidecars:

- PostgreSQL 16 at `127.0.0.1:5432`, database
  `memeloop_token_center_test`, user/password `token_center`;
- MinIO at `127.0.0.1:9000`, bucket `memeloop-token-center-test`, with the
  fixed non-production test identities in the workflow, including the
  list-only policy identity.

The sidecar data is disposable and isolated from production. Neither service
is created through Forgejo `services:` or a container daemon visible to the
workflow. The runner image provides Rust 1.95.0 with rustfmt/clippy, Node
24.18.0/npm, Python/venv, PostgreSQL and SQLite clients, Helm, kubeconform
0.6.7, Trivy, cargo-deny, cargo-audit, Chromium runtime dependencies, curl,
and jq.

`mtc-release-rootless` is a different short-lived, non-privileged runner Pod.
It receives no database/S3 connectivity and no host container socket. A
same-Pod rootless BuildKit sidecar exposes only its Unix socket through an
`emptyDir`. The recommended non-secret Forgejo variable is:

```text
MTC_ROOTLESS_BUILDKIT_HOST=unix:///run/user/1000/buildkit/buildkitd.sock
```

BuildKit state/cache resides on a dedicated `longhorn-large-single` PVC with
bounded garbage collection. The release runner contains `buildctl`, Crane,
Trivy, Cosign, jq, and base64. Its egress is limited to Forgejo, approved base
image registries, and Harbor HTTPS.

## Quality and publication sequence

The release job has a hard `needs: [quality-gates]` dependency. The quality
job runs, without release credentials:

1. secret/misconfiguration scanning, cargo-deny, cargo-audit, and vendored
   dependency provenance checks;
2. rustfmt and Clippy with warnings denied;
3. all Rust targets/features against SQLite, PostgreSQL, MinIO, mock upstreams,
   plus the explicit cucumber-rs binary;
4. npm audit, type checking, localization/schema tests, browser build, and
   Cucumber.js/Playwright acceptance;
5. replay-safe SQLite and PostgreSQL migrations plus CPAMP initial, overlap,
   incremental, conflict, and replay acceptance;
6. OpenAPI route/role checks, Helm rendering/schema checks, packaging and
   migration-operation contracts;
7. the optimized 15-minute memory, streaming, and 500 MiB asset gate.

Only after all gates pass does the release Pod build the service, importer,
and plugin-installer images. Each tag is exactly the lowercase 40-character
Git commit SHA. `latest`, `master`, shortened SHA, timestamp, and run-number
tags are forbidden. The Harbor `mtc-ci` project must enforce tag immutability.

The publisher rejects a pre-existing SHA tag, builds through rootless
BuildKit, records the returned digest, resolves the tag back from Harbor, and
requires an exact digest match. It verifies non-root user/entrypoint and OCI
revision labels, attached SPDX SBOM and SLSA provenance, scans the exact
digest for HIGH/CRITICAL vulnerabilities, signs it with Cosign, attaches a
signed release predicate, and verifies both signature and predicate. A
BuildKit attestation is counted only after the OCI index descriptor, native OCI
artifact manifest and subject, in-toto layer, statement `_type`, `predicateType`, and
the exact linux/amd64 subject SHA-256 have all been parsed and matched. Registry
strings alone are never treated as attestation evidence. A
release is complete only when one manifest contains exactly all three digest
references for the same revision.

Consumers must use `image@sha256:...` from that complete manifest. A tag or a
partial workflow run is never deployment evidence. This workflow itself does
not run kubectl, Helm upgrade, GitOps reconciliation, or database migration
against any deployed environment.

## Secrets

Only the `release-harbor` job receives these Forgejo Actions secrets:

- `HARBOR_USERNAME`
- `HARBOR_PASSWORD`
- `COSIGN_PRIVATE_KEY`
- `COSIGN_PASSWORD`
- `COSIGN_PUBLIC_KEY`

All five exist only in the environment of the single publisher shell step;
checkout and the seven-day digest-evidence upload receive none of them. The
publisher's exit trap unsets them and deletes its temporary auth directory.

Use a Harbor robot account restricted to push/pull the three `mtc-ci`
repositories. The Cosign key is dedicated to this temporary path. Secrets are
written only to a mode-0600 temporary Docker-compatible auth file or read from
environment by Cosign. They are never build arguments, OCI labels,
attestations, artifacts, repository files, or shell trace output.

## Failure and rollback

Quality failure publishes nothing. Build, scan, digest, SBOM, provenance,
signature, predicate, or three-image-set failure produces no complete release
manifest; therefore consumers must ignore any partial SHA repositories. Since
SHA tags are immutable, an authorized Harbor administrator must remove all
partial repositories for that SHA before an explicit retry.

This path changes no deployment and needs no service rollback. If it behaves
incorrectly, disable Forgejo Actions for the mirror or disable both runner
labels. Existing GHCR digest deployments remain unchanged.

## Removal checklist (by 2026-09-30)

1. Confirm two consecutive GitHub Actions/GHCR releases pass the same shared
   release source contracts and immutable digest verification.
2. Point every GitOps consumer back to signed GHCR digest manifests in a
   separately approved deployment change.
3. Disable `mtc-quality-pod` and `mtc-release-rootless`, revoke the Harbor robot
   and temporary Cosign key, and delete the five Forgejo secrets/one variable.
4. Remove `.forgejo/workflows/harbor-release.yml`, this runbook, its contract
   test, and Forgejo-only publisher while retaining shared GitHub release
   validation where still useful.
5. Delete temporary Harbor `mtc-ci` image/referrer/cache data only after its
   digests are absent from every deployment and audit retention has elapsed.
6. Delete the private Forgejo mirror after audit artifacts are retained.
