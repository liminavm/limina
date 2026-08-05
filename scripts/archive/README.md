# Archived scripts (superseded — pending deletion)

These are kept for reference after being superseded. They are **not** wired into
any current workflow. Slated for deletion once we're confident nothing here is
worth resurrecting.

| Script | Superseded by | Why |
|---|---|---|
| `build-mesa-zink-sysext.sh` | `scripts/build-mesa-rpm.sh` | systemd-sysext delivery was abandoned: enhanced mesa 26.2 vs stock 25.3.x has a different `libgallium` soname, and an overlay can only *shadow* (not *remove*) the stock lib → a 25.3⊕26.2 ABI blend breaks mutter's KMS EGL. An RPM **replaces** stock (old soname removed). See memory `limina-enh-delivery`. |
| `build-mutter-sysext.sh` | `scripts/build-mutter-rpm.sh` | Same sysext→RPM pivot; mutter ships as a target-version-matched RPM carrying our rebased patches. |
| `build-mesa-zink.sh` | `scripts/build-mesa-rpm.sh` | Prefix-style guest zink+venus build, superseded by the RPM delivery — and **zink-as-guest-GL was dropped as a supported configuration 2026-08-04** (GL rides virgl/vrend, venus stays for Vulkan; `docs/design/gl-path-vrend-vs-zink.md`). |

The RPM delivery is end-to-end validated (pristine F43 → 16k+venus desktop,
commit 510e527).
