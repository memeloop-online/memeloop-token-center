# API2 e339de8 live acceptance

Date: 2026-08-29 UTC

This receipt covers only the reversible API2 trial. It does not authorize an
API3 or old-CPA change.

## Exact release and rollout

- Source: `e339de8982cd2b485f02e1252bf449bbd564b560`
- GitHub Actions: `33240090081` (`success`)
- Release manifest SHA-256:
  `32a9319a0d4b28d046818b3d737d8b268bfb2b6029ccfa9f915a13b7436b8b0b`
- Service:
  `ghcr.io/memeloop-online/memeloop-token-center@sha256:d4be9bcbd5b5ed5d906ea007aaeb307b2645ece4e169997506f595ca2771afb1`
- Importer:
  `ghcr.io/memeloop-online/memeloop-token-center-importer@sha256:75fef96d76af5fb39851514fd89ba8e12fee394e88c7d6f5fac91a5e8ca5a429`
- Plugin installer:
  `ghcr.io/memeloop-online/memeloop-token-center-plugin-installer@sha256:b29b0d5da789744b07fe85d579bad54d47f8d8fa8d0aeebf13903f4c1c41c9af`
- Schema: `59`
- GitOps revisions: `80eb28c` staged the candidate, `746b5ae` started the
  one-shot sync, and `c3416f3` restored manual sync.

The exact-SHA CI passed every release gate, including the optimized 15-minute
memory/stream/500 MiB asset harness and publication verification. Argo then
reported Synced/Healthy with automated synchronization absent. Gateway,
control and worker each became 1/1 Ready with zero restarts and the exact
service digest. The previous service digest
`sha256:24e46602754d80dc885959ce83f4d08040081ce93db6c4331c9c24ae93ae7e39`
is the immediate schema-compatible rollback point.

## Browser acceptance

Playwright Chromium `151.0.7922.34` used only GET, HEAD and OPTIONS. Existing
Kubernetes credentials were read into process memory and were never printed,
placed in a URL or written to a file.

- portal, gateway health, stable dogfood identity and historical data loaded;
- portal and operator credentials survived reload, then remained absent after
  explicit manual clear and another reload;
- the singleton tenant selected automatically without the all-tenant warning;
- all seven usage views loaded and one-click price synchronization was enabled;
- route creation was collapsed by default;
- an actual upstream selection rendered its selection chip and its exact
  tenant/account/model-catalog request returned HTTP 200;
- entering a new model displayed the explicit custom-model confirmation and
  left the create button valid without submitting a write;
- Chinese/English, light/dark and 390 by 844 layouts had no horizontal
  overflow;
- gateway health returned HTTP 200 while gateway `/operator` returned the
  required HTTP 404; and
- no page, console, request or cross-origin failure occurred.

The remote workspace remains outside the operator ingress allowlist, so the
same live control Service was inspected through a temporary local
port-forward. It was stopped after the run. Credential-free screenshots are
retained outside Git as
`api2-operator-e339de8-live-chromium-20260829.png` and
`api2-portal-e339de8-live-chromium-20260829.png`.

## Codex CLI text and image

Codex CLI `0.150.0-alpha.8` used API2 as a custom Responses provider. With
`request_max_retries=0`, `stream_max_retries=0` and user configuration ignored,
model `Qwen` returned exact marker `MTC_API2_E339DE8_NO_RETRY_TEXT_OK`. The
corresponding self-service session recorded name
`MTC API2 e339de8 Codex CLI no-retry acceptance`, task kind
`release-dogfood-no-retry`, one successful request and zero errors.

The same CLI invoked a temporary TypeScript-only image driver. Real
`qwen-image` request `01a04ca8-6621-7330-b472-c5dfaa172c21` completed HTTP 200,
settled exactly 0.01 USD and archived one PNG:

- response: 288 bytes, SHA-256
  `55f79e6df6be7ef8439429252bf1f26f8db537bb6d5c5157a9fe00cc644435cd`;
- PNG: 515,188 bytes, exact `Content-Length`, `image/png`, SHA-256
  `1a3aab5282dadbefb79c75b4997afe9e5b431b26602ec9cefe8a55a4ec3a885b`;
- two exact same-idempotency replays returned the same request ID and
  byte-identical response without another provider generation or image charge;
  and
- request detail reported `archive_complete=true`, protocol `openai-image` and
  cost `0.01`.

The retained PNG is
`api2-codex-image-e339de8-20260829.png`. The temporary browser and image scripts
were deleted, the port-forward was stopped, and no temporary Pod, Job, PVC or
cluster configuration remained.

## Reused unchanged-runtime gates

Changes after the immediately preceding `8d3348e` release affect the offline
archive/route importer and Web test synchronization, not gateway, control or
worker runtime behavior. Its 30-sample public-health/readiness/digest/restart
observation and cleaned disabled-provider sentinel canary therefore remain
applicable. The exact `e339de8` browser and Codex CLI runs above independently
rechecked the live Web and runtime paths that changed or could regress.

## Remaining boundary

This closes exact-release API2 browser and Codex CLI text/image acceptance. It
does not close owner-reviewed legacy route/policy apply, Copilot/Cursor OAuth,
complete session-archive migration, final CPAMP delta, subscription authority,
paired PostgreSQL/MinIO recovery or production route rollback. API3 still
requires the user's explicit production-window approval.
