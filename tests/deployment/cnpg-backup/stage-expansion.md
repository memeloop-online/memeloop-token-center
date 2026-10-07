# Bounded stage expansion planning

`stage-expansion.ts` produces a plan, not authorization or cluster mutations. Its
fresh inventory must include the dedicated stage's stopped Longhorn `engine`,
including its Volume owner, UID/resourceVersion, current size and complete
`status.snapshots` map with `volume-head`. No primary database engine is accepted.

Longhorn v1.12.0 creates a system snapshot during expansion. Its
[snapshot eligibility check](https://github.com/longhorn/longhorn-engine/blob/v1.12.0/pkg/controller/control.go)
checks available count and byte budgets; the
[total snapshot ceiling](https://github.com/longhorn/longhorn-engine/blob/v1.12.0/pkg/types/types.go)
is 250. The planner conservatively includes removed snapshots in its count and
includes the head in its byte accounting. It does not assume deleted snapshots
have been purged or permit expansion to bypass the existing byte ceiling.

When the count limit is full, the first planned patch reserves exactly one
additional slot on the same Volume, guarded by UID/resourceVersion, old capacity,
old count and unchanged snapshot-byte limit. StorageClass enablement and the PVC
request follow. With an available slot, those original two patches are unchanged.
The complete physical-space budget is unchanged, and no snapshot is deleted.

Before applying the ordered plan, re-read the same stopped engine and detached
Volume. A changed engine UID/resourceVersion or inventory requires a new plan.
After an accepted resize, do not shrink or restore a count limit below usage;
preserve every partial and investigate any incomplete reconciliation. Disable the
dedicated StorageClass's expansion capability during closeout with fresh identity
checks. No source SQL, backup retry, restore allocation or foreground concurrency
change is authorized by this planner.
