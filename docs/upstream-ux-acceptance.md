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

The browser mock asserts request shape and version/idempotency headers only. It is not proof of backend implementation, vendor behavior, or deployed availability.

## Reproduction

From `web`, start `npm run dev -- --host 127.0.0.1 --port 4175`, then run:

```sh
MTC_UX_BASE_URL=http://127.0.0.1:4175 node --import tsx --test --test-concurrency=1 e2e/upstream-connection-browser.test.ts e2e/model-route-ux-browser.test.ts e2e/upstream-ux-static-browser.test.ts
node --import tsx --test e2e/upstream-ux-contract.test.ts e2e/upstream-quota-window-contract.test.ts e2e/upstream-quota-reset-safety-contract.test.ts e2e/upstream-quota-browser-contract.test.ts
npm run typecheck
npm run build
```

The static quota fixture forbids all fetches, directly renders presentation components and opens a preview confirmation without activating any quota button. Screenshot artifacts are saved under `/tmp/mtc-upstream-ux-*.png`, `/tmp/mtc-model-route-*.png`, and `web/e2e-artifacts/upstream-quota/`.
