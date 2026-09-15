# Operator design system

## Foundation and boundaries

Use Microsoft Fluent UI React v9, exact `@fluentui/react-components@9.74.7`.
Its declared React peer range includes the application's React 19. It is MIT
licensed. The lockfile pins transitive resolution/integrity; installs do not need
package lifecycle scripts. No CDN, remote font, icon bundle, or telemetry is added.
Use named ESM imports via `src/design-system`; do not add Fluent v8 or Northstar.

`MtcFluentProvider` follows the shell's existing `data-theme` rather than creating
another preference store. Fluent owns semantic colors, type scale, elevation,
corner radii and control focus behavior. Product spacing is 24 px per section,
16 px between fields; mobile section spacing is 16 px and detail targets 44 px.
Use system fonts, sentence case, tabular numerals for measurements, and Fluent
Text presets. Reduce nesting: one DataSurface per task, no cards inside cards.
Separation uses space and typography; borders are reserved for input affordance,
selection, error and forced-color boundaries. Reduced motion disables decorative
animation; forced colors use system colors and visible surface outlines.

This commit establishes a foundation, **not a completed page redesign**. Requests,
monitoring, pricing and provider editors are separate page-owned migrations.
Do not mass-override legacy selectors or claim existing custom controls are Fluent.
Legacy `input`, `.panel`, and themed selectors still exist: each migrated page
must remove its old component classes and verify composed styles in both themes.
The entire shell has a provider so portaled Fluent overlays receive the same theme.

## Components and task flow

- `Disclosure`: Fluent Accordion disclosure for optional/advanced groups. Native
  `<details>/<summary>` triangles are not an accepted primary disclosure pattern.
  Use `open` + `onOpenChange(nextOpen)` for controlled validation flows, or
  `defaultOpen` for uncontrolled sections. Do not switch modes during a mount.
  The caller opens the invalid section then focuses its invalid input in an effect
  using a cancellable `requestAnimationFrame`, after Accordion context consumers
  have committed visibility. Fluent Input's native input ref is its `input.ref`
  slot. The primitive never steals focus on an ordinary disclosure.
  The supported collapse-motion render slot keeps children mounted and sets
  `hidden` synchronously, preserving local/uncontrolled drafts without an animation
  frame race against validation focus. Disclosure deliberately has no collapse
  animation. Fluent also makes collapsed content inert so keyboard navigation
  cannot enter hidden fields. Validation still belongs to the form.
- `ActionButton`: requires localized visible `label`, with optional icon. Icon-only
  actions are exceptions only for familiar high-frequency actions (close/search);
  require both localized accessible label and Fluent Tooltip. Do not add a custom
  icon-only shortcut merely to make a crowded form fit.
- Localization is caller-owned: all labels, descriptions, tooltips, validation and
  empty states use the current locale dictionary. Primitives provide no English
  fallback strings. A page contract must exercise Chinese and English and assert
  its translated visible/accessibility labels; missing translation is an error,
  not permission to expose keys or fallback English. Opaque IDs are not translated.
- `DataSurface`: semantic section; pass `aria-label`/`aria-labelledby` when useful.
- `FormSection`: fieldset/legend/description, no decorative box. Group by actual
  sequence: identity → connection and proxy → routing → optional advanced settings.
- `Field` + Fluent controls: labels, descriptions, validation belong together.
  Never communicate a required dependency only through a disabled Save button.
- `DetailTooltip`: one **focusable element**, supplemental plain-text `content`.
  Hover/focus and click reveal details; Escape and blur dismiss via Fluent.
  Use `className="mtc-detail-trigger"` on a native button; never nest controls.
  Critical names/status/errors remain visible. Route IDs, exact monetary precision,
  protocol and completion timestamp can be secondary details. Interactive details
  use a nonmodal Fluent Popover, not Tooltip or a backdrop/dialog.
- Monitoring background visualization is decorative (`aria-hidden`), derived from
  real data with a visible numeric equivalent. Use low-opacity fills behind text;
  do not invent traces or imply a percentage without a known denominator. Disable
  decoration in forced colors and movement under reduced motion.

## Performance, SSR and acceptance

Vite client rendering is the current target. The provider has a server snapshot,
but full SSR requires a request-scoped Griffel renderer and style extraction;
do not assume provider-only SSR prevents a flash of unstyled content. Fluent uses
context, so future React Server Components must put the provider at a client boundary.

The aggregate npm tarball is ~2.2 MB unpacked, **not the shipped bundle size**.
Named ESM imports allow tree shaking, but provider/styles and accessibility helpers
are real added runtime cost. CI must record before/after production gzip chunks;
no bundle-size claim is made from installation size. Avoid importing this barrel
for non-UI utilities; retain lazy page/charts loading and inspect actual chunk output.
No local production build is required for this slice.

Browser contract covers the production legacy styles composed with the provider,
both themes, 320/768/1440 layout, field association, focus/click detail visibility,
Escape/blur dismissal, forced colors and reduced motion. Page acceptance additionally
requires real workflows, loading/empty/error/permission states, keyboard navigation,
touch operation and contrast checks; screenshots alone cannot establish completion.

## Official implementation references

- [Fluent 2 React overview](https://fluent2.microsoft.design/components/web/react)
- [Provider ownership](https://fluent2.microsoft.design/components/web/react/core/fluentprovider/usage)
- [Tooltip content and accessibility](https://fluent2.microsoft.design/components/web/react/core/tooltip/usage)
- [Microsoft Fluent UI source and MIT license](https://github.com/microsoft/fluentui)
- [Griffel SSR implementation guidance](https://griffel.js.org/react/guides/ssr-usage/)
# Global typography and acceptance boundary

Shared page titles, section headings, and supporting text use Fluent typography
tokens from the existing provider. The baseline keeps readable fallbacks for
standalone surfaces. Technical IDs, credentials, code, and machine payloads keep
their monospace presentation; explanatory prose does not inherit it from a
generic panel-heading selector. Remaining native actions use tokenized baseline
states, while Fluent actions own their geometry, focus, disabled, and pressed
states without global button overrides.

`app-typography-browser-contract.test.ts` renders the production `index.html` /
`main.tsx` / `Application` / `AppShell` / `Operator` graph at desktop and mobile
widths in both themes. Only API responses and the idle event stream are synthetic;
unexpected network calls and writes are rejected. Its full-page captures validate
the actual stylesheet graph, not a manually assembled component shell. They are
CI candidate evidence, not evidence that a deployment or real account is healthy.
