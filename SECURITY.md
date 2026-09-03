<!--
SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
Copyright © 2026 Marcelo Jorge Vieira
-->

# Security Policy

## Reporting a vulnerability

The limina team takes security vulnerabilities seriously. If you discover a security issue in
limina or one of the project-maintained components it ships, please report it responsibly.

**Do not open a public GitHub issue or discussion for a suspected vulnerability.** Report it
privately through GitHub's [vulnerability reporting
form](https://github.com/liminavm/limina/security/advisories/new). You can also reach the form
from the repository's **Security** tab by selecting **Report a vulnerability**.

Include enough information for us to understand and reproduce the issue:

- a description of the vulnerability and its potential impact;
- the affected limina version or commit;
- steps to reproduce the issue;
- a minimal proof of concept, if available;
- relevant macOS, hardware, guest, and VM configuration details; and
- any known mitigations or workarounds.

Do not submit credentials, signing material, VM disk images, snapshots, passkey stores, or other
personal data. Ask the maintainers to arrange a suitable transfer method before providing
sensitive artifacts that are essential to the investigation.

We will acknowledge the report as soon as practical and work with you to confirm its scope,
develop a fix, and coordinate disclosure.

## Responsible disclosure

Please keep the vulnerability private until a fix or mitigation is available. Allow the
maintainers reasonable time to investigate and address it, and coordinate with us before
publishing an advisory, proof of concept, blog post, or CVE.

Once the issue has been addressed, we will credit your contribution unless you prefer to remain
anonymous.

## Supported versions

limina does not yet publish stable releases. Until the first release, security fixes target the
current `main` branch only; older commits are not supported. Users should update to the latest
commit on `main` to receive security fixes.

This policy will be updated with a version support table when stable releases are available.
