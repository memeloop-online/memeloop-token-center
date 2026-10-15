Default text body readers with an actual shared memory reservation use byte
admission instead of the global reader count. The existing route class and
reservation select this behavior; it introduces no model or request allowlist.
Today this covers chat completions, alpha search, embeddings, messages and
message token counting. Before polling the body, the reader still reserves the
route maximum multiplied by the existing request memory weight, then refunds
unused bytes. Body limits, cancellation cleanup and subsequent JSON/response
memory admission are unchanged.

The gateway body-reader metric counts actual admission/read owners with an RAII
guard. It includes readers that do not take a count permit and releases on
completion, rejection or cancellation. The count semaphore remains independent
of this measurement.

Responses and compact readers with a shared reservation also bypass the count.
Before opening a spool or waiting for input, they charge request-part structure,
URI and header bytes, capture/spool structures, file wrappers and the bounded
64 KiB write copy. The same shared budget charges each yielded frame's bytes
until its writes complete. Charges use the existing 64 KiB budget units, not a
new request count. Metadata stays owned by the spool; independently owned byte
permits cannot be released by subsequent request-reservation refunds. Blocking
open/write tasks retain their permits even if the request future is cancelled.
Disk-byte admission and body size checks remain independent and unchanged.
Local file-descriptor/I/O exhaustion retains the request_spool_unavailable
classification, with OS error evidence, rather than claiming upstream overload.

Images, Audio and asynchronous generation readers retain their existing count
behavior in this change. Completing actual multipart/upload/media response
resource accounting, then removing redundant count admission, remains an ID69
engineering task; their current counts are not a permanent design decision.
Do not replace them with another guessed request/file count.

This change preserves authentication, tenant policy, journal recording,
settlement and all byte/disk budgets. It adds no upload timeout and does not
lower concurrency. Validation runs through normal GitHub Actions.

Focused ownership tests use synthetic 64 KiB and 16 KiB frames corresponding to
the current HTTP/1 read-buffer bound and locked Hyper HTTP/2 default. They check
metadata/first-frame budget boundaries, dishonest Content-Length, and leases
held by paused blocking open/write tasks after caller cancellation. These helper
tests do not exercise the wire transport or prove pre-poll coverage of arbitrary
Body producers, downstream replay cancellation, or total process RSS. Changes
to transport bounds or frame coalescing require revisiting that assumption.
