# Complete provider account editor: source and screenshot contract

## Baseline and deployment boundary

- Implementation baseline: `b02f8f2ef288cd83c4276b00a90c8204894449b7` (GitHub master API checked 2026-10-14 UTC).
- `9ca3dbf` was a descriptive shorthand, not a Git ref. The 2026-10-14 01:35 UTC control readback records image index `sha256:9ca66b6838bc59e7db088c2c0dfe4453bb8610443085e64e68485b582b7ca2a2`. Fresh registry reads, each checked against its raw SHA-256, resolve this to amd64 manifest `sha256:db33821bc0cece80051d42e82afeebc993ab9133869e8249c516969ac70447a5`, config `sha256:1262bde8a3097c9d98eba7b175ecacac517349e0c62168bdaf2352d5a4399520`, and OCI revision `3dbf18898357cd00ab33e8a79523d85dd6a08367`. A fresh read-only cluster query was refused; current production state is not inferred from master. No deployment, credential change or OAuth action is part of this PR.
- Both that image revision and baseline already include PR540 (`9df02cb0`): validated metadata-only rename receipts, name validation, and returning the confirmed account to the detail workspace. The editor composition, rename helper, identity, notes and layout CSS have no diff between those revisions. The reported production rename symptom is not explained by an undeployed PR540 fix; its root cause remains unproven. Do not reimplement that fix.
- PR603 remains frozen at `23ceef4b9e81074143113c22ab870e229a8aa67a`; none of its egress UI changes are included here. OAuth work in PR605/606 is separate.

## Actual composition and minimal change

Both account-detail Edit and reauthorization Rename enter the same `UpstreamProviders` settings workspace in `ManagementPages.tsx`. It composes `CreateJourney`, the existing RJSF profile/connection fields, `UpstreamConnection`, authentication actions, routing, `ProviderModelCatalog`, and `UpstreamNotes`.

The previous composition put the model catalog before account identity. A workspace-wide `type=button` rule also replaced Fluent primary/secondary appearances, including separately saved notes. Notes used a different boxed section treatment, and the page inherited the generic add-provider description.

The complete reading and keyboard order is now account identity/name → connection and its existing disclosures → authentication → routing → account save → model catalog → separately saved notes. The shared template applies to all provider settings entrances, not a Kimi-only parallel page. The existing Fluent button appearances remain authoritative; action rows share alignment and spacing. The heading uses the confirmed saved name, not an unsaved draft. The new description explains the separate save boundaries.

No changes to OAuth, credentials, account scope/generation fences, proxy saving, notes saving, dirty confirmation, mounted authorization lifecycle, backend APIs, or navigation handlers. The user's renewed authorization is never replayed or overwritten.

## GHA-only executable screenshot contract

Extend the existing `web/e2e/kimi-device-browser-contract.test.ts`; do not run tests, build or install locally. The `authorization-code.html?full-page&scope-controls&app-shell` fixture renders the existing AppShell and production provider page, with synthetic accounts and network interception. Unexpected non-rename writes fail the test; quota reads are synthetic too. The initial head's fixture rendered the complete provider component but omitted AppShell; those images were actually inspected and are not accepted as full application-shell evidence. The explicit shell option closes that acceptance gap without changing production code.

Artifacts use the existing CI upload path `web/e2e-artifacts/ui-system/account-workspace/`:

| Evidence | Required checks |
| --- | --- |
| `kimi-rename-{zh-CN,en}-{320,390,1440}-{light,dark}.png` | Full page, not isolated cards: profile, endpoint/proxy, authentication, routes/catalog, notes and final actions; no horizontal overflow; every visible button has a usable target and stays inside the viewport; section and DOM order asserted. |
| `kimi-save-conflict-{zh-CN,en}.png` | Failed save keeps draft and actionable error; no success or OAuth mutation. |
| `kimi-saved-heading-{zh-CN,en}.png` | Confirmed name replaces old heading even when the following list response returns the older account revision; credential generation/config/expiry preserved. |
| `kimi-reopened-heading-{zh-CN,en}.png` | Detail → reauthorization → Rename reuses the confirmed name in heading and input. |

The same test retains invalid/overlong-name validation, dirty-discard cancellation, focus return, metadata-only payload assertions and late-save tenant-switch isolation. It intentionally uses an expired synthetic account to detect accidental authorization changes; it does not describe the real user's current authorization.

Existing GHA contracts remain required: `form-mobile-navigation-browser-contract` (full shell, proxy editing and independent-save locks), `provider-edit-real-shapes-browser-contract` (Codex/shared schema, unknown fields and disclosures), `provider-proxy-draft-browser-contract` (permission, empty, slow/error, saved/list-refresh failure and long names), and the existing authorization lifecycle contracts.

## Independent review and reporting

Report exact PR head and actual GHA run separately. A source PR or a screenshot filename is not evidence of green CI or visual acceptance. After GHA produces artifacts, inspect the complete screenshots, including the bottom notes/actions, at mobile and desktop widths. Hand off to the independent Kimi reviewer for layout, button hierarchy, copy and rename-context review; do not count author's CI reruns as reviews. At most a necessary second review round. Kimi service 503 failures have produced no independent R1 and are not passes. The separate 0142 generation/automatic-refresh investigation belongs to Kepler, with UI state-selection findings handed off separately; quota-hook work belongs to its separate owner and is not part of PR607.
