# Responses request spool ownership

Responses and compact Responses uploads authenticate before body polling and keep
the upload in process memory. Each request keeps its configured body-size
ceiling; invalid declared sizes are rejected before the buffer is retained and
received bytes are checked before copying. Uploads have no body deadline. The
raw buffer is released when the upstream returns its first byte. Terminal
conversation projection keeps the already parsed semantic JSON, not a second
copy of the upload, and not a copy for retry.

There is no aggregate logical disk-byte admission budget. The retired
`MTC_RESPONSES_REQUEST_SPOOL_BYTES` environment variable is no longer read and the
Helm `config.responsesRequestSpoolBytes` value has been removed. Remove that value
from chart overrides before upgrading; the chart rejects unknown config keys.
Existing environment injections have no effect. Neither declared lengths nor
retained source files can reject another upload against a synthetic byte cap.
Persisted `Config` JSON containing `responses_request_spool_bytes` still
deserializes through the existing unknown-field behavior. Rewriting that config
removes the retired key; no runtime compatibility field or startup rejection is
introduced. Helm override files require an explicit migration deleting
`config.responsesRequestSpoolBytes` before chart validation, rather than keeping
an exposed setting that no longer controls admission.

`memeloop_token_center_request_spool_bytes{measure="used"}` counts bytes
retained in memory for live uploads, including partial captures. It does not
count promised upload lengths or RSS. Active-buffer telemetry counts opened
uploads, including buffers retained by a blocking copy after cancellation. The
old synthetic limit and capacity-rejection metric series are removed.

The spool still charges metadata, a 64 KiB write-copy allowance, and each retained
transport frame to the existing proxy RAM budget. Each request awaits its
blocking write before copying another piece or polling the next frame. Replay,
JSON parsing and terminal projection retain their existing memory admission.
The existing RAM checks can still reject requests; fair RAM waiting, oversized
transport-frame allocation, and whole-process memory accounting are separate
work. This change introduces no new upload deadline or concurrency cutoff.

`requestSpool.mountPath` is only the writable directory for terminal journals.
Request bodies are not written there. The chart still caps that directory so a
journal cannot fill the node, and that cap is what kubelet enforces. It is not
an application admission budget for uploads. Memory admission remains the limit
on how much upload data a process may retain. The retained bytes are released
after the upstream returns its first byte, so a long generation does not keep
the raw upload.

The optional `gateway_archive` database pool is independent of ingress spooling.
Its bounded background capture can retain an archive gap on failure; this change
does not modify archive behavior or mandatory usage-reservation safety.

CI regressions cover declared and unknown-length uploads stalled for 120 seconds,
zero body polls and zero promised-byte telemetry while file opens are paused,
six retained 12 MiB spools exceeding the former 64 MiB aggregate default, new
admissions while those sources remain live, no ingress replay, actual-byte and
file cleanup, per-request size rejection, and ENOSPC/EDQUOT classification.
Existing frame-boundary, cancellation during blocking open/write, digest,
anonymous-file, symlink, proxy memory, archive degradation, and no-replay
regressions remain enabled. Local tests and builds are prohibited in this
workspace; validation runs in GitHub Actions.
