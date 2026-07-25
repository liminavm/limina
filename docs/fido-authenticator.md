# Touch ID → guest FIDO2 authenticator (M14)

limina exposes the Mac's **Touch ID / Secure Enclave** to a Linux guest as an ordinary
FIDO2 security key. Browsers get WebAuthn passkeys; PAM gets Touch-ID login/sudo; SSH
gets `sk-*` keys — all backed by enclave-bound ES256 keys that never leave the Mac, with
a host Touch ID prompt gating every signature.

**Raw fingerprint data is never involved** — macOS exposes no sensor data at any
privilege level. This is an *authentication service*, not sensor forwarding: the host
runs a CTAP2 authenticator (`crates/limina/src/fido/`, `sep.rs`, `swift/fido_sep.swift`)
whose keys live in the Secure Enclave; the guest agent presents a `/dev/uhid` FIDO HID
device and bridges CTAP frames over the vsock control plane. Design + decisions:
`docs/roadmap.md` M14.

## Two transports (agent/uhid + stock USB), one authenticator

The CTAPHID core (`crates/limina/src/fido/`) is transport-agnostic by design, so the
Touch-ID authenticator reaches the guest two ways — both feeding the *same* per-VM passkey
store and the same keepalive engine (`crate::fido::pump`):

- **Enhanced tier — `limina-agent` + `/dev/uhid`, over the vsock control plane.** The
  agent creates the HID device; CTAP frames ride `Message::FidoReport`. Needs the agent.
- **Stock tier — emulated USB HID gadget, over the xHCI controller** (M14 Stage C, shared
  infra with M7 USB). With `--usb` and a usable Secure Enclave, limina cold-plugs a
  vendor-neutral FIDO HID gadget onto the emulated controller; the guest's stock `xhci-plat`
  + `usbhid` bind it as a plain `/dev/hidrawN` with **zero guest components**. The gadget in
  the worker is a thin transport (a generic libkrun "HID report pipe" — mechanism); its
  64-byte CTAPHID frames are shuttled over a UNIX socket to the supervisor's authenticator
  (policy — SEP/LAContext + the store live in the Apple-Development-signed supervisor). A
  guest with the agent gets uhid; a stock guest gets USB; a guest with both gets both.

## What works today (enhanced tier)

- **Vendor-neutral FIDO HID device** — systemd's fido-id detects it by usage page
  (0xF1D0), so hidraw access + browser/libfido2 discovery are automatic.
- **WebAuthn in browsers** — verified: register + login on webauthn.io in guest Firefox
  (which uses authenticator-rs, not libfido2) with a host Touch ID prompt.
- **PAM (`pam_u2f`)** — verified: `pamtester` authenticates through the enclave.
- **`fido2-cred` / `fido2-assert`** — verified end-to-end (attestation + assertion).

Passkeys are **device-bound** (like a hardware key, not iCloud-synced) and **per-VM**:
they persist in the managed VM's bundle dir (`<bundle>/fido-credentials.json`).

## Requirements

- A Mac with a Secure Enclave (Apple silicon / T2). Without one the host never advertises
  the `fido` capability and the guest simply has no authenticator (graceful degrade).
- The app / supervisor must be **Apple-Development-signed** (ad-hoc signing can't use the
  enclave — same class as the TCC accessibility trap). The shipped `.app` satisfies this.
- Enhanced-tier guest with **`limina-agent` ≥ 0.3.0** (creates the `/dev/uhid` device).

## Recipes

### Browser passkeys (zero guest config)

Just browse. On webauthn.io (or any WebAuthn site), choose the **security key** /
"use a different device" option (not the platform/screen-lock one) — that routes to the
limina device and prompts Touch ID on the Mac.

### Touch ID for sudo / login (`pam_u2f`)

Verified recipe (Fedora guest). Packages: `pam-u2f`, `pamu2fcfg` (separate package on
F44), optionally `pamtester` to test.

```sh
sudo dnf install -y pam-u2f pamu2fcfg

# 1. Register the credential (prompts Touch ID once):
mkdir -p ~/.config/Yubico
pamu2fcfg -u "$USER" > ~/.config/Yubico/u2f_keys

# 2. Enable pam_u2f. Cleanest is authselect (applies to system-auth = login/sudo/GDM):
sudo authselect enable-feature with-pam-u2f
#    …or, to scope it to sudo only, add to the TOP of /etc/pam.d/sudo:
#       auth  sufficient  pam_u2f.so  cue
```

Now `sudo` (and GDM / console login) accepts a Touch ID tap. `cue` prints a "touch
device" hint. To *require* the key (2FA) use `required` instead of `sufficient`.

Notes:
- Each registered credential is enclave-bound to this Mac; re-run `pamu2fcfg` per Mac.
- The guest user needs hidraw access — the seated user gets it via uaccess automatically;
  for a headless/ssh context add a udev rule or run as the seated user.

### SSH keys gated by Touch ID

`ssh-keygen -t ecdsa-sk` (needs `libfido2`) creates a security-key-backed SSH key; each
use prompts Touch ID. Resident variant: `-t ecdsa-sk -O resident`.

## Implementation notes / gotchas

- **CTAP2 canonical CBOR is mandatory.** getInfo/response maps must be sorted
  shorter-key-first then bytewise (e.g. options `rk,up,uv,plat`). libfido2 rejects
  non-canonical CBOR as invalid and falls back to U2F → `FIDO_ERR_RX`. Firefox's
  authenticator-rs is equally strict.
- **CTAPHID keepalive is mandatory.** A command that blocks on the Touch ID prompt must
  stream `CTAPHID_KEEPALIVE(processing)` or clients time out. The host runs the CTAP2
  command off the serve thread and pumps keepalive every 100 ms.
- **Debugging:** `FIDO_DEBUG=1 fido2-cred …` prints libfido2's wire trace; a raw Python
  CTAPHID probe on `/dev/hidraw0` isolates our-stack-vs-client (see
  `spikes/touchid-fido/`).

## Stock-tier USB path: what's automated vs. manual

**Automated (L1, `l1_xhci_fido_authenticator`):** the USB controller is on by default, so the
gadget cold-plugs and the stock guest binds it as `/dev/hidrawN` with usage page 0xF1D0
(fido-id-style detection); a raw CTAPHID probe drives **INIT + authenticatorGetInfo** end-to-end
through the whole proxy path — both presence-free, so no Touch ID. `LIMINA_FIDO_TEST_APPROVE=1`
lets CI advertise the capability on a host without a usable enclave (getInfo touches no enclave
key). Run a stock guest yourself with a bare `limina` (add `--no-usb` to drop the controller):
`fido2-token -L` sees the device and `fido2-token -I` completes.

**Left for a human to validate (real Touch ID):** `makeCredential` / `getAssertion` — i.e.
`fido2-cred`, `fido2-assert`, browser passkey registration/login, and `pam_u2f` — all block
on a live host Touch ID prompt, so they need someone at the Mac. They exercise the identical
SEP path the (already-verified) uhid transport uses; only the transport in front of it is new.

## Not yet

- **Stock-tier user-presence flows over USB not yet auto-guarded** — makeCredential/
  getAssertion over the USB gadget need an L2 (enhanced image, `fido2-cred`/`fido2-assert`)
  with the test-approve knob backing a software key, parallel to the existing FIDO guard.
- `hmac-secret` (systemd-cryptenroll / LUKS) — can't live in the SEP; needs a
  software-key fallback, decided per use.
