# Shared quota observations

Group-routing plugins may opt into `group_routing_quota`. The worker samples
only accounts on enabled routes attached to groups bound to one of those
currently installed plugins. With no opt-in bindings it makes no quota calls.
Each round pins the current application-plugin revision.

Sampling uses the existing native quota adapter and the account's network proxy.
It does not refresh OAuth credentials, execute models, or reset quotas. Database
leases prevent concurrent workers from sampling the same account. The lane has
at most four concurrent reads and can be cancelled independently during shutdown.

Worker settings (environment variables, effective after worker restart):

| Variable | Default | Accepted effective range |
| --- | --- | --- |
| `MTC_QUOTA_OBSERVATION_INTERVAL_MILLIS` | 10000 | 1000–60000 |
| `MTC_QUOTA_OBSERVATION_BATCH_LIMIT` | 4 | 1–16 |
| `MTC_QUOTA_OBSERVATION_TIMEOUT_MILLIS` | 10000 | 1000–30000 |

Values outside these ranges are clamped. The existing quota cache can reuse a
fresh supplier read; this interval is not a promise to call the supplier every
tick. Larger batches are scheduled fairly by oldest sampling attempt first.

Migration 98 stores only credential-free observations and refresh leases. Every
observation is bound to tenant, account, credential generation, and the account's
transport/configuration revision. Failed reads clear shared evidence. Freshness
ends at the supplier snapshot's stale deadline or the earliest window reset,
whichever comes first. Unknown amounts remain null, never “healthy” or zero used.

Gateway requests perform a single bounded batch database read, never a quota
network call. The read shares the existing group-routing JSON budget; exceeding
it reports `quota_observation_payload_limit` and uses native routing. Disabled,
rotated, reconfigured, stale, and reset-passed accounts cannot reuse old evidence.
Quota input is disclosed only to opted-in plugins and only for already authorized
candidates. Native health and authorization constraints remain in force.
