# Contributing to limina

Thanks for contributing. This file covers the terms a contribution comes in under and the
few conventions the repository enforces. For how the project is built and validated, read
[`docs/dev-onboarding.md`](docs/dev-onboarding.md) and the project guide
[`CLAUDE.md`](CLAUDE.md); the pull request template asks for the validation you ran.

## License terms for contributions

limina is licensed **GPL-2.0-only WITH LicenseRef-limina-exception**
([`LICENSES/LicenseRef-limina-exception.txt`](LICENSES/LicenseRef-limina-exception.txt)).
Contributions are accepted under those same terms, inbound equals outbound. Two parts of the
exception matter to you as a contributor:

- **Section 1, the linking exception**, lets limina be combined with Apache-2.0 material such
  as libkrun. Your contribution carries it like every other file.
- **Section 2, the platform distribution grant.** Every copyright holder grants the limina
  maintainers, named in [`AUTHORS`](AUTHORS), permission to convey limina through application
  stores whose terms impose conditions the GPL alone would not allow. It restricts nobody and
  removes no right from any recipient; it exists so that a build of limina can reach such a
  store while every contributor's copyright and the GPL terms stay intact. By contributing
  under the project license you make that grant for your contribution.

You keep your copyright. Do not assign it to anyone and do not expect to.

## Certify your contribution: sign off every commit

Each commit must carry a `Signed-off-by:` line with your real name and email address
(`git commit -s` adds it). The sign-off certifies the
[Developer Certificate of Origin 1.1](https://developercertificate.org/): that you wrote the
change or have the right to submit it, and that you submit it under the license indicated in
the files you touch, which for limina is the license and exception described above.

## Record your copyright

Add your own copyright line to the SPDX header of every file you create, and to files you
substantially change if you want the credit:

```
// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Your Name
```

For formats without comments, use a REUSE `.license` sidecar file. The repository is
[REUSE](https://reuse.software/)-compliant, and `reuse lint` must stay clean; the catch-all
annotation in [`REUSE.toml`](REUSE.toml) attributes header-less files to the original author,
so a file of yours without a header is credited to someone else.

## Do not change the licensing terms

Changes to `LICENSES/`, `REUSE.toml`, `AUTHORS`, SPDX headers, or the License section of the
README are maintainer decisions. A pull request that touches them will be asked to drop those
hunks unless a maintainer requested the change.

## What not to commit

No credentials, signing material, notarization profiles, VM disk images, snapshots, or build
artifacts. The working tree of a limina developer routinely holds multi-gigabyte images next to
the sources; stage files by name, never with `git add -A` or `git add .`.

## Formatting and lints

The pre-commit hook (`scripts/setup-hooks.sh` enables it) runs `cargo fmt --check` and
`cargo clippy -- -D warnings` on the code we own. Continuous integration runs the same checks
where a hosted runner can; the full HVF boot suite (`scripts/run-suite.sh`) needs a developer
Mac and remains the real validation for anything that can affect the boot path.
