# Upstream accounts

![Upstream accounts with routes, availability, and quota refresh controls. Identity details are replaced; the interface is shown in Chinese.](/images/providers.png)

An upstream account is the connection unit between MTC and an AI provider. It has a stable `account_id`, a provider driver `driver`, connection configuration `config`, and a current encrypted credential generation.

## Account model

- `api_key`, `oauth`, and `none` are **connection methods**. Re-authorization preserves the account identity, routes, and historical ownership.
- Provider keys and OAuth tokens are encrypted at rest, and account interfaces never return them; `config` contains no credential material.
- Each account maintains a model catalog synchronized by credential generation (`POST /internal/v1/upstreams/{account_id}/models/sync`). Route creation uses the catalog to validate public/upstream model compatibility.
- Common management interfaces: `GET/POST /internal/v1/upstreams`, `GET/PUT /internal/v1/upstreams/{account_id}`, `GET /internal/v1/upstreams/{account_id}/health`, and `GET /internal/v1/upstream-availability`.

## Synchronizing models and prices

Choose **Sync models and prices** in an upstream account's details or editor. MTC reads the directory using the account's connection settings, then matches models against configured price sources. Each step displays its own result. Models awaiting pricing can be completed in model pricing.

Manual and background account refreshes share the server-side coordinator. A successful directory update commits before USD price synchronization; unavailable price sources never roll back model availability or overwrite manual prices. The sync response reports pricing separately in `price_sync` (`ready`, `partial`, `error`, or `skipped`). Directory reads expose removed models in `disabled_models`, including their disappearance time, while `models` remains the active selection list. Temporary discovery failures retain both lists unchanged.

Compatibility note: these are additive fields on the internal v1 response. Clients that reject unknown response properties must update their validation schema alongside this release. The existing active-model selection semantics are unchanged.

Newly discovered models appear in the route model picker. When a previously discovered model disappears, its account stops participating in routes for that model; participation resumes when the model returns. Manual route enable/disable settings are preserved. Private models that have only been configured manually retain their custom selection.

Public model names, account selections, and client access remain managed through routing configuration.

## OAuth login

OAuth accounts are created through management-side login flows, with tokens stored encrypted:

| Provider | Flow | Endpoints |
| --- | --- | --- |
| Codex / Kimi / Copilot / Cursor | Device code (start + poll) | `POST /internal/v1/oauth/{provider}/start`, `POST /internal/v1/oauth/{provider}/poll` |
| Claude | Authorization code (start + complete) | `POST /internal/v1/oauth/claude/start`, `POST /internal/v1/oauth/claude/complete` |
| Plugin-declared provider | Generic authorization-code PKCE | `POST /internal/v1/oauth/authorization-code/start`, `.../complete` |
| Plugin adapter | PKCE polling | `POST /internal/v1/oauth/provider-adapter/start`, `.../poll` |

Login management interfaces require `oauth:write`. Existing accounts support `POST /internal/v1/upstreams/{account_id}/oauth/refresh` and `POST /internal/v1/upstreams/{account_id}/oauth/disconnect`.

## Account proxies

An account can use a private SOCKS5 proxy. The proxy URL (including optional proxy username and password) is encrypted and viewed or changed only through dedicated management interfaces:

- Ordinary account metadata exposes only `has_proxy`, the proxy scheme, remote-DNS semantics, and a host-free label/fingerprint for display and recognition.
- To view or copy the complete proxy URL, use the explicit management interface (requires `providers:write`, global operator permission, and tenant-ownership validation):

```bash
curl "https://mtc.example.com/internal/v1/upstreams/0193f2ab-7c1e-7000-8000-0000000000a1/transport-proxy?tenant_external_id=default" \
  -H "Authorization: Bearer mts_example_service_token"
```

This GET returns the full proxy URL, network scope, and version fields required for editing, with `Cache-Control: private, no-store`. A `PUT` on the same path with the current version changes the proxy while preserving the account's API key and OAuth tokens. Rules:

- `socks5`: MTC resolves and pins both target and proxy; `socks5h`: the proxy resolves the provider hostname, and only safe private-IP-literal proxies are allowed.
- HTTP(S) proxies, public SOCKS proxies, and hostname-form `socks5h` are rejected; special addresses such as metadata endpoints are always blocked.

## Reading quotas

`GET /internal/v1/upstreams/{account_id}/quota` (requires `providers:read`) reads a provider-side quota snapshot for an account, for operator observation and optional plugin input:

The provider list's **Refresh all** action uses `POST /internal/v1/upstreams/quota/batch` with the current page's account IDs. The server derives tenant ownership from the authenticated service and the loaded accounts, loads all current account credentials in one database statement, and runs at most three reads concurrently through the same cache, per-account singleflight, and global quota-read permits. Results are returned per account, so a failed account retains its prior UI snapshot while successful peers update. The batch read never performs a quota reset.

- The snapshot is cached for 30 seconds with concurrent request coalescing. After a read failure, a bounded stale value is kept for at most five minutes; failure never presents old evidence as fresh.
- Operator refresh controls request `fresh=true` and label the action as `trigger=manual` or `trigger=bulk`. This bypasses a still-current cache entry while preserving safe coalescing with a newer in-flight read.
- The response is organized by **window**. Each window includes an identifier, period (such as five hours or weekly), reset time, used/remaining ratio, and exhausted state. Window information comes from explicit provider response fields—unknown quantities remain unknown and are never shown as zero or full.
- `unsupported` means this account type has no quota adapter; it does not mean “unlimited quota.”
- Reading quota does not refresh tokens, make model requests, or consume reset quota.

Current adapter coverage:

| Provider | Read data |
| --- | --- |
| Codex (native OAuth) | Usage windows, reset time, and reset-credit balance |
| Kimi (native OAuth) | Usage-window ratio and period |
| Google Antigravity (plugin OAuth) | Remaining ratio by group/bucket and explicit window metadata |
| Cursor (native OAuth) | Read-only model list, current billing-period usage, and plan information. **MTC does not provide Cursor inference:** Cursor accounts can only be used for the read-only data above and cannot serve as an upstream for model requests. |

## Quota reset

Some providers offer reset-credit operations. MTC models this as an explicit two-step confirmation; no read ever triggers it implicitly:

1. `POST /internal/v1/upstreams/{account_id}/quota-reset/prepare` (`Idempotency-Key`): create an operation from a fresh read-only observation and return a one-time `confirmation_token` valid for 120 seconds. This step consumes no quota.
2. `POST /internal/v1/upstreams/{account_id}/quota-reset/{operation_id}/confirm`: use the confirmation token to request the reset from the provider.
3. `POST /internal/v1/upstreams/{account_id}/quota-reset/{operation_id}/reconcile`: re-check the observation after reset; query operation status with `GET /internal/v1/upstreams/{account_id}/quota-reset/{operation_id}`.

Prepare, confirm, and reconcile each have their own idempotency key. An outstanding operation blocks another prepare for the same account.

## Deletion and retirement

Before deleting an account, use `GET /internal/v1/upstreams/{account_id}/deletion-readiness` to check for route references. Once an account is disabled, historical request ownership remains unchanged.
