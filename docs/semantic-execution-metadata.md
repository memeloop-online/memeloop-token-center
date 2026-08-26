# Semantic execution metadata

MemeLoop Token Center combines two deliberately separate evidence classes:

1. protocol and Merkle-prefix evidence groups compatible requests and records
   confirmed continuation, retry, edit, branch, compaction and subagent edges;
2. downstream applications may declare human-readable execution metadata for
   audit and visualization.

Declared metadata never grants access, changes routing, merges tenants, affects
billing identity or upgrades a low-confidence candidate edge. Missing values
stay missing. The service does not ask an AI model to invent a session name,
agent role or task classification from prompt contents.

## Request contract

Every supported `/v1/*` text request may send these optional headers:

| Header | Meaning |
| --- | --- |
| `X-MTC-Session-Name` | Human-readable session name |
| `traceparent` | W3C trace id and caller span id |
| `X-MTC-Trace-Id` | Explicit trace override for non-W3C clients |
| `X-MTC-Span-Id` | Current downstream application span |
| `X-MTC-Parent-Span-Id` | Parent span override |
| `X-MTC-Agent-Id` | Stable agent instance or role identifier |
| `X-MTC-Parent-Agent-Id` | Parent agent identifier |
| `X-MTC-Task-Kind` | Bounded type such as `interactive` or `background` |
| `X-MTC-Session-Labels` | JSON object containing at most 16 string pairs |

Equivalent values may be placed under request `metadata` using snake-case
names. Header values take precedence. Label keys are at most 64 ASCII
alphanumeric/`_`/`-`/`.` characters and values at most 128 non-control
characters. Keys resembling credentials, authorization, cookies, passwords,
secrets or tokens are discarded. The entire label header is capped at 2 KiB.

Example from a TypeScript downstream application:

```ts
const response = await fetch(`${gateway}/v1/responses`, {
  method: 'POST',
  headers: {
    authorization: `Bearer ${clientCredential}`,
    'content-type': 'application/json',
    'x-codex-session-id': session.id,
    'x-mtc-session-name': session.name,
    'x-mtc-turn-id': turn.id,
    'x-mtc-parent-turn-id': turn.parentId,
    'x-mtc-agent-id': agent.id,
    'x-mtc-parent-agent-id': agent.parentId,
    'x-mtc-task-kind': session.background ? 'background' : 'interactive',
    'x-mtc-session-labels': JSON.stringify({ workflow: 'release', app: 'fulfilment' }),
    traceparent,
  },
  body: JSON.stringify({ model: 'gpt-5', input }),
});
```

## Codex coverage

Current Codex-compatible traffic is incorporated as far as evidence permits:

- `X-Codex-Session-Id`, Responses `previous_response_id`, turn/parent headers
  and message-prefix Merkle nodes preserve session and ancestry evidence;
- compaction and explicitly marked subagent turns create typed edges only after
  stable-key, principal, tenant and temporal parent checks;
- when a Codex wrapper can add the headers above, the same observations also
  carry names, W3C trace context, agent ancestry, task kinds and application
  labels;
- older traffic without those declarations remains visible in the request
  timeline and relationship graph but is not assigned fabricated semantics.

The session detail API returns declared execution metadata alongside each
request. It also returns a separate `structure` projection containing bounded
client/protocol evidence: explicit session, turn, parent turn, upstream
response, branch, compaction and client name. `structure.source` is
`client_protocol`; it is never labelled as a human declaration. Confirmed and
candidate prefix-tree relations remain separate edges with their own evidence
and confidence.

The web console combines those evidence classes without collapsing them:

- solid lanes are client-declared agents/tasks; dashed lanes are protocol or
  confirmed-prefix-tree structure whose human meaning is unknown;
- the aligned request timeline shows wall-clock start and elapsed time;
- the execution-duration flame view uses declared agent ancestry or confirmed
  request edges for depth and request elapsed time for width. It is explicitly
  not represented as a CPU-sampling flame graph;
- the task pie includes every visible request and retains missing types as
  `Unclassified`; and
- per-agent and per-task costs use the authoritative billed request fact and
  always split currencies.

This is deliberately analogous to `Server-Timing`: the application can report
semantics that only it knows, while the gateway validates, stores and visualizes
them without letting those diagnostics alter authorization or billing.

## Codex CLI configuration

Codex custom model providers support an API `base_url`, an environment-backed
credential, environment-backed HTTP headers and static headers. A user-level
Codex configuration can therefore attach one stable semantic context to every
request in a Codex process without placing secrets in the file:

```toml
model_provider = "memeloop-token-center"

[model_providers.memeloop-token-center]
name = "MemeLoop Token Center"
base_url = "https://token-center-api2-trial-portal.k3s.onetwo.website/v1"
env_key = "MTC_CLIENT_CREDENTIAL"
wire_api = "responses"

[model_providers.memeloop-token-center.env_http_headers]
"X-Codex-Session-Id" = "MTC_SESSION_ID"
"X-MTC-Session-Name" = "MTC_SESSION_NAME"
"X-MTC-Agent-Id" = "MTC_AGENT_ID"
"X-MTC-Parent-Agent-Id" = "MTC_PARENT_AGENT_ID"
"X-MTC-Task-Kind" = "MTC_TASK_KIND"
"X-MTC-Session-Labels" = "MTC_SESSION_LABELS"
"traceparent" = "MTC_TRACEPARENT"
```

The launcher or host application should set those variables immediately before
spawning each root or child Codex process. This makes root/child agent IDs,
interactive/background type, workflow labels and trace context explicit. A
plain Codex invocation that cannot set per-session values still contributes
Responses parent IDs, prompt-cache/session hints when present, request timing,
cost and Merkle-prefix relations.

Codex also supports OTLP log, metric and trace exporters in user-level
configuration. Token Center does not currently pretend to be an OTLP collector:
model-provider requests are billable request facts, while Codex tool/turn spans
are non-billing execution events. A future collector adapter may ingest those
events into a separate execution-event table, but may join them to requests only
through a stable opaque session/turn/request/trace identifier. Raw prompt export
must remain disabled (`otel.log_user_prompt = false`) unless a separate explicit
content-retention approval exists. See the official
[Codex configuration reference](https://learn.chatgpt.com/docs/config-file/config-reference).
