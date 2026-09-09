# Native Cursor migration: verified source contract

Status: source contract verified; native runtime and account migration **not complete**.
An account pointer is not an OAuth credential. Do not create an enabled account
from the source auth JSON, guess an access token, or route it through an
OpenAI-compatible HTTP transport.

## Exact source

- Core: `linonetwo/CLIProxyAPI`, `v7.2.128-onetwo.1`,
  commit `9f15efc8c7a34598b31c1d02781ff75247764ab8`. Its complete Git tree
  has no Cursor executor. Cursor is supplied by a separately deployed plugin.
- Plugin: `linonetwo/cpa-copilot-cursor`, `v0.2.0-rc.16`,
  commit `324c84e669c9959ce297358b0242ee74b8689529`.
- The existing GitOps deployment pins its image to
  `sha256:1bc600741c7266a23a1567a2fc19b3708e0b4476eacbe9b5df1637e19d7c10e5`.
- The plugin Dockerfile pins official Cursor agent
  `2026.07.23-e383d2b`, archive SHA-256
  `702ad595213bee5df0268be9f80a19f29fcceaa2a42fc55e39f2b5199051f0c4`.

Primary source files at the immutable plugin commit:

- [Account storage](https://github.com/linonetwo/cpa-copilot-cursor/blob/324c84e669c9959ce297358b0242ee74b8689529/internal/bridge/store.go)
- [Official runtime integration](https://github.com/linonetwo/cpa-copilot-cursor/blob/324c84e669c9959ce297358b0242ee74b8689529/internal/bridge/runtimes.go)
- [Runtime packaging](https://github.com/linonetwo/cpa-copilot-cursor/blob/324c84e669c9959ce297358b0242ee74b8689529/Dockerfile)

## What must actually move

The source `AuthRecord` contains `type: "copilot-cursor"`, `upstream:
"cursor"`, `handle`, `label`, `login`, and `created_at`. It contains neither
an access token nor a refresh token. The handle identifies
`/data/cursor/<handle>/home` on the independent
`cpa-copilot-cursor-data` PVC. Moving the main auth file alone does not
migrate the login. A configured PKCE endpoint in MTC does not repair this gap.

Capture the selected account's actual CLI state from this PVC into an
owner-only, encrypted migration package. Enumerate metadata first, reject
symlinks/hardlinks and escaping paths, enforce file/count/size bounds, and
retain exact source-to-destination identity attestations. Never print file
contents, tokens, cookies, or a plaintext credential map. Treat the complete
home as sensitive until its format is independently classified.

The target must own the runtime and encrypted account state independently:
no old plugin HTTP endpoint, old sidecar dependency, or source name in the
new provider identity. Original source identity belongs only in the private
migration provenance record. The ordinary provider label is `Cursor`.

## Real protocol, not assumed REST compatibility

| Function | Source implementation | Required target behavior |
| --- | --- | --- |
| Login/state refresh | Official CLI `login`, account-specific home | Preserve/import actual state; official runtime owns its refresh behavior |
| Models | Official CLI `--list-models` | Discover for that exact account; do not claim hardcoded IDs prove access |
| Quota | Official CLI `status` | Display observed status and timestamp; source supplies no numeric reset contract |
| Generation | Official CLI print/ask in an isolated temporary workspace | Native subprocess integration, bounded output, cancellation, isolated account state |
| Streaming | Source buffers final text and synthesizes chunks | Do not describe this as native token streaming or invent usage |

The source's command uses `--print --trust --mode ask --workspace <temporary>
--output-format text`, optionally `--model <id>`. Blindly reproducing
`--trust` inside the gateway is not acceptable: the official agent must not
gain access to operator credentials, repository files, shell tools, or other
accounts. Use a dedicated restricted runtime process/container with a
minimal environment, no inherited service credentials, isolated working
directory and per-account state, and explicit network policy. Prompts must
not become executable shell input or unbounded log/argument output.

The source has no Cursor quota reset implementation. Do not add or trigger
a reset action based on inferred capability.

## Remaining delivery gates

1. Metadata-only PVC inventory, consistent sealed account-state capture,
   source count/hash receipt, and protected restore into target-owned state.
2. Native official-runtime packaging with pinned hashes and no old source
   endpoint/configuration dependency.
3. Account and tenant isolation, concurrency/refresh ownership, cancellation,
   timeout/output limits, redacted failures, and secure state persistence.
4. Protocol translation contracts for chat, tools, conversations, streaming,
   and authoritative accounting. Unsupported shapes must fail before a
   provider call; text-only flattening must not silently lose information.
5. Account-specific model discovery and quota status UI, with timestamp and
   explicit absence of numerical usage/reset data.
6. CI mock-runtime tests for all preceding gates, then separately authorized
   minimal runtime acceptance without quota reset or repeated expensive
   requests. Preserve the source PVC until state and histories are verified.

No source credential was read and no real supplier operation was made during
this source audit. This document is not an implementation or migration receipt.

## Implemented building blocks (not activated)

`src/native_cli/process.rs` implements server-owned command execution with a
cleared environment, stdin input, concurrent bounded stdout/stderr, a deadline,
group-wide cancellation, and explicit child reaping on ordinary error/success.
Group termination precedes reaping to avoid signaling a recycled PID.
Errors never include stderr or command arguments. CI-only process fixtures
exercise stdin, output bounds, environment isolation, failure and timeout.
Tests were authored but not run locally.

`src/native_cli/cursor.rs` pins the official binary/script paths and implements
model discovery and a status probe. It neither substitutes `auto` on failure
nor exposes raw status text. A successful status command is **not** proof of a
valid login or numerical quota. Model IDs are deduplicated and bounded.

Generation currently fails before launching any process because the verified
source and documented final-result contract supply no authoritative usage.
No route/adapter is registered and no production account is activated by these
modules. They are building blocks for the dedicated isolated runtime, not
permission to launch a tool-capable official agent inside the gateway.
