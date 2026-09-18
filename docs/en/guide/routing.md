# Model routing

Model routing decides which upstream accounts serve the public model name requested by a client. Authorization is relational: model access is not hidden in a model-name list, but granted explicitly through the credential → route → account chain.

## Authorization chain

![Credentials receive authorization through routes or route groups; routes select accounts and provider groups, then exclusion rules, health, and model-compatibility checks produce dispatchable candidates.](/diagrams/routing.svg)

- A Route Group is an authorization set: grant a collection of routes to a credential.
- A Provider Group is a collection of upstream accounts. It participates only when a route explicitly includes it; exclusion always takes precedence over direct accounts and includes.
- A Credential Group does not participate in the authorization graph.
- Candidates are always bound to the request's public model and protocol. An incompatible catalog, disabled account, or unauthorized account cannot become a candidate.

## Create a route

`POST /internal/v1/model-routes` (see the complete definition in the [model-route JSON Schema](https://github.com/memeloop-online/memeloop-token-center/blob/master/schemas/model-route.schema.json)):

| Field | Description |
| --- | --- |
| `public_model` | Public model name visible to clients |
| `upstream_model` | Model name sent to the provider; if it is not in a selected account's catalog, `custom_model_confirmed=true` is required |
| `protocol` | `openai`, `anthropic`, or `generation` |
| `upstream_account_ids` | Candidate accounts (or use `included_provider_group_ids`) |
| `priority` | Priority when multiple routes serve the same model, default 0 |

Archive routes no longer in use with `POST /internal/v1/model-routes/{route_id}/archive`; historical request ownership is unaffected.

## Candidate ordering and account preference

Before preparing transport, the gateway resolves a bounded, authorized candidate set. Ordering comes from route priority and health. An account “preference” can only be adjusted by an installed traffic-policy plugin **within the authorized set**—it cannot add accounts, override grants, or force an unavailable account on. The client request itself cannot select an upstream account.

## Health and failover boundaries

Cooldown and “whether retry is allowed” are separate decisions:

- **Candidate may be changed:** an explicit HTTP 429 rejection or a connection failure established before dispatch.
- **Never replay:** a 503 or other 5xx received after dispatch, an output-frame error, any visible output, or an uncertain send timeout. Once dispatched, the request cannot prove that the provider did not execute it; blind retrying could create duplicate charges.

For native Codex upstreams, a structured `usage_limit_reached` 429 is recognized as quota exhaustion and cooled down using the provider's `resets_at` / `Retry-After` (maximum seven days); ordinary throttling uses the normal cooldown. Recovery after cooldown uses a single half-open probe.

When all candidates are unavailable and the failure is temporary (`unavailable`), an unsent request may wait once for recovery within its original Deadline. Waiting does not add attempts or extend the deadline. A request that has already consumed an outbound attempt does not enter the wait.

## Codex transport policy `transport_policy`

Native Codex accounts can adjust connection, failover, and SSE framing budgets in `config.transport_policy` (version 1; a missing `version` is treated as 1; unknown fields and out-of-range values are rejected):

| Field | Default | Allowed range |
| --- | --- | --- |
| `connect_attempts` | 2 | 1–4 |
| `connect_retry_delay_millis` | 150 | 0–2000 |
| `shared_probe_attempts` | Follows service health setting | 0–4 |
| `candidate_attempts` | 3 | 1–8 |
| `failover_deadline_millis` | 300000 | 1000–300000 |
| `max_sse_event_bytes` | 8388608 | 262144–16777216 |
| `max_sse_framed_bytes` | 8454144 | `max_sse_event_bytes`–16842752 |
| `max_sse_terminal_hold_bytes` | 8454144 | `max_sse_event_bytes`–`max_sse_framed_bytes` |

Change this with the existing account update `PUT /internal/v1/upstreams/{account_id}` (CAS). Candidate count and Deadline are snapshotted once on request entry. SSE limits are snapshotted once for the selected outbound attempt and shared by header admission, sanitizer, delivery capture, archive projection, and terminal hold. Runtime changes apply to later requests and never change an executing stream. After the Deadline, no new send starts, but a response stream that has been admitted successfully is not truncated.

Each admitted SSE stream reserves a two-buffer framing and terminal envelope from the process-wide proxy memory budget before delivery starts. Larger account limits therefore reduce admitted stream concurrency instead of allowing aggregate framing and terminal buffers to exceed the global budget. Archive JSON transformations retain their separate weighted admission.

## Reservation bounds for custom models `reservation_token_bounds`

Before execution, the gateway reserves balance based on price and token limits; after complete provider usage is available, it settles at actual usage. The reserved amount is not the final cost.

- A custom model outside the catalog must set exact `reservation_token_bounds`; without it, the candidate is skipped before reservation, archiving, or sending, without affecting other authorized candidates.
- Values may come only from the exact same model in a synchronized account catalog or from the provider's confirmed output limit. Do not borrow another model's context window or a historically unknown value.
- When a catalog publishes only a context window, it can be used as a conservative reservation bound, not as the provider's claimed maximum output.

## Plugin routing

Operators can select an installed plugin's routing strategy (the `group-routing-v1` ABI) for a Provider Group or Route Group. Plugins only order authorized candidates and provide bounded health advice. See [Plugin group routing](../plugins/routing.md) for protocol details.
