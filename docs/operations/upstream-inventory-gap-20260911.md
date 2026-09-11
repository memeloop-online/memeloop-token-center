# Native upstream inventory gap — 2026-09-11

Status: **open P0 migration gap; this is evidence, not a migration receipt.**

## Read-only production finding

An authorized, read-only query against the PostgreSQL replica counted three
active upstream accounts in the production tenant:

| Driver / authorization | Active account count | Result |
| --- | ---: | --- |
| Native OpenAI Codex OAuth | 2 | Present |
| HTTP JSON | 1 | Present; the display label is `广电国产自部署模型` |
| Native Kimi OAuth | 0 | Missing |
| Native Copilot OAuth | 0 | Missing |
| Native Cursor OAuth | 0 | Missing |

The inventory and route lookup found one Kimi-named public route,
`kimi-k2-thinking`, but it targets the single HTTP JSON account.  Historical
Kimi traffic or an HTTP route is not evidence of a native Kimi OAuth account.
No active account or route with a Copilot or Cursor identity was found.

The query ran with PostgreSQL's `default_transaction_read_only=on`.  It did not
retrieve credentials, configuration values, quotas, source auth files, or send
an upstream request.  This document deliberately records counts and provider
classes rather than account emails, source filenames, opaque configuration, or
credential material.

## What the Overview does and does not prove

`主要上游模型` is a bounded *traffic* projection: it ranks completed
account/model pairs in the selected window.  It is not an account catalog and
cannot show accounts that have no routed terminal traffic.

The current source groups those pairs by `upstream_account_id`, then displays
the models within that stable identity; equal display names never merge two
accounts.  The production inventory above contains only one account whose
display name is `广电国产自部署模型`.  Thus a repeated label in a stale client or
older deployed image is not evidence for inventing, merging, or deleting a
second account in MTC.  The correct next check is the rendered release revision
and its snapshot payload's stable account IDs.

## Required migration receipt before CPA retirement

1. Obtain an owner-approved, encrypted source inventory with stable source
   identities and counts for Kimi, Copilot, and Cursor.  Do not copy bridge
   labels, `cpa-` names, credentials, or runtime homes into the normal account
   name field.
2. Use the native target import path for each provider and preserve a
   source-to-target identity/provenance receipt.  A source auth file by itself
   is insufficient for Cursor because its runtime state is account-specific.
3. Re-run this exact redacted read-only inventory after each import, reconcile
   every migrated target account to a native driver/OAuth identity, and record
   routes, history references, and validation status.  Do not infer successful
   migration from a model string or a dashboard card.
4. Only after the receipt proves all intended identities and history/config
   references have been reconciled may the remaining CPA runtime be retired.

Related source-boundaries: [`native Kimi OAuth`](native-kimi-oauth.md),
[`native Cursor source migration`](../native-cursor-source-migration.md), and
[`native official-runtime design`](../native-official-runtime-design.md).
