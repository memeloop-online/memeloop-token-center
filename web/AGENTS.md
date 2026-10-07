# Frontend UI and copy

- Treat every visible string as finished product copy for someone unfamiliar with the implementation. Explain what the feature does, the consequence of an action, or the next useful step.
- Do not put worklogs, review notes, delivery status, implementation plans, acceptance criteria, or permission-success announcements in the interface. Keep them in PRs and operational records.
- Describe unavailable features in terms of what the user can do now, not what engineers have or have not integrated. Errors should identify the useful cause and next action rather than repeat the status code.
- Preserve material warnings: unsaved changes, costs, destructive effects, stale information, uncertain results, and the fact that closing a page may not cancel an operation. Use plain language; do not replace uncertainty with a success claim.
- Reuse the existing Fluent components, navigation and form state. Do not create a separate status panel or workflow for functionality that belongs in an existing settings section.
- Keep shared Chinese and English copy in the existing translation catalog or the feature's shared copy helper. Do not duplicate messages across entry points. Use names instead of internal identifiers in ordinary labels; retain necessary technical information in clearly labeled details.
- Review the actual empty, loading, permission-denied, error and success states, including mobile layouts. Keep copy regressions in the existing contract/browser tests; automated validation runs in GitHub Actions, not locally.
- A technical configuration label may need a protocol name or precise unit. Do not indiscriminately remove information needed to configure the feature safely.
