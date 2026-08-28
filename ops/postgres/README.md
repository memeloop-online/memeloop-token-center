# PostgreSQL history partition backfill

`backfill-postgres-history-partitions.ts` repairs historical rows that landed in
`request_records_default` or `request_events_default` before their UTC daily
partitions existed. It is PostgreSQL-only; SQLite migrations and test storage
are not touched.

The command is deliberately safe by default:

- no option means a read-only dry run;
- mutation requires the literal `--apply` flag;
- only completed UTC days are accepted;
- one day per table is processed per invocation unless `--max-days` is raised;
- a PostgreSQL session advisory lock prevents two operators from moving the
  same table concurrently;
- detached staging tables survive interruption;
- copy work commits in bounded batches;
- count checks run after every batch;
- final count equality and bidirectional `EXCEPT` validate every row;
- the source delete and `ATTACH PARTITION` happen in one transaction, so
  readers see either the old default rows or the attached partition, never a
  partial state.

Before moving data, apply mode installs global/tenant/key newest-request and
global/tenant event-cursor indexes. New leaf indexes are built with
`CREATE INDEX CONCURRENTLY`; the short parent operation only attaches them to
partitioned-index metadata. These are operational indexes, not a new app schema
version, so application migration numbers remain available to application
features.

## Runbook

Set libpq variables without putting the password in argv:

```sh
export PGHOST=postgres.example.internal
export PGPORT=5432
export PGUSER=token_center
export PGPASSWORD='...'
export PGDATABASE=memeloop_token_center
```

Inventory the default partitions:

```sh
node ops/backfill-postgres-history-partitions.ts
```

Install/verify only the access-path indexes:

```sh
node ops/backfill-postgres-history-partitions.ts --apply --indexes-only
```

Move one oldest request day:

```sh
node ops/backfill-postgres-history-partitions.ts \
  --apply --table request_records --batch-size 5000 --max-days 1
```

Repeat until dry-run inventory is empty. An interrupted copy can be rerun with
the same arguments; existing staged identities are skipped and exact equality
is rechecked before cutover. Do not drop `_mtc_stage_YYYYMMDD` tables manually.

After a sizeable set of days has moved, schedule normal maintenance outside the
script so dead tuples in the former default partition are reclaimed according
to the deployment's I/O budget:

```sql
VACUUM (ANALYZE) public.request_records_default;
VACUUM (ANALYZE) public.request_events_default;
```

`VACUUM FULL` is intentionally not part of this procedure because it requires
an exclusive table lock. Use it only in a separately approved maintenance
window if physical file compaction is necessary.

## Lock and recovery model

The long copy phase does not delete source rows and uses small transactions.
The cutover takes `SHARE ROW EXCLUSIVE` on the default leaf: reads continue,
and writes routed to other attached partitions continue. A validated temporary
exclusion constraint allows PostgreSQL to attach the new daily leaf without a
second unbounded scan of the default partition.

If copy fails, committed staging rows and the state row remain. If the final
transaction fails, its delete, rename and attach all roll back. If the process
dies after commit, rerunning recognizes the already attached target and records
the operation as complete.

Operational status is visible without inspecting request bodies:

```sql
SELECT table_name, day_start, status, source_rows, staged_rows, moved_rows,
       completed_at
FROM public.mtc_history_partition_backfill_state
ORDER BY table_name, day_start;
```
