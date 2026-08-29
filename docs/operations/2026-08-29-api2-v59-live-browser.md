# API2 v59 live browser acceptance

Date: 2026-08-29 UTC

This is a read-only acceptance receipt for the isolated API2 trial. It does not
authorize API3, a production route change, or a CPA cutover.

## Release under test

- Source: `413f15161397f3ac640b4921c8e7becdfc8aa3b9`
- GitHub Actions: `33216849486` (`success`)
- Service image:
  `ghcr.io/memeloop-online/memeloop-token-center@sha256:f059036b27f2543972e2b5edcd273aea4ff56e46c094e7b330c9dde0d0de369c`
- Schema: `59`
- API2 gateway, control, and worker: each `1/1` Ready on the exact digest above
- Browser: Playwright Chromium `151.0.7922.34`

The public portal returned HTTP 200. The public control origin returned the
expected ingress-layer HTTP 403 from this workspace because its source address
is not in the reviewed exact `/32` allowlist. The control UI was therefore
tested through a temporary local `kubectl port-forward` to the same live
Service. The port-forward changed no Kubernetes object and was stopped after
the run.

## Browser results

The browser obtained the service and client credentials directly from the
existing Kubernetes Secrets in memory. It did not print them, place them in a
URL, write them to a file, or retain them after the contexts closed.

- The public portal authenticated the existing stable dogfood credential and
  returned stable key ID `01a042e5-ea2d-70b1-90ca-b7e39c06aae8`.
- The self-service history showed 44 existing rows. Reload restored the saved
  credential without reflecting it into the password input, and manual clear
  removed the authenticated view.
- Portal Chinese/light and English/dark modes rendered. At a 390 by 844
  viewport, horizontal overflow was exactly zero.
- The operator selected `cpa-dogfood-import`, loaded the latest 30-day usage,
  and opened all seven views: overview, trend, model, client credential,
  session, upstream account, and heatmap.
- The request metric rendered as `30.99万`, preserving Chinese number-format
  semantics. Operator Chinese/dark and English/light modes rendered. At a 390
  by 844 viewport, horizontal overflow was exactly zero.
- Operator reload restored the saved service credential and manual clear
  removed the tenant picker.
- The gateway returned `/healthz` 200 and exact 404 for both `/operator` and
  `/internal/v1/tenants`.
- The run observed no unexpected page error, console error, request failure,
  non-read-only browser method, cross-origin request, or credential-bearing
  URL/header destination.

Credential-free screenshots were retained outside Git as
`api2-portal-v59-live-chromium-20260829.png` and
`api2-operator-v59-live-chromium-20260829.png`. They are supporting visual
evidence; the assertions above are the repository receipt.

## Post-release local fixes

The browser installation also allowed the current working tree to expose and
fix three issues that are not present in the deployed `413f151` image yet:

1. after a deliberate operator credential clear, a failed ordinary client
   credential can no longer restore the prior tenant list; a failed direct
   replacement still preserves the last valid operator session;
2. a background generation refresh no longer clears a user's next image/video
   draft; and
3. the multimodal route and Codex root/worker semantic browser fixtures no
   longer depend on React render or concurrency scheduling races.

After rebuilding `web/dist`, the production build and TypeScript checks passed.
The real Chromium suite passed 20/20 scenarios and 145/145 steps. The combined
operations suite passed 44 runnable tests with three explicit environment
skips, including the TypeScript-only repository contract and the v59
conversation EXPLAIN contract. No GitHub Actions run was started for these
unreleased changes.

## Existing CPA credential continuity

A separate secret-safe read-only probe exercised every active legacy CPA
credential against the public API2 gateway. The source management token and
credentials existed only in process memory; the program emitted no secret,
credential hash, URL or stable identifier and was deleted after its temporary
port-forwards were stopped.

- all 10 active source credentials authenticated and resolved to 10 distinct
  stable Token Center key identities;
- all 10 could read their own history, accounting for 309,849 imported
  requests in total;
- all 10 received the expected denial from the control plane; and
- all 10 received an empty `/v1/models` list.

The first three results close credential attachment, stable identity, imported
history attribution and management isolation. The last result is a production
blocker: the old CPA key policy is non-uniform, with per-key grant counts of
2, 4, 4, 4, 4, 4, 4, 34, 36 and 36, but those grants have not been mapped to
Token Center routes. Granting every key the one current route group would both
broaden some source policies and omit source families that have no reviewed
target route. In particular, the current `deepseek-v4-flash` route combines
three upstream candidates, so it cannot stand in for a provider-specific old
grant without an exact candidate-set review.

The count-only evidence is retained in
[`api2-v59-legacy-credential-continuity-summary.json`](../evidence/api2-v59-legacy-credential-continuity-summary.json).
The 61 distinct non-secret source grant shapes are retained separately in
[`api2-v59-legacy-policy-grant-inventory.json`](../evidence/api2-v59-legacy-policy-grant-inventory.json),
with inventory SHA-256
`3cb8a1cb1150808e76189b2ef12cf9eaf0255583a244d51258ac04216f4beb15`.
The raw source snapshot fence at capture time was
`e28af1e10410c9a7728d3048b97e6206b32dc54b4c681318f07b4168819aaf89`;
a final dry-run must recheck it because old CPA remains live. The working tree
now contains the strict TypeScript importer, hardened Job template and 11/11
focused tests. Production authorization still requires the missing reviewed
routes, exact owner mapping, live dry-run, apply and zero-write replay.

The upstream inventory itself is not missing 18 accounts: the earlier number
27 meant four provider blocks plus 23 nested model mappings. Seven API-key
connections and two managed Codex OAuth connections correspond to all nine
current API2 upstreams. The routing layer is incomplete: the two Codex accounts
have no route candidate, Copilot and Cursor each require native
reauthorization, Claude has two source mappings and no target route, and the
DeepSeek/GLM/Qwen source mappings have been compressed into fewer target
routes. Two current generation routes use a provider family absent from the
corresponding old source policy and therefore require separate expansion
approval. The count-only comparison is
[`api2-v59-upstream-continuity-summary.json`](../evidence/api2-v59-upstream-continuity-summary.json).

The sealed CPAMP source also contains no durable paid user/subscription or
entitlement ledger: none of its 37 tables match the reviewed identity/billing
terms. Two Copilot/Cursor subscription-labelled records are upstream OAuth
accounts that require reauthorization, not customer credit. Access-policy
migration remains nonzero and mandatory, but paid entitlements cannot be
invented from aliases or upstream accounts; a separate authoritative MemeLoop
Web source is required if such subscriptions exist.

The existing paid SiliconFlow video was also re-read through the current v59
public self-service path without creating or billing another job. The full
response returned HTTP 200, `video/mp4`, exact `Content-Length` 301,078 and
SHA-256 `786c1a49aafb9b076268e678d49c80c3047ad89bcfd465d581dd2b3128c5e2ff`.
Range `32-4127` returned HTTP 206, exact 4,096 bytes and
`Content-Range: bytes 32-4127/301078`. This proves the previously generated
real video remains readable through the current exact service digest while
avoiding another paid provider request.

## Remaining boundary

This receipt closes the current API2 read-only browser/i18n/theme/usage-view,
legacy credential authentication/history, and gateway/control isolation
checks. It does not close legacy route-policy migration, the dedicated
disabled-provider secret-canary gate, the full session-archive migration, the
final CPAMP delta, a separate authoritative subscription reconciliation, an
external-failure-domain paired PostgreSQL and MinIO restore drill, or
production route rollback. API3 remains prohibited until the user explicitly
opens the production window.
