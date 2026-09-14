# Explicit context-compaction evidence

Request list/detail summaries and request SSE events add the optional nullable
field `compaction`. New responses return `true` only when the already-persisted,
key/session-owned conversation observation has an explicit compaction marker.
They return `null` otherwise. Older servers may omit the field. Consumers must
treat null/absence as unknown, never as “not a compaction”. Legacy zero/default
observations also remain unknown; `false` is not part of this wire contract.

This is client **context compaction**, not archive chunk compression, ZIP export,
large request inference or a duration threshold. A 287-second request without an
explicit marker remains unknown. Existing declared-header/metadata rules for
recording a marker are unchanged. Serving history never examines request bodies.

The projection reuses the existing bounded conversation-observation joins for
list/detail/event reads. It adds no migration, new write, per-row lookup, model
invocation, timeout or retry. Generation and orphan historical activity without
matching conversation evidence remain unknown. Existing session structure fields
are unchanged. UI consumers can adopt `compaction?: true | null` independently;
this change does not add a badge, filter or alter request ordering.
