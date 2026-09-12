# Request attempt budget observability

The proxy emits one `request_attempt_budget_snapshot` info event when it freezes
the primary prepared candidate's request-local budget, before reservation and
archive work. This is policy selection evidence, not proof of admission,
dispatch, healthy capacity, successful delivery, or billing.

The event contains request, route and upstream-account UUIDs, transport revision,
credential generation, policy version, candidate-attempt limit, and deadline
duration. `policy_source` is one of `host_default`, `codex_default`, or
`account_transport_policy`. The last value means an account policy object was
present; omitted fields within that object can still use defaults. A disabled
deadline is represented by `failover_deadline_enabled=false` and duration `0`,
not an immediately expired deadline. These are structured log fields, not
unbounded metric labels. No endpoint, model name, proxy address, credential,
request body, raw configuration, or plugin output is logged.

The deadline is the original selection/send budget, not the successful stream
lifetime. It starts when the budget is frozen and is not replenished by a new
candidate, transport reload, or subsequent account-policy edit. New requests can
observe new account settings. Existing terminal events distinguish attempt
exhaustion from the original deadline. The snapshot does not describe every
candidate's connection retries or imply that global breaker timing is dynamic.

Route-group grants authorize routes; they do not grant arbitrary accounts or
models in a group. Candidates remain bound to the requested public model and
protocol, active credentials, and explicit eligibility. Account hints and
plugin preferences only order host-authorized candidates. A preference cannot
expand authorization or override the frozen request budget.

Cooldown and replay permission remain separate decisions: a dispatched HTTP
503 can affect health without authorizing another execution. This diagnostic
event changes neither the existing 429 failover rules nor the no-replay boundary
for ambiguous delivery, visible output, or dispatched 503 responses.
