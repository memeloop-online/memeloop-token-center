# Isolated host plaintext delegation

This experiment owns a separate stdio `codex app-server` process. It never
attaches to a running desktop/root session and does not modify its configuration,
the installed binary, or reserved collaboration tools.

The actual installed CLI 0.167 schema generated on 2026-11-12 supports
`thread/start.dynamicTools`, `item/tool/call`, and ordinary `turn/start` text
input. The bridge registers one custom `delegate_plaintext_kimi` function on a
new ephemeral parent thread. Its caller receives the complete task arguments,
starts a distinct ordinary Kimi thread with explicit plaintext text input, and
returns that thread's real visible result to the original dynamic call.
This is an explicit custom-tool bridge, not a fix to built-in collaboration's
encrypted producer. It does not decode or replay old opaque messages.

```console
export MTC_BASE_URL=https://gateway.example/v1
export MTC_TASK_DIRECTORY=/absolute/isolated/worktree
export MTC_TASK_ARCHIVE=/absolute/private/bridge-receipt.json
node scripts/maintenance/plaintext-host-bridge.ts < useful-task.txt
```

Use an existing authorized downstream `MTC_API_KEY` in RAM. Optional settings:
`MTC_CODEX_BINARY` (default codex), `MTC_PARENT_MODEL` (default gpt-6.1-sol),
`MTC_TASK_MODEL` (default k3.1-pro). Both threads use the supplied authorized MTC
provider, read-only sandbox and no approval escalation; this does not grant
tenant or provider access. No model/request retries or artificial task deadline
are added. The owned app-server is terminated when the operation finishes.

The private receipt records exact task text, parent/child thread IDs, call IDs,
visible agent results and actual turn status. It excludes reasoning events and
credentials. Request/account/route and server archive evidence must be checked
separately through MTC; a local receipt does not prove complete server archives.
Before use, independently review the protocol and run its regressions through
GitHub Actions. A schema export alone does not prove model/provider acceptance
or a completed child task. Current desktop version must not be inferred from
this standalone CLI binary or the older locally audited 0.154 source.
