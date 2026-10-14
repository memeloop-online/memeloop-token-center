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

Responses spooling, Images, Audio and asynchronous generation readers retain
their existing count behavior in this change. The shared byte reservation is
not yet complete accounting for every upload-phase metadata, live frame, file
descriptor, multipart copy or media response allocation. Completing actual
resource accounting for these paths, then removing redundant count admission,
remains an ID69 engineering task; their current counts are not a permanent
design decision. Do not replace them with another guessed request/file count.

This change preserves authentication, tenant policy, journal recording,
settlement and all byte/disk budgets. It adds no upload timeout and does not
lower concurrency. Validation runs through normal GitHub Actions.
