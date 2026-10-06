# Actual upstream price coverage preflight

This independent prerequisite for PR474 (ledger 36) is extracted from commit
3e1ac519. It adds no runtime admission checks, migrations or pricing writes, and
can be delivered on the existing schema117 before PR469/474. Current release602
continues using its existing rules; this tool checks coverage for the proposed
actual-upstream-model price lookup before it is deployed.

## Required stage and production handoff preflight

474 must not enter either stage or production until the actual target database
has a completed, zero-gap price coverage report from the exact reviewed source
revision. This is an operator-run read-only handoff prerequisite, not another
request admission gate. It does not authorize connecting to production now;
release 602 remains unaffected. The check works before migrations 0118/0119 and
does not run migrations. Migration order 469/0118 then 474/0119 remains mandatory.

From a reviewed checkout, use Node 24 and an existing PostgreSQL client with an
authorized SELECT-only role. Supply the approved target through libpq environment
variables `PGHOST`, `PGPORT`, `PGUSER`, `PGDATABASE` and `PGPASSFILE` (or the existing
approved authentication mechanism); never put secrets in command arguments or
the report. Run:

```text
node scripts/maintenance/check-routing-price-coverage.ts > price-coverage.json
```

The script takes no mutation option. It uses a repeatable-read, read-only
transaction, bounded statement/lock/connection/process timeouts, and rolls back.
It checks every direct and included-provider-group candidate of enabled,
non-archived text routes in active tenants with active accounts, respecting group
exclusions. This intentionally includes catalog-dormant and temporarily unhealthy
candidates, rather than testing just today's first choice or hiding failover gaps
behind credential expiry, account balance, catalog readiness, or key grants.
Generation pricing is a separate contract and is not certified by this check.

Currencies are the tenant's distinct key/account currencies, including inactive
keys/accounts to avoid a reactivation blind spot. Append `--currency USD` and/or
`--currency CNY` for planned currencies not yet represented in those records;
these additional currencies apply to every tenant in scope. Missing currency
scope or an empty candidate inventory is not a PASS. No currency conversion or
public-alias price fallback is performed. Exact upstream model matching,
uppercased currency, parseable price UUIDs and base/tier integer rates are checked;
negative rates are rejected conservatively even if the current reader decodes
them. Legacy base prices without tier rows remain valid.

Output includes total candidate/check/gap counts and at most 200 gap samples with
tenant/route/upstream-account IDs, currency, reason and whether a public-alias
price exists. It never prints model names, aliases, prices, credentials or request
data. Samples may truncate, but the full gap count never does. Exit 0 means
complete zero-gap coverage; exit 2 means gaps/unknown/empty scope; exit 1 means
the check failed and cannot certify coverage.

The stage/prod handoff must record the target environment identity separately,
reviewed commit, UTC report timestamp, exit status, complete gap count and report
artifact. Resolve each gap through explicit authorized pricing decisions and
rerun the read-only check; never copy public rates, invent prices, disable routes,
or rewrite historical snapshots to manufacture PASS. Any relevant route, group,
catalog membership, currency or price change invalidates the evidence: rerun
immediately before the handoff. Hold 474 if coverage is incomplete or stale.
Zero gaps certify only this inventory snapshot, not future configuration or the
independent CI, migration-order, financial/replay and stage verification gates.


## Verification boundary

The existing Rust GHA test job invokes the offline Node CLI contracts and the
real PostgreSQL/psql/Node regression against schema117 with read-only sessions.
Cases cover public-alias-only and failover gaps, exact model case, extra
currencies, malformed IDs/tiers, legacy base-only prices, group
inclusion/exclusion, empty scopes and missing currency scope. No local
build/test/install or stage/deployment acceptance is claimed. GHA acceptance
must refer to this PR's exact head, not historical PR417/469/474 runs.
