# Upstream and model form UX acceptance

Scope: PR #15 (`feat/upstream-ux-redesign`). All browser actions run on a loopback-only Vite server with intercepted API responses. No production writes, real model requests, health probes, quota reads/refreshes, reset preparations or reset confirmations are performed during this acceptance.

## Interaction contract

- Fixed Codex Base URL is visible before OAuth login. The separate, required private `socks5h://` proxy is validated before login and never echoed from stored credentials. Invalid inputs associate their hint/error with the input for assistive technology.
- Upstream cards separate connection settings, availability details, account authorization operations and a collapsed danger zone. Quota remains unloaded until explicitly requested by the operator; page load does not fan out provider requests.
- Model create/edit forms share three named fieldsets: public name, upstream/model selection, priority/access. Save requires a valid catalog selection, effective candidates, compatible protocol and integer priority in the supported range. Saving configuration does not perform inference.
- Quota cards preserve unknown usage as unknown, mark stale data and retain the last snapshot after a failed refresh. Reset controls explain supplier-credit consumption and are separate from display refresh. The shared confirmation dialog starts on Cancel, traps focus and supports Escape.

## Backend integration gap (not hidden by mocks)

This PR's original backend baseline does **not** implement `PUT /internal/v1/upstreams/{id}/transport-proxy` or return the redacted `has_proxy`, `proxy_scheme`, `proxy_remote_dns` fields. Integration requires the backend to implement and validate this contract before the proxy-save workflow is usable:

- Request: `tenant_external_id`, replacement `proxy_url`, `expected_updated_at`, `expected_credential_generation`; nonempty `Idempotency-Key` header.
- Response: updated account metadata including credential generation and redacted proxy state; never return the proxy URL, username or password.
- Enforce write tenant, optimistic concurrency, private-IP `socks5h` with remote DNS, credential encryption and idempotency. Reject unsupported providers or stale updates without replacing credentials. Saving must not trigger a health probe, quota refresh or model request.
- Unknown proxy metadata displays as unknown, not proof of a missing or configured proxy. API errors retain the editor with a non-secret retry message.

The backend transport-proxy workstream has confirmed this request/response contract, including the optional `can_update_transport_proxy` capability. The UI honors an explicit false value. Quota recovery hardening is tracked separately in [PR #23](https://github.com/memeloop-online/memeloop-token-center/pull/23); prepare and confirm each use their own idempotency key, and a user-requested confirmation retry reuses the original key and token.

These dependencies must land before declaring end-to-end backend availability. The browser mock asserts request shape and version/idempotency headers only. It is not proof of backend implementation, vendor behavior, or deployed availability.

## Reproduction

Acceptance is performed by GitHub CI, not on the development host. Push the PR branch and inspect the `web` job. It runs type checking, production build, the browser-backed contracts and a loopback-only Vite server for these isolated interaction tests:

```sh
MTC_UX_BASE_URL=http://127.0.0.1:4175 node --import tsx --test --test-concurrency=1 e2e/upstream-connection-browser.test.ts e2e/model-route-ux-browser.test.ts e2e/upstream-ux-static-browser.test.ts
```

The static quota fixture forbids all fetches, directly renders presentation components and opens a preview confirmation without activating any quota button. Download the `upstream-model-ux-browser-*` and `monitoring-interactions-browser-*` artifacts from that exact CI run. They retain mobile/desktop connection and model forms, quota unknown/stale/retained-error states and the Cancel-focused confirmation dialog. Old development-host screenshots are not acceptance evidence for this revision.
