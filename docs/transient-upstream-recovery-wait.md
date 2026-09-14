# Waiting for transient upstream recovery

When an unsent request has exhausted its authorized candidates, it may wait for
one candidate whose current health failure is `unavailable`. Healthy standbys
and the existing connection-recovery path are attempted first. A request that
already consumed an outbound attempt never enters this wait. An explicit 503
or ambiguous delivery still stops that request and does not authorize replay.

The waiting deadline is frozen once before reservation/archive work: the earlier
of the existing request attempt deadline and the configured unavailable cooldown
plus one configured probe lease. The surrounding request-lifecycle deadline
also still applies. These reuse existing account transport policy and process
health configuration; process environment configuration is not hot reload.
Waiting never replenishes attempts or deadlines. A fixed four-waiter process
resource ceiling bounds retained request bodies; their original memory-budget
reservations remain charged and no body is copied. A full ceiling emits the
fixed `skipped/recovery_wait_capacity` health metric, without dynamic labels.

Every recheck validates the original transport revision and credential generation.
The primary database atomically grants the existing single probe lease. The wait
path refuses quota, authentication, rate-limit and invalid-response isolation,
including a health-kind change while waiting; its lease CAS restricts the failure
kind as well as generation. It does not clear cooldown or expand shared-probe
fanout. The 250 ms recheck cadence is an internal load bound, not a new retry
policy. No transaction spans a timer sleep. An owned database recheck retains
the waiter permit until completion, so caller cancellation cannot discard a
claimed lease or allow unlimited detached database checks. The permit is released
before upstream dispatch; the ordinary attempt guard takes over lease cleanup.

CI tests use a committed database transition as the real-route wait barrier,
check that no request is sent before that transition, and verify both recovery
and a concurrent quota isolation change. Paused-clock tests cover the deadline
and resource cap; explicit channels cover cancellation and late-result cleanup.
Database concurrency tests retain single-probe and generation fences. No local
compilation, production probes, state resets, or deployment are part of this PR.
