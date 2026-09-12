# Upstream and model form UX acceptance

PR #15 is based on the merged transport-proxy and quota-reset contracts. Acceptance runs only in GitHub CI. No development-host compilation or browser execution, live upstream calls, quota refresh, reset preparation or reset confirmation are permitted for this acceptance.

## Interaction contract

- Upstream create/edit separates identity and authentication, API endpoint, and advanced network/retry settings. Fixed official Base URLs are read-only. Generic provider proxy schemas still allow their existing hostname proxies.
- Codex onboarding requires a separate private-IP `socks5h` proxy. Reauthorization reuses the backend's encrypted proxy. Editing requires explicit requester-aware `can_update_transport_proxy=true`; replacements carry account versions and an idempotency key. RFC1918, ULA and CGNAT are accepted, excluding the metadata address. Missing port defaults to 1080. Stored proxy addresses and credentials are never echoed; only redacted state, DNS mode and fingerprint appear.
- Unknown, missing and invalid proxy metadata remain distinct. Saving a proxy refreshes account metadata, not quota or health. Backend validation remains authoritative.
- Quota loads only on operator request. Display refresh is separate from supplier-credit reset; unknown usage remains unknown and failed refresh retains previous evidence. The merged reset state machine remains unchanged: separate prepare/confirm idempotency keys, in-memory confirmation token, explicit second confirmation, no automatic retry of a mutation.
- Model form fieldsets retain the merged catalog/consent behavior: stale or partial aggregate-listed models require runtime catalog-confirmed candidates; custom consent never carries across scope changes.

## Minimal regression evidence

Each retained test covers an independent failure mode:

- `upstream-ux-contract.test.ts`: deterministic proxy-policy boundary table prevents rejecting valid Tailnet proxies or accepting public/metadata/local-DNS endpoints; schema assertions protect immutable official URLs without restricting generic hostname proxy configuration. Source guards protect strict capability/version/idempotency behavior and the shared form structure.
- `upstream-quota-browser-contract.test.ts`: one loopback-only static fixture checks read-only endpoint, keyboard-opened advanced settings, proxy validation, unknown quota meter semantics, representative desktop/light and mobile/dark overflow, and Cancel-focused/Escape-dismissable confirmation. All API/external requests are blocked. It never saves a proxy or activates quota controls; confirmation is a presentation-only preview.
- Existing provider-routing Cucumber steps cover upstream creation/editing and collapsed account controls. Existing quota-reset contracts cover the merged reset workflow; no duplicate reset state-machine tests are introduced.

The existing `web` CI job supplies compilation and browser evidence. Download `monitoring-interactions-browser-*` from the exact accepted head for screenshots under `upstream-quota/`. There is no separate fixed-port Vite process, sleep loop or duplicate browser matrix. Historical screenshots do not validate this head. Mock evidence establishes UI behavior only, not deployed vendor availability.
