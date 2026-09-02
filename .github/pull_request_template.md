<!--
SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
Copyright © 2026 Marcelo Jorge Vieira
-->

<!--
Thanks for contributing to limina. Keep the PR description focused and delete sections that do
not apply. Prefer exact commands and observed results over a generic "tests pass" statement.
-->

## Summary

<!-- What changed, why it changed, and any issue or design document that provides context. -->

## Validation

<!--
List the exact commands and results. Changes that can affect the VM boot path should use
`cargo xtask test` through `scripts/run-suite.sh <log>`. If a relevant check was not run, explain
why and what evidence was used instead.
-->

- Automated:
- Manual:
- Not run:

## Compatibility and risk

<!--
Describe relevant behavior for stock and enhanced guests, partial enhanced-component states,
macOS versions or hardware, and the fallback or rollback path.
-->

- Stock guest:
- Enhanced guest:
- Host/platform:
- Fallback:

## Forked stack changes

<!--
If this changes libkrun, imago, virglrenderer, Mesa, Linux, edk2, or another fork-model input,
link the fork commit and identify the `third_party/manifest.toml` pin. Explain whether the change
is upstreamable. Delete this section when no forked dependency changes.
-->

## Visuals

<!-- Add screenshots or recordings for user-visible changes. Delete this section otherwise. -->

## Checklist

- [ ] The PR is focused and does not include unrelated changes.
- [ ] Every commit is signed off (DCO) and new files carry my copyright line (see
      `CONTRIBUTING.md`).
- [ ] Relevant documentation and design decisions are updated.
- [ ] Validation results and intentionally skipped checks are recorded above.
- [ ] No credentials, signing material, VM images, or generated build artifacts are committed.
