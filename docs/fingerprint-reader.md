# Touch ID → guest fingerprint reader (M14)

limina exposes the Mac's **Touch ID** to a Linux guest as an ordinary USB fingerprint
reader. `fprintd` enrolls and verifies a finger; GNOME's *Fingerprint Login* and PAM
(`pam_fprintd`) then accept a fingerprint for login / unlock / sudo — each verification
raises a host Touch ID prompt on the Mac.

**Raw fingerprint data is never involved** — macOS exposes no sensor data at any privilege
level, and Touch ID only ever reports *that* a trusted finger matched (never an image, never
*which* finger). This is an *authentication service*, not sensor forwarding: the guest talks
to an impersonated **Elan match-on-chip (MOC)** reader whose "template store" holds only the
opaque `user_id` string libfprint itself authored, and a "match" is a `LAContext` biometric
prompt. Design + decisions: `docs/design/usb-moc-fingerprint.md`, `docs/roadmap.md` M14.

## One reader, one logical finger

Real match-on-chip readers store templates on the device and can hold several fingers. Ours
can't and shouldn't: the Secure Enclave only ever answers the yes/no "was a trusted finger
presented" — it never tells us which finger — so **a single logical finger is the honest
model.** The emulated reader presents exactly one slot; a second, distinct enrollment is
refused at the protocol layer (the guest sees the reader report "full"), so you never end up
with two guest credentials that both map to the same indistinguishable Touch ID answer. Any
of your macOS-enrolled fingers satisfies the prompt.

## Architecture — the FIDO stock-USB split, reused

Same shape as the stock-tier FIDO authenticator (`docs/fido-authenticator.md`): **mechanism in
the worker, policy in the supervisor.**

- **Worker (`crates/limina-vmm/src/moc_usb.rs`).** Presents the byte-exact Elan USB identity
  (`04f3:0c7d`, a generic libkrun `BulkPipe` gadget — the reusable bulk-endpoint mechanism)
  cold-plugged onto the emulated xHCI controller. The guest's stock `xhci-plat` binds it and
  `libfprint`'s `elanmoc` driver claims it as a real reader — **zero guest components.** Raw
  bulk packets are shuttled over a UNIX socket to the supervisor.
- **Supervisor (`crates/limina/src/moc/`).** Runs the `elanmoc` protocol state machine, backs
  every "match" with a biometrics-only `LAContext` Touch ID prompt (`swift/fido_sep.swift`,
  the Apple-Development-signed binary where the enclave works), and owns the per-VM store.

Passkey-style, the enrolled finger is **per-VM**: the `user_id` persists in the managed VM's
bundle dir (`<bundle>/moc-templates.json`), the same bytes `fprintd` keeps in its own on-disk
print, so the two stay consistent across reboots.

## What works today

Live-validated end-to-end on a stock Fedora 44 guest (with `LIMINA_FP_TEST_APPROVE` standing
in for the finger):

- **`libfprint` binds `04f3:0c7d`** as *Elan MOC Sensors*; `fprintd-list` sees the device.
- **Enroll → list → verify → delete** all round-trip: `fprintd-enroll` drives the full 9-stage
  enrollment to `enroll-completed`, `fprintd-verify` reports `verify-match`, `fprintd-delete`
  clears it.
- **Single-finger policy holds** — a second distinct enroll is refused (not silently
  overwritten).
- **fwupd does not claim the device** — the chosen PID isn't in fwupd's `elanfp` quirk, so no
  firmware plugin touches it (defense-in-depth: an unconditional firmware-version bluff is the
  fallback should a future fwupd add it — see `docs/design/usb-moc-fingerprint.md` §6).

## Requirements

- A Mac with **Touch ID and at least one finger enrolled in macOS**. limina gates the
  `fingerprint` capability on `LAContext.canEvaluatePolicy(...biometrics)` — a Mac with no
  sensor or no enrolled finger simply never advertises the reader (graceful degrade), so the
  guest has none.
- The app / supervisor must be **Apple-Development-signed** (ad-hoc signing can't reach the
  enclave — same class as the TCC accessibility / FIDO trap). The shipped `.app` satisfies
  this.
- A guest with stock `libfprint` + `fprintd` (Fedora Workstation has both). **No limina guest
  components are needed** — this is a stock-tier feature.

## Recipes

Start the VM with the reader attached:

```sh
limina --fingerprint            # ad-hoc; add --net etc. as usual
# managed VMs: set [hardware] fingerprint = true in vm.toml
```

