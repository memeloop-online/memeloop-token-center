# Requests and usage

![Live request list showing models, client credentials, usage, costs, and response performance. Identity details are replaced; the interface is shown in Chinese.](/images/requests.png)

MTC keeps queryable model, protocol, status, usage, and cost information for each client request. This page describes the views available to a client and the meaning of settlement data.

## Query your requests

A client credential can query its own data through these paths:

| Path | Contents |
| --- | --- |
| `GET /self/v1/requests` | Paginated request records |
| `GET /self/v1/stats` | Usage and cost statistics |
| `GET /self/v1/sessions` | Sessions and related requests |
| `GET /self/v1/conversations` | Replayable conversation records |

Use `limit` and the cursor returned by the service to read the next page. Fields vary slightly across protocols; unknown values remain unknown and should not be treated as zero.

## Session and execution metadata

Downstream applications can attach optional metadata to text requests for request-list and session views:

| Header | Meaning |
| --- | --- |
| `X-MTC-Session-Name` | Human-readable session name |
| `traceparent` | W3C trace context |
| `X-MTC-Trace-Id` / `X-MTC-Span-Id` / `X-MTC-Parent-Span-Id` | Trace and span identifiers |
| `X-MTC-Agent-Id` / `X-MTC-Parent-Agent-Id` | Agent instance and parent |
| `X-MTC-Task-Kind` | Task type such as `interactive` or `background` |
| `X-MTC-Session-Labels` | A bounded object of string labels |

These fields support visualization and correlation; they do not change authorization, routing, or billing identity. Missing values remain missing, and MTC does not infer session names or task types from request content.

## Usage and cost

- `provider_reported` means usage reported by the provider at terminal state.
- `provider_estimated` means an estimate supplied by the provider.
- `contract_ceiling` means the contract ceiling used when reliable usage was unavailable.
- `not_observed` means no valid usage was observed; it is not a zero-cost result.

Cost is the settlement amount in MTC's local ledger, not the provider invoice. Archive status distinguishes a body that is still uploading, an upload failure, and a body that is unavailable.
