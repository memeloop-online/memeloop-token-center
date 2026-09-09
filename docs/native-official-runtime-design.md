# Native official-runtime migration, first implementation slice

This slice is **not activated** and is not a migration receipt. No source
credentials were read, no account state was captured, and no supplier request,
login, refresh, quota preparation or reset was executed.

## Verified source, not an assumed OAuth REST adapter

The installed provider plugin is pinned to
`linonetwo/cpa-copilot-cursor@324c84e669c9959ce297358b0242ee74b8689529`
(`v0.2.0-rc.16`). Its
[store](https://github.com/linonetwo/cpa-copilot-cursor/blob/324c84e669c9959ce297358b0242ee74b8689529/internal/bridge/store.go)
holds only account handles in auth JSON. The actual login state is in the
separate provider/handle/home directory on its dedicated PVC. Thus six exported
main auth files do not prove six usable native credential migrations.

Its [runtime](https://github.com/linonetwo/cpa-copilot-cursor/blob/324c84e669c9959ce297358b0242ee74b8689529/internal/bridge/runtimes.go)
uses the official Copilot SDK `v1.0.8` and CLI account state, not the existing
MTC GitHub device-token REST adapter. Its
[Dockerfile](https://github.com/linonetwo/cpa-copilot-cursor/blob/324c84e669c9959ce297358b0242ee74b8689529/Dockerfile)
pins Copilot CLI `1.0.73` and an archive SHA-512 recorded in the native module.

The new code never calls that plugin, source sidecar, CPA endpoint, or generic
HTTP bridge. It speaks directly to a target-owned official executable over
child stdio. Source names remain documentary provenance only.

## Implemented code

- `native_cli/process.rs`: no shell, cleared inherited environment, fixed
  server-created executable/arguments, independent account home/workspace,
  concurrent limited diagnostics, deadlines, process-group termination and
  child reaping, including cancelled futures. Interactive protocol ownership
  supports SDK servers that intentionally do not exit between requests.
- `native_cli/copilot.rs`: official
  [Content-Length framing](https://github.com/github/copilot-sdk/blob/v1.0.8/go/internal/jsonrpc2/frame.go),
  exact protocol 3 handshake, fixed no-tools session configuration, send,
  root-session events and authoritative usage. Frame/header, total wire,
  prompt, event-count and output-size limits are independent. Unknown
  callbacks, tool/subagent activity, cross-session events, duplicate usage,
  mismatched model and absent accounting fail closed. The async output
  callback provides an archive acknowledgement/backpressure boundary.
- `native_cli/cursor.rs`: official account-specific model discovery and
  redacted status probe. Cursor generation fails before spawning because
  the verified source text/result contract supplies no authoritative usage.
- `native_cli/state.rs`: bounded encrypted CLI-state carrier using the existing
  v2 HKDF/ChaCha20-Poly1305 envelope. AAD binds tenant, target account UUID,
  provider and credential generation. The source handle is not an auth token.
  File-count/byte limits and traversal/duplicate/prefix checks are enforced.
  This module deliberately has no tar extractor or filesystem write API.

All tests use synthetic state, in-memory mock JSON-RPC, and harmless CI fixture
processes. Product builds/tests must run in GitHub Actions, not locally.

## Mandatory gates before production activation

1. **Capture:** root-reviewed metadata-only inventory of the separate PVC,
   then consistent owner-only sealed capture. Reject links, devices, sockets,
   unexpected ownership and changing files. Preserve a signed/hash-bound
   source-account inventory and do not retire the source state yet.
2. **State:** reviewed per-provider authentication-file allowlist. Never
   restore arbitrary CLI hooks, settings, plugins, startup files or executables
   merely because they were present in an account home. Carrier path validation
   alone is not that allowlist. Provision using directory-relative no-follow
   opens, owner-only modes, per-tenant/account directories and exclusive leases.
   Persist runtime refresh changes into a new sealed generation using CAS.
3. **Runtime packaging/isolation:** immutable hashes, no auto-update, a dedicated
   non-root sandbox worker with no gateway/operator/Kubernetes credentials,
   read-only runtime, a private per-account state mount, isolated ephemeral
   workspace, no host/shared files, and explicit supplier-only egress. The
   process supervisor is **not** an OS sandbox and must not run inside the
   gateway before this gate exists.
4. **Protocol/accounting/archive:** integrate normal gateway reservation,
   archive spool, settlement and cancellation owners exactly once. No guessed
   zero usage; unknown outcome stays explicit. The current Copilot contract
   intentionally accepts one root model call, not an unreviewed multi-tool
   agent run. Original chat/system/tool/image/conversation semantics require
   independently tested translation, not the source's prompt flattening.
5. **Model/quota UI:** account-specific authorized model catalog, normalized
   approved quota fields, freshness/unsupported status. A successful CLI
   status process does not prove OAuth usability or numeric available quota.
   Neither provider has an implemented reset action in this slice.
6. **Evidence:** CI mock protocol, process, cancellation, tenant isolation,
   generation-CAS and state round-trip gates; then separately authorized
   minimal target-owned account acceptance. No repeated expensive supplier
   requests, real resets, or hidden legacy fallbacks.

Outstanding packaging, state capture/provisioning, API routing, model database
integration and production acceptance mean this is a useful implementation
slice, **not completion of the full account/history migration**.
