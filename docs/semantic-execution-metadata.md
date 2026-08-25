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
request. The web console renders agent-duration lanes and a request-count task
distribution. Request cost and currency stay attached to the same request, so
future cost pies/flame aggregations can reuse authoritative billing facts
without reparsing prompts or mixing currencies.
