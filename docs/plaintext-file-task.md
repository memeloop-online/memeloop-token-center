# Plaintext Kimi and DeepSeek file tasks

This ordinary Chat API runner lets a model inspect and edit files in an explicitly
chosen isolated directory. It is not a desktop native child, a gateway adapter or
an opaque-envelope decoder. OpenAI reserved collaboration schemas remain unchanged.

```console
export MTC_TASK_DIRECTORY=/absolute/path/to/isolated-worktree
export MTC_TASK_ARCHIVE=/absolute/private/path/task-receipt.json
export MTC_TASK_MODEL=k3.1-pro # or an authorized deepseek-v4.1-flash route
node scripts/maintenance/plaintext-file-task.ts < task.txt
```

Set the existing authorized `MTC_API_KEY` in the environment. `MTC_BASE_URL`
optionally selects a gateway including `/v1`. Never put credentials in task text.
The tool set is `list_files`, `read_file`, and `write_file`; no shell, execution,
network or test command is exposed. Parent directories must already exist.
The runner continues through actual tool calls and results until a final model
response. It sets no artificial output cap or total deadline, and does not retry
a failed request. Only complete successful tool-call turns execute file tools.
Each tool call keeps its provider ID and complete argument string; each result
is returned with the matching ID. Thinking-provider protocol context is replayed
only in process memory and excluded from printed results and local receipts.

Every API turn goes through normal MTC task/response archiving and accounting.
The private JSON receipt records original task, visible assistant output, tool
arguments/results, per-turn request IDs, HTTP status, usage and terminal state.
Inspect actual routing through those IDs; a requested model name does not prove
which account was selected. A provider error, missing stream terminal or `length`
finish is not a completed task. The receipt is not a resumable execution journal:
do not restart it as an automatic retry after an uncertain outcome.

Use a private isolated directory without concurrent external writers. File paths
are confined to it, including existing symlink checks; this is not an OS sandbox.
Review the resulting diff before moving edits to a shared checkout. The receipt
contains full task and file contents and should stay outside that workspace so
the model cannot overwrite its own evidence.

Native desktop handoff remains separate: the audited local source is v0.154,
not proof of current desktop 0.161 dispatch; the installed shell CLI may differ
again. No currently integrated host producer/dispatch entry point has been proven
to deliver complete plaintext to a real third-party native child. Offline alias
experiments alone do not establish that behavior or support durable replay.