**Persistence caveat:** the enrolled finger's host-side record lives in the **managed VM's
bundle** (`<bundle>/moc-templates.json`). An **ad-hoc `limina --fingerprint`** run keeps it
only in memory, so it's lost on restart — after which the guest's `fprintd` still lists the
print but a verify is an immediate no-match (no Touch ID prompt). Use a managed VM to persist
enrollment, or `fprintd-delete` + re-enroll after an ad-hoc restart.

### GNOME Fingerprint Login (zero config)

**Settings → Users → Fingerprint Login → Add** walks the enrollment. It takes a **single Touch
ID tap** on the Mac — the enclave only answers once, so we prompt on the first capture stage
and let GNOME's swipe progress bar race to done on its own (don't wait for nine taps).
Afterwards the login screen, screen-unlock, and `sudo` (once PAM is enabled, below) accept a
fingerprint.

### fprintd directly

```sh
fprintd-enroll        # enroll the current user's finger (prompts Touch ID)
fprintd-verify        # verify  (prompts Touch ID → match / no-match)
fprintd-list "$USER"  # show the enrolled finger
fprintd-delete "$USER"
```

### Fingerprint for sudo / login (`pam_fprintd`)

```sh
# Fedora: enable the fprintd PAM leg for system-auth (login / sudo / GDM / unlock)
sudo authselect enable-feature with-fingerprint
```

Now `sudo` (and GDM / console login / screen unlock) offers a fingerprint — each tap raises a
Touch ID prompt on the Mac. Enroll first (above) or PAM has nothing to match.

## Implementation notes / gotchas

- **Single logical finger, by design** (see above) — a second distinct enroll is refused; a
  delete-then-re-enroll replaces it. This is a policy choice grounded in what Touch ID can
  actually answer, not a limitation of the emulation.
- **Transport dependency — xHCI Immediate Data (IDT).** The tiny elanmoc commands ride inline
  in the TRB (`IDT=1`, xHCI §4.11.7); the emulated controller must read the immediate bytes,
  not DMA from the parameter field. Handled in `patches/libkrun/0100`; without it the reader's
  open sequence stalls (*"endpoint stalled or request not supported"*). See
  `docs/design/usb-moc-fingerprint.md` §2.1.
- **`vulkaninfo`-style false negatives don't apply here, but a login shell might.** `fprintd`
  runs as a system service; a match still needs the host to be able to raise a Touch ID sheet
  (a seated/open Mac). A clamshell-closed or locked Mac may not present the prompt.
- **Debugging:** `RUST_LOG=limina=debug` prints the per-command elanmoc trace
  (`moc: cmd [40, ff, 00] -> DATA ep 0x83 len 2`) — the field oracle that pinned the IDT bug.

## Stock-tier: what's automated vs. manual

**Automated (L1, `l1_xhci_fingerprint_reader`):** with `--fingerprint`, the gadget cold-plugs
and the stock guest enumerates it with the byte-exact identity `04f3:0c7d`. Enumeration answers
from the descriptors alone, so it's deterministic in CI; `LIMINA_FP_TEST_APPROVE=1` lets the
supervisor advertise the capability on a host without a usable sensor. The protocol engine
(open / enroll / list / verify / delete, single-finger enforcement, the 71-byte get-userid
reply) is unit-tested against a **golden pcapng capture of real elanmoc hardware traffic**
(`crates/limina/src/moc/`), and the IDT transport fix has its own xHCI unit test.

**Left for a human to validate (real Touch ID):** the full `fprintd-enroll` / `fprintd-verify`
round-trip against a live finger — it blocks on a host Touch ID sheet, so it needs someone at
the Mac. It exercises the identical `LAContext` path the (already-verified) FIDO authenticator
uses; only the elanmoc transport in front of it is new. The full flow *is* automatable with
`LIMINA_FP_TEST_APPROVE=1` (which auto-approves the match) on a booted stock guest — this is
the manual live-validation recipe, and a future L2 (stock image + `fprintd` + the test-approve
knob) would fold it into the suite.

## Not yet

- **Full `fprintd` enroll/verify flow not yet auto-guarded** — needs an L2 (stock image with
  `fprintd`, `LIMINA_FP_TEST_APPROVE` backing the match), parallel to the FIDO stock-USB gap.
- **Multi-finger** — deliberately not supported (the enclave can't distinguish fingers; see
  the single-finger rationale).
