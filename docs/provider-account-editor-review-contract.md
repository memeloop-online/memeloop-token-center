# Complete provider account editor: source and screenshot contract

## Baseline and deployment boundary

- Implementation baseline: `b02f8f2ef288cd83c4276b00a90c8204894449b7` (GitHub master API checked 2026-10-14 UTC).
- The parent reports deployed control `9ca3dbf` and gateway `921…`. These are not evidence that master is deployed. GitHub's commit API cannot resolve `9ca3dbf` (422); an immutable control image-to-source receipt is still needed for an exact deployed-source diff. No production read, deployment, credential change or OAuth action is part of this PR.
- Baseline already includes PR540 (`9df02cb0`): validated metadata-only rename receipts, name validation, and returning the confirmed account to the detail workspace. Do not reimplement that fix or infer a new rename backend defect from the old deployed heading.
- PR603 remains frozen at `23ceef4b9e81074143113c22ab870e229a8aa67a`; none of its egress UI changes are included here. OAuth work in PR605/606 is separate.

## Actual composition and minimal change

Both account-detail Edit and reauthorization Rename enter the same `UpstreamProviders` settings workspace in `ManagementPages.tsx`. It composes `CreateJourney`, the existing RJSF profile/connection fields, `UpstreamConnection`, authentication actions, routing, `ProviderModelCatalog`, and `UpstreamNotes`.

The previous composition put the model catalog before account identity. A workspace-wide `type=button` rule also replaced Fluent primary/secondary appearances, including separately saved notes. Notes used a different boxed section treatment, and the page inherited the generic add-provider description.

The complete reading and keyboard order is now account identity/name → connection and its existing disclosures → authentication → routing → account save → model catalog → separately saved notes. The shared template applies to all provider settings entrances, not a Kimi-only parallel page. The existing Fluent button appearances remain authoritative; action rows share alignment and spacing. The heading uses the confirmed saved name, not an unsaved draft. The new description explains the separate save boundaries.

No changes to OAuth, credentials, account scope/generation fences, proxy saving, notes saving, dirty confirmation, mounted authorization lifecycle, backend APIs, or navigation handlers. The user's renewed authorization is never replayed or overwritten.

## GHA-only executable screenshot contract

Extend the existing `web/e2e/kimi-device-browser-contract.test.ts`; do not run tests, build or install locally. Its actual full-page `authorization-code.html?full-page&scope-controls` fixture renders the application shell and production provider page, with synthetic accounts and network interception. Unexpected non-rename writes fail the test; quota reads are synthetic too.

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

Report exact PR head and actual GHA run separately. A source PR or a screenshot filename is not evidence of green CI or visual acceptance. After GHA produces artifacts, inspect the complete screenshots, including the bottom notes/actions, at mobile and desktop widths. Hand off to the independent Kimi reviewer for layout, button hierarchy, copy and rename-context review; do not count author's CI reruns as reviews. At most a necessary second review round. No screenshot was visually inspected when this contract was authored.
