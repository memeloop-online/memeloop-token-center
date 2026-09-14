# UI surface audit — 2026-09-12

Base: `a47db73f9d3c554d16338759bc5175f8507ef6fb` (master / PR #66).

Scope: shared operator/self request diagnostics, Overview drilldowns, upstream
availability, and schema-generated upstream connection forms. No backend,
Helm, plugin, dependency, production, quota-reset, or upstream-call changes.

## Before evidence and classification

Inspected the retained Chromium screenshots under
`/tmp/mtc-ui-artifacts-w62Cbt/`, artifact revision
`96d76b20032833c9a1497dcde995721f569cdeaf`. These screenshots predate the base;
findings were cross-checked against the base source rather than represented as
fresh base screenshots.

| Surface / screenshot | Finding | Change and after evidence |
| --- | --- | --- |
| `overview-trends-light-1440.png` | Broken rail image is a fixture artifact: `configFile:false` serves public assets at `/`, while the application correctly requests `/ui-assets/`. The vertical rail label is intentional. | Fixture-only public asset adapter; browser asserts successful image decode. No product logo/fallback change. |
| `overview-trends-light-1440.png`, `request-diagnostics-light-390.png` | Light-theme table/session links retain the pale dark-theme foreground. | Shared light-theme link and keyboard-ring colors; matrix asserts the request link token. |
| `request-diagnostics-light-390.png` | Copy buttons are tiny; ordinary IDs fit narrowly. Very long session labels need explicit containment, not page-level clipping. The unstyled final UUID is the fixture's opened-session output, not a product footer. | 44px mobile copy/session actions; full detail IDs can wrap independently of copy; long React fixture session name/agent metadata. Table IDs continue to use ellipsis. |
| `upstream-availability-light-390.png` | Clickable attempt results look like passive status badges; timestamps, status and latency wrap unpredictably. Expiry also depends on the test execution date. | Underlined result actions, 44px mobile targets, timestamp on its own row and subtle separators; model routing state aligns left on narrow screens. Fixture clock fixed to its observation time. |
| `provider-form-light-mobile.png`, `provider-form-dark-mobile.png` | Required/optional sections already preserve values and expose invalid fields; no need to regroup or hide more fields. Narrow headings/help and low-contrast focus borders need shared treatment. | Bounded wrapping legends/help, stronger theme-aware focus, 44px disclosures and full-width mobile submit. In-flow actions avoid obscuring fields or keyboard focus. |

## Verification and after artifacts

Only `git diff --check` is run locally. Typecheck, build, browser contracts and
the rest of the full gates are delegated to CI; no local heavy test run.

`web/e2e/ui-system-browser-contract.test.ts` covers the four fixtures at
320/390/768/1440 in English and Chinese, light and dark (64 combinations).
It checks component scroll width and viewport bounds (not just body clipping),
full session labels, keyboard focus and disclosure, touch-target dimensions,
and real logo decoding. It uses fixed wall time and animation-frame readiness,
no sleeps or screenshot pixel thresholds. External browser requests are blocked.

After screenshots are emitted to `web/e2e-artifacts/ui-system/` for 390 and 1440
in both locales and themes, retained by the monitoring-interactions artifact.
Existing form contracts continue to verify validation reveal, value preservation,
failed-save focus, and restoration after save/refresh. Existing request contracts
continue to verify actual clipboard copy and exact session navigation.

CI results and after screenshot review are pending when the draft is opened;
the draft must not be represented as visually verified until those artifacts
have been inspected. This is a shared accessibility/containment pass, not a
visual redesign or a claim of full WCAG conformance.
