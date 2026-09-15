# Native Cursor OAuth reads

This adapter implements Cursor's own Connect/protobuf read protocol. It does
not execute Cursor Agent, run a bridge, use Cloud Agents API keys, or represent
Cursor OAuth as an OpenAI-compatible HTTP service.

## Protocol provenance

Static analysis of the first-party [Cursor Agent 2026.07.23-e383d2b distribution](https://downloads.cursor.com/lab/2026.07.23-e383d2b/linux/x64/agent-cli-package.tar.gz),
SHA-256 `702ad595213bee5df0268be9f80a19f29fcceaa2a42fc55e39f2b5199051f0c4`.
The distribution was read as text, not executed. No distribution code is
vendored here. The relevant named bundle modules are:

- `src/client.ts`: AgentService methods, OAuth Bearer interceptor, HTTP/1
  Connect transport and Cursor client/privacy headers.
- Generated `agent/v1/agent_service_pb.js`: GetUsableModelsRequest field 1
  repeated custom_model_ids; response field 1 repeated ModelDetails.
- Generated `agent/v1/agent_pb.js`: ModelDetails field 1 model_id; fields
  8–10 are optional BYOK credential messages, not public model metadata.
- `5727.index.js` model-list command constructs `src/client.ts`'s AiService
  client; `7397.index.js` model discovery invokes its GetUsableModels with an
  empty custom-model list. AgentService declares the same messages, but that
  declaration alone does not establish the CLI's model-list endpoint.
- `1931.index.js`, `src/usage/usage-data.ts`: actual DashboardService usage,
  hard-limit and plan reads under the signed-in OAuth client.

The implementation is independently written against those wire definitions.
The service is not a documented compatibility API; changes in future clients
or supplier responses require new evidence and fixture updates.

## Transport and authority

All requests target the fixed HTTPS `api2.cursor.sh` backend, HTTP/1.1,
`POST /<service>/<method>`, `Content-Type: application/proto`, and
`Connect-Protocol-Version: 1`. Unary protobuf bodies are **not** streaming
five-byte envelopes. The supported requests are empty protobuf messages.
OAuth identity and expiry are validated before a read. The native access
token is the Bearer credential; no refresh is performed by this adapter.

Only the encrypted account proxy is used. Account base URLs, request headers,
server-config agent URLs and environment proxies cannot redirect authority.
The existing network validation, explicit-proxy and no-retry client builders
apply. Redirects are disabled; errors expose bounded static codes rather than
supplier bodies, request URLs or credentials. Reads have an eight-second
whole-operation deadline and a two-MiB response cap. No production endpoint
override is exposed for fixtures.

## Model catalog semantics

`/aiserver.v1.AiService/GetUsableModels` yields public model IDs. The persisted
source is `cursor_native`, protocol `cursor_agent`. This protocol marker does
not promise a working inference adapter. The current catalog schema does not
store display names; raw supplier messages are never persisted because they
can contain BYOK credentials. Context window, output limits and reservation
bounds remain absent: ModelDetails does not provide trustworthy values for
them. Existing generation/lease fences still govern catalog replacement.

## Inference remains a separate implementation

The first-party client uses `agent.v1.AgentService/Run` as a bidirectional
Connect stream, or `RunSSE` together with `aiserver.v1.BidiService/BidiAppend`
under HTTP/1. It exchanges typed conversation, KV, execution and interaction
messages, not an OpenAI chat-completions payload. Discovery and quota reads
must not enable a generic OpenAI inference fallback. A future adapter must
also preserve optional authoritative TurnEndedUpdate token counters instead
of treating missing usage as zero.
