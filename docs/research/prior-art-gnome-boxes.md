# Prior art: GNOME Boxes ergonomics — what to copy / take inspiration from

**Status:** research note (2026-07-17). Competitive/prior-art scan, not a technical-area doc — hence
the non-numbered name (the `01..11` series is limina's own subsystem research).

**Why this file exists:** Boxes is the closest thing to limina in spirit — a desktop-first VM app for
Linux guests, and (as of 2025) explicitly chasing good **aarch64** VM ergonomics, the same ground we
stand on. It also happens to use a **vsock**-based SSH story much like ours. This note captures what
they've built that we could copy or be inspired by, and — just as important — where limina is already
ahead so we don't inherit their ceilings. Prompted by a GUADEC talk (Felipe Borges, GUADEC 2025) the
user watched; the talk itself is flagged below to mine properly later.

The one-line framing: **Boxes leans 100% on the SPICE guest stack for integration and pays for it with
SPICE's ceilings; limina's thesis is native devices for the enhanced tier _plus_ SPICE as a stock
on-ramp.** So most of Boxes' integration features map onto [[roadmap M12 SPICE]] (validating it), and
the genuinely new idea to steal is their **SSH-over-vsock with zero-touch credential injection**.

---

## 1. SSH-over-vsock with zero-touch credential injection ⭐ (the standout to copy)

Source: Maximiliano Sandoval, *SSH into GNOME OS running in a sandboxed Boxes VM*, 2026-07-15
(https://blogs.gnome.org/msandova/2026/07/15/ssh-into-gnome-os-running-in-a-sandboxed-boxes-vm/).

Boxes' SSH access is **better than limina's today** (we do gvproxy NAT + an auto-allocated `2222+`
TCP forward the user must read from the log — see [[limina-fedora-access]]). Their scheme has two
**independent** halves; the value is that each stands alone:

### Half A — credential injection via SMBIOS type-11 OEM strings → systemd-creds (the gem)

The host's SSH **public** key is base64'd and passed to the guest as a firmware **SMBIOS type-11 OEM
string**:

```
io.systemd.credential.binary:ssh.ephemeral-authorized_keys-all=<base64 of ~/.ssh/id_ed25519.pub>
```

(libvirt `<sysinfo type='smbios'><oemStrings><entry>…` → QEMU SMBIOS type 11.)

Stock **systemd** in the guest picks this up automatically — **no cloud-init, no SSH agent, no guest
agent of ours**:
- `systemd-analyze smbios11` shows the string;
- the credential lands in `/run/credentials/@system/` (`systemd-creds --system list`);
- sshd honors `ssh.ephemeral-authorized_keys-all` once `sshd.service` is enabled.

**Why this is the gem for limina:** it is **zero-install SSH key provisioning on a completely stock
Fedora guest** — it works *before* `limina-agent` exists, so it belongs to the baseline/bootstrap tier
(fits the two-tier guarantee perfectly). Fedora Workstation ships systemd new enough to consume it.

**What limina needs:** libkrun must expose **SMBIOS type-11 OEM strings** to the guest. Check first
whether libkrun sets any SMBIOS at all (aarch64 microvm may not) — if not, it's a small, upstreamable
libkrun patch (mechanism in libkrun, policy — which key — in limina, per the house rule).

### Half B — transport over vsock instead of a forwarded TCP port

Boxes connects with `ssh user@vsock/<cid>` using **`systemd-ssh-proxy`** as the ProxyCommand
(systemd ≥ 257 on the host). No port collisions, no host network exposure, no `--ssh-port` bookkeeping.
`scp` uses `user@vsock%<cid>:path`.

**What limina needs:** this half does **not** port literally — the macOS host is not systemd, so
`systemd-ssh-proxy` isn't present. But the mechanism is trivial to reimplement: a tiny host-side
**`ProxyCommand` helper that dials the guest's vsock ssh port** (we already own a multiplexed vsock
control plane and the guest already connects out — [[limina-m5]], [[limina-m3-networking]]). Result:
the user gets a stable `ssh limina-<vm>` with zero port juggling, replacing "read N from the log".

**Recommendation:** worth doing. Half A alone is a clean baseline-tier win; Half A + a host vsock
ProxyCommand helper gives Parallels-grade "it just connects" SSH. Wired into the roadmap as an **M3
follow-up** (see the roadmap M3 "Remaining" section) rather than its own milestone, since M3 is the
SSH home. Gating spike: does libkrun expose SMBIOS OEM strings today, and does stock Fedora's systemd
light up `ssh.ephemeral-authorized_keys-all` from them under libkrun?

---

## 2. Express / unattended install + an OS catalog (libosinfo + osinfo-db)

Boxes' headline first-run ergonomics: pick an OS from a list → it **downloads the image and runs an
unattended install** (Fedora, Ubuntu, Debian, Windows, openSUSE, CentOS/RHEL). The reusable machinery
is **libosinfo + osinfo-db** — a maintained database of distro metadata *and* unattended-install
script templates (kickstart / preseed / cloud-init / Windows sysprep).

**Relevance to limina:** today we ship a prebuilt enhanced image; for **user-created** VMs, this is the
difference between "hunt for an ISO, click through Anaconda" and "pick Fedora 44, wait". We could
**consume osinfo-db data directly** rather than reinventing distro metadata + install scripts.
Inspiration, not a near-term milestone — but the right building block to remember when VM-creation UX
comes up ([[limina-vm-definitions]]).

---

## 3. The whole SPICE integration stack (validates M12; notes a shared-folder alternative)

Boxes gets **all** guest integration from stock SPICE guest packages — which is exactly the M12 bet,
now **confirmed working on GNOME/Wayland** (the GUADEC clipboard demo is the evidence that de-risks the
old X11-era Wayland-clipboard worry flagged in [[roadmap M12 SPICE]]):

| Boxes feature | Mechanism (stock guest pkg) | limina mapping |
|---|---|---|
| Clipboard | `spice-vdagent` | M12 primary ✓ |
| Drag-drop file push (host→guest) | SPICE file transfer (`VD_AGENT_FILE_XFER_*`) | M12 secondary ✓ |
| USB redirection | SPICE `usbredir` | **Differs** — our M7 is native USB/IP, no SPICE |
| Shared folders | **`spice-webdavd` + phodav (WebDAV over a SPICE channel)** | **Differs** — our M5 is native **virtiofs** |
| Dynamic resolution | `spice-vdagent` `MONITORS_CONFIG` | **We're ahead** — native EDID / display modes ([[limina-display-modes]]); M12 excludes this |

Two takeaways:
- **Shared folders via spice-webdavd is a _different_ mechanism from our virtiofs.** Ours is a native
  mount (faster; auto-mount needs `limina-agent`). Theirs is a stock-package WebDAV mount that needs
  **zero** custom guest components. We almost certainly keep virtiofs (technically better), but it's
  worth knowing spice-webdavd is the zero-install baseline path if we ever want shared folders on a
  guest that won't take our agent.
- The meta-point: Boxes' reliance on SPICE **caps** it exactly where SPICE caps out; M12 is
  complementary to our native stack, not a pivot toward SPICE-for-everything.

---

## 4. Where limina is already ahead — differentiators to KEEP, not gaps to close

Boxes' widely-documented pain points are mostly things we've solved or designed past. Listed so we
don't accidentally regress toward them:

- **No CLI / scripting API.** We have `limina` + `cargo xtask` ([[limina-vm-definitions]], roadmap M11).
- **Weak networking** (no NAT tuning / port-forward / custom adapters). We have supervised gvproxy NAT
  with auto-port + `--ssh-port`, bridged planned ([[limina-m3-networking]], roadmap M3).
- **Basic snapshots, no branching trees.** M9 is designing host-side snapshots ([[limina-m9-suspend-resume]]);
  named/branching snapshot management is an easy place to exceed them.
- **Degrades under concurrent VMs.** Our dynamic-memory ballooning ([[limina-m6-dynamic-memory]]) targets
  exactly this.
- **3D accel** — Boxes rides virgl/venus via SPICE/QEMU; our venus-on-KK tier ([[limina-tier2-venus]]) is
  the deep-owned version of the same idea.

---

## 5. To mine properly later

- **GUADEC 2025 — Felipe Borges, Boxes / "first easy aarch64 Linux VM experience."** The concentrated
  version of Boxes' current ergonomics thinking, on the same aarch64 desktop-VM ground as limina. Get
  the recording/slides and extract specifics (deferred by user, 2026-07-17). Timetable:
  https://events.gnome.org/event/259/timetable/?view=standard_numbered
- Boxes NEWS / GitLab for the GTK4/libadwaita rewrite era details (Borges, 2025) — mostly codebase
  modernization, low direct relevance to our Rust/AppKit front-end, but the UX decisions in the
  rewrite are worth a skim.

### Sources
- SSH-over-vsock + SMBIOS credentials (2026-07-15): https://blogs.gnome.org/msandova/2026/07/15/ssh-into-gnome-os-running-in-a-sandboxed-boxes-vm/
- Feature/limitations guide (2025-10): https://www.glukhov.org/post/2025/10/gnome-boxes-linux-virtual-machines-manager/
- Boxes — Apps for GNOME: https://apps.gnome.org/Boxes/
- GUADEC 2025 timetable: https://events.gnome.org/event/259/timetable/?view=standard_numbered
- gnome-boxes NEWS: https://github.com/GNOME/gnome-boxes/blob/main/NEWS
