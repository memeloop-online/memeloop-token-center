# Isolated native plaintext collaboration experiment

This is an offline, single-turn prototype. It is not imported by the gateway,
does not send requests, does not execute tools and is not enabled for customers.

The real-provider experiment on 2026-11-10 accepted the original collaboration
schema (HTTP 200/completed). Removing its three encrypted message markers
returned HTTP 400: `collaboration.followup_task` is reserved and must match the
configured schema. PR827 restored the reserved schemas. Do not repeat that rewrite.

The locally audited Codex `rust-v0.154.0` source gives a separate useful contract:
`core/src/tools/router.rs` marks a collaboration message call as
`DirectPlaintextMessage` only when `encrypted_function_args` is explicitly `[]`.
`core/src/tools/handlers/multi_agents_v2.rs` then constructs a readable task;
otherwise it stores the message as encrypted content. These facts do not prove
the current host version 0.161.0 has identical behavior. The three schemas use
`with_encrypted()` in `multi_agents_spec.rs`; changing the configurable namespace
alone does not change that dispatch rule.

The prototype preserves every original tool and all history. It adds a separate
`mtc_plaintext_collaboration_v1` namespace containing copies of the three task
tools, with plaintext message schemas. Only fresh calls from that alias can be
restored to `collaboration` with an empty encrypted-arguments marker. Existing
reserved calls and opaque content pass through unchanged. IDs, call IDs and
argument string values stay identical.

```console
node scripts/maintenance/native-plaintext-collaboration-prototype.ts request < original-request.json > experimental-request-and-context.json
```

Request mode returns `{request, context}`. Only the request field is a provider
payload; retain that same context for the response. Response mode takes
`{response, context}`; sse mode takes `{sse, context}`. These modes are offline
and never deliver their output to a host automatically. The same functions are
library entry points for an isolated test harness.
SSE conversion takes a fully captured successful stream, confirms complete
alias arguments, and changes added/done/completed item metadata consistently.
Argument delta/done frames remain byte-for-byte identical. Failed, truncated or
inconsistent captures remain unchanged. No live streaming support is claimed.

Before any host integration, independently review this experiment, verify that
the actual provider accepts the added alias, and verify a complete plaintext
parameter value. Then test the current host receiving the restored call and
observe the full task in a real child request and its archive. A provider 200
alone is insufficient. No tools should execute during the provider-only step.

Replay is deliberately unsupported. A later adapter needs provenance that
survives host persistence to distinguish its own alias calls from other
plaintext calls. Seeing `collaboration` plus `[]` alone is not that provenance.
The prototype does not rename old calls, invent decryptions, remove validation,
change usage/finish/error semantics or claim native child compatibility is fixed.
