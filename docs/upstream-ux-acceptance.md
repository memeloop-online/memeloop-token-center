# Upstream and model form UX acceptance

PR #15 is based on the merged transport-proxy and quota-reset contracts. Acceptance runs only in GitHub CI. No development-host compilation or browser execution or live upstream/quota/reset calls are permitted. Quota interactions below use a strict in-page mock only; browser routing blocks all API and external network traffic.

## Interaction contract

- Upstream create/edit separates identity and authentication, API endpoint, and advanced network/retry settings. Fixed official Base URLs are read-only. Generic provider proxy schemas still allow their existing hostname proxies.
- Codex onboarding requires a separate private-IP `socks5h` proxy. Reauthorization reuses the backend's encrypted proxy. Editing requires explicit requester-aware `can_update_transport_proxy=true`; replacements carry account versions and an idempotency key. RFC1918, ULA and CGNAT are accepted, excluding the metadata address. Missing port defaults to 1080. Stored proxy addresses and credentials are never echoed; only redacted state, DNS mode and fingerprint appear.
- Unknown, missing and invalid proxy metadata remain distinct. Saving a proxy refreshes account metadata, not quota or health. Backend validation remains authoritative.
- Quota loads only on operator request. Display refresh is separate from supplier-credit reset; unknown usage remains unknown and failed refresh retains previous evidence. The merged reset state machine remains unchanged: separate prepare/confirm idempotency keys, in-memory confirmation token, explicit second confirmation, no automatic retry of a mutation.
- Model form fieldsets retain the merged catalog/consent behavior: stale or partial aggregate-listed models require runtime catalog-confirmed candidates; custom consent never carries across scope changes.

## Minimal regression evidence

Each retained test covers an independent failure mode:

- `upstream-ux-contract.test.ts`: deterministic proxy-policy boundary table prevents rejecting valid Tailnet proxies or accepting public/metadata/local-DNS endpoints; schema assertions protect immutable official URLs without restricting generic hostname proxy configuration. Source guards protect strict capability/version/idempotency behavior and the shared form structure.
- `upstream-quota-browser-contract.test.ts`: one browser scenario checks read-only endpoint, keyboard-opened advanced settings, proxy validation, unknown quota meter semantics, and representative desktop/light and mobile/dark overflow. Computed border/label styles must resolve to the existing theme tokens in both themes. It then mounts the real `UpstreamQuota` against strict mock fetch: initial reads/writes zero, hover no preparation, Cancel no consuming confirmation, explicit Confirm exactly once, status/reconciliation never repeat preparation or consumption. The confirmation screenshot comes from the real component. Cancel leaves the prepared operation locked; a fresh mount supplies the independent confirmed operation. No sleep, randomized timing, live fetch or proxy save is used.
- Existing provider-routing Cucumber steps cover upstream creation/editing and collapsed account controls. Existing quota-reset contracts cover the merged reset workflow; no duplicate reset state-machine tests are introduced.

The existing `web` CI job supplies compilation and browser evidence. Download `monitoring-interactions-browser-*` from the exact accepted head for screenshots under `upstream-quota/`. There is no separate fixed-port Vite process, sleep loop or duplicate browser matrix. Historical screenshots do not validate this head. Mock evidence establishes UI behavior only, not deployed vendor availability.
