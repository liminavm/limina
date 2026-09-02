---
name: Bug report
about: Report an issue or crash on macOS
title: ''
labels: bug
assignees: ''
---

<!--
SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
Copyright © 2026 Marcelo Jorge Vieira
-->

**Before submitting**

Please check the pinned issues and search the existing issues first. If the same problem has
already been reported, add your details and diagnostics there instead of opening a duplicate.

**Describe the issue**

Replace this text with a clear and concise description of the problem.

**Steps to reproduce**

1. Describe the first step.
2. Describe the next step.
3. Describe what happens.

**Expected behavior**

Describe what you expected to happen instead.

**Configuration (required)**

* Limina version or commit:
* Installation method (DMG, local build, or other):
* macOS version:
* Mac model and chip:
* Guest distribution and version:
* Guest setup (stock, enhanced, or partially enhanced; list installed Limina components):
* VM configuration (vCPUs, memory, graphics mode, disks, and relevant devices):

**Crash log**

If Limina crashed, open Console.app, select `Crash Reports`, and attach the latest report for
`Limina`, `limina`, or `limina-vmm`. Also attach `~/Library/Logs/Limina/panic.log` if it exists.

**Debug log**

For a managed VM, attach the VM bundle's `logs/supervisor.log`. Bundles are stored in
`~/Library/Application Support/Limina/VMs` by default. For a command-line or development run,
attach the terminal output and the worker log named when Limina starts. For a command-line run,
set `RUST_LOG=warn,limina=info` before starting Limina.

**VM definition**

If the issue is specific to a VM, attach its `.liminavm/vm.toml` after reviewing and redacting
personal host paths or other private information. Do not upload VM disk images,
`fido-credentials.json` (the passkey store next to `vm.toml`), signing material, or other
personal data.

**Additional context**

Add screenshots, recordings, or any other context that may help diagnose the issue.
