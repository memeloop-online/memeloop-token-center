# Optional routing observations follow-up

Parent: PR #423 at `b116e25afd8a5d335e02455dec3d662eeb52a9b2`.
This follow-up removes two optional waits from request preparation and SSE frame
delivery. It does not establish complete storage isolation.

## Session preference

The forwarding path performs a nonwaiting lookup in a process-local cache.
Tenant, principal, key, explicit session, model and protocol are all included in
the length-delimited cache identity. Explicit policy account hints still bypass
this preference. The authorized candidate query and all admission/security checks
remain mandatory; cached values only influence ordering within those candidates.

The cache holds at most 1024 fixed-size entries with a one-second freshness
interval. Cold, expired, oversize or contended lookups return no preference and
may schedule a refresh. Four permits bound refreshes including running work;
there are no waiting submitters. Each refresh owns three strings of at most 1024
bytes and three UUIDs, rather than a cloned authentication policy or request body.
The existing 50 ms read deadline applies only inside the background refresh.
Expired entries are reclaimed on capacity pressure; failure never serves stale
entries. Local stream terminals publish before EOF without SQL, and their version
tokens prevent an older in-flight refresh from overwriting them. Terminal timestamp
and request UUID order prevent an older local terminal from replacing a newer one.
Successful buffered settlement invalidates its cached hint, including any pending
refresh. No failed or duplicate settlement publishes a replacement terminal.

This is an advisory cache, not a cross-replica read-after-write guarantee. Cold
requests, immediate buffered retries, cache contention and saturation can use
ordinary route ordering. Other replicas observe persisted evidence on refresh;
fresh cached values can lag new remote evidence by up to one second. Session SQL
publication and financial finalization still exist after streamed HTTP EOF.

## Validated-output health

Only a protocol-validated billable frame that has passed the existing mandatory
delivery prepare/confirm CAS and been enqueued downstream may submit early
recovery. Healthy attempts retain their original terminal epoch; active plugin
policies that defer delivery recovery retain that behavior.

Sixteen permits bound submitted/running health jobs. Submission uses try-acquire
and allocates no waiting task on rejection. Each admitted task owns fixed identity
fields, database/metrics handles and one result channel, with no request body or
HTTP sender. SQL preserves the existing account, credential-generation, transport
revision and probe-token fences, including active-account checks. Queue rejection
conservatively leaves recovery to the existing terminal health owner; it never
skips mandatory financial, authorization, delivery or terminal health transitions.

The guard consumes an admitted recovery acknowledgement before conclusive health
publication or lease abandonment. Cancellation while waiting keeps the receiver
in the guard; its existing cancellation owner consumes it before cleanup. This
orders early recovery before terminal health and prevents late acknowledgement
from racing a release. The heartbeat remains owned while acknowledgement is
pending. Newer generations, transport revisions and health epochs reject stale
SQL. No application timeout cancels an accepted health write; capacity remains
charged until SQL returns. Existing database timeouts still apply. A SQL error
does not claim recovery or replay an upstream request.

Health jobs and session refreshes share a separate lazy two-connection observation
pool, independent of mandatory forwarding and archive pools. They still share
database locks, CPU, storage and process scheduling. Stalled health work can retain
terminal lifecycle ownership after EOF. Existing terminal/drop ownership tasks
are preserved, not replaced by an unbounded publication retry mechanism.

Fixed-label metrics report health jobs/rejections and session refresh
jobs/rejections/failures. Recovery metrics require a positive fenced SQL result.

## Validation and integration

GitHub Actions only; no local builds or tests. Added contracts exercise real
stream EOF and exactly-once settlement with the observation pool occupied/closed
and all health slots held, rejected futures never running, terminal convergence,
stale health epoch rejection, scope isolation, and local-terminal/refresh ordering.
Existing long-stream probe admission, malformed output, billing and generation
fence contracts remain required.

No migrations are reserved. Terminal-cause ownership remains separate: migration
0114 belongs to PR #422; 0115 belongs to terminal cause. This patch does not edit
the winning terminal SQL update, its cause bind 19, models, query/conversation
projections, session analytics or UI. When integrating those branches, preserve
the nullable allowlisted cause in the same `completed_at IS NULL` CAS.

Request filesystem spooling still needs an independently bounded source-memory
architecture. Shared financial/event/statistics locks and slow finalizer ownership
remain unresolved. Neither this patch nor green focused CI proves the full invariant.
