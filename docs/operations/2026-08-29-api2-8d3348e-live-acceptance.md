# API2 8d3348e live acceptance

Date: 2026-08-29 UTC

This receipt covers only the reversible API2 trial. It does not authorize an
API3 or old-CPA change.

## Exact release

- Source: `8d3348e79921efd9e9e54a124cca40efbf000f9d`
- GitHub Actions: `33233291973` (`success`)
- Service:
  `ghcr.io/memeloop-online/memeloop-token-center@sha256:24e46602754d80dc885959ce83f4d08040081ce93db6c4331c9c24ae93ae7e39`
- Importer:
  `ghcr.io/memeloop-online/memeloop-token-center-importer@sha256:66487e14184c72f8692e3a7e6628c7ba57dbd938bb1c1bc70b61b76d5a0d3fa4`
- Plugin installer:
  `ghcr.io/memeloop-online/memeloop-token-center-plugin-installer@sha256:fb7f462962903ed7a6ca1cb0517985c3dcbf11f212d3cacbdd78e30d51451784`
- Schema: `59`
- GitOps final manual-sync revision:
  `120725358732f2e0c5e8b953653fc344f28924fe`

The release manifest binds all three digests to the source SHA. Anonymous GHCR
resolution returned HTTP 200 and the exact service digest. Argo completed the
one-shot sync and then returned to manual mode. Gateway, control and worker were
each 1/1 Ready with zero restarts and exact image ID. The previous service
digest `sha256:f059036b27f2543972e2b5edcd273aea4ff56e46c094e7b330c9dde0d0de369c`
is retained as the immediate schema-compatible rollback point.

## Browser acceptance

Playwright Chromium `151.0.7922.34` used only GET/HEAD/OPTIONS browser methods.
Credentials were read from existing Kubernetes Secrets into process memory and
were never printed, placed in URLs or written to files.

- public portal and gateway health returned HTTP 200;
- gateway operator and internal-tenant paths returned exact HTTP 404;
- the dogfood credential matched its expected stable key ID and showed 44
  historical rows;
- portal and operator credentials survived reload without appearing in the
  password field, then disappeared after manual clear and another reload;
- singleton tenant `cpa-dogfood-import` selected automatically without the
  all-tenant warning;
- all seven usage views loaded;
- one-click price synchronization was enabled;
- client credential creation and route creation were collapsed by default;
- selecting an upstream and entering a new model displayed the explicit
  unverified-custom-model confirmation without submitting the form;
- Chinese/English, light/dark and 390 by 844 layouts had no horizontal
  overflow; and
- no unexpected page, console, request, cross-origin or non-read-only-method
  failure occurred.

The remote workspace is not an approved control-ingress source and received
the expected ingress-layer HTTP 403. The same live control Service was tested
through a temporary local port-forward without widening the allowlist; it was
stopped after the run. Credential-free screenshots are retained outside Git as
`api2-operator-8d3348e-live-chromium-20260829.png` and
`api2-portal-8d3348e-live-chromium-20260829.png`.

## Codex CLI text and image

Codex CLI `0.150.0-alpha.8` used API2 as a custom Responses provider with model
`Qwen` and returned exact marker `MTC_API2_8D3348E_TEXT_OK`. Self-service
session detail showed HTTP 200 plus the declared session name, task kind, agent
ID, trace ID, explicit structure session ID and Codex CLI client identity.

The same CLI invoked the TypeScript-only image acceptance driver. Real
`qwen-image` request `01a04bf6-02b5-7552-99e6-820ca022022b` completed HTTP 200,
settled exactly 0.01 USD and archived one PNG:

- response: 288 bytes, SHA-256
  `6996c47e9c203a107e8408b366455b4e3ef21e3d0f085657a312d2b06ed799de`;
- PNG: 591,547 bytes, exact `Content-Length`, `image/png`, SHA-256
  `3f7d625ad7c78b620878014d4a9d386f17347b3591a73f68fe8e12bf7c232d9c`;
- two exact same-idempotency replays returned the same request ID and
  byte-identical response without another provider generation or image charge.

The temporary browser port-forward was stopped. No temporary Pod, Job, PVC or
cluster configuration remained. API3 and old CPA were untouched.

## Stability sample

A 30-sample observation at ten-second intervals covered the public portal
health endpoint plus the gateway, control and worker Pod status, restart count,
exact image ID and memory use. All 30 health requests returned HTTP 200; every
service Pod remained Ready on the exact digest with zero restarts or digest
mismatches. Maximum service-Pod memory was 9 MiB. Public-path health latency was
1,273.910 ms at p95 and 5,489.086 ms maximum; this includes the remote
workspace-to-ingress network path and did not coincide with a readiness failure.
The machine-readable receipt is
`docs/evidence/api2-8d3348e-stability-summary.json`.

## Disabled-provider secret canary

A dedicated trial-only provider was created with a fresh high-entropy sentinel
credential and immediately disabled before browser inspection. The provider had
no routes or request history. The existing guarded Chromium profile audited the
provider list, 100 request summaries, one request detail and the rendered DOM;
the sentinel appeared zero times. Chromium issued no non-read-only or
cross-origin request. The ordinary dogfood client credential received HTTP 401
from the control upstream-list endpoint.

The disabled provider was deleted with its latest `expected_updated_at`. A
post-delete list returned zero matching rows. The port-forward was stopped, no
Kubernetes object was created, and the Pod/Job/PVC name-plus-UID inventory had
the same 64-object SHA-256 before and after. The secret-safe machine receipt is
[`../evidence/api2-8d3348e-disabled-provider-canary.json`](../evidence/api2-8d3348e-disabled-provider-canary.json).

One preliminary preflight failed closed on an HTTP 400 before the successful
recorded run. Its cleanup handler completed and created no retained evidence or
cluster object. Because that diagnostic did not yet retain the failing path,
the response cannot be attributed more precisely and is not counted as a
passed canary; the complete rerun above is the recorded gate.

## Remaining boundary

This closes exact-release API2 browser and Codex CLI text/image acceptance. It
does not close owner-reviewed legacy route/policy apply, Copilot/Cursor OAuth,
complete session-archive migration, final CPAMP delta, subscription authority,
paired PostgreSQL/MinIO recovery or production route rollback. API3 still
requires the user's explicit production-window approval.
