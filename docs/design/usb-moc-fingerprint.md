# Impersonated MOC fingerprint reader (M14, wave 3)

Status: **proposed** (2026-07-24). Parent design: `docs/design/usb-xhci.md` §3.5 (which
deferred this to "its own design doc"). Companion: `docs/fido-authenticator.md` (the
shipped Touch-ID authenticator whose transport split this mirrors), roadmap §M14.

This is the last M14 gadget. It presents the guest with a **USB match-on-chip (MOC)
fingerprint reader** that stock Fedora's `libfprint`/`fprintd`/GNOME bind and drive with
**zero guest components**, and backs every "match" decision with a **host Touch ID
prompt**. No real fingerprint data ever exists (macOS exposes none at any privilege
level — see `docs/fido-authenticator.md`); the device is a thin impersonation whose only
job is to look exactly like a real reader on the wire and answer "did an authorized human
touch the sensor" via the Secure Enclave.

## 1. What we impersonate, and why elanmoc

We synthesize the **device side of libfprint's `elanmoc` driver** — an Elan match-on-chip
reader. The driver source *is* the spec: whatever bytes `elanmoc.c` sends, we answer;
whatever it expects back, we produce. Two independent research passes (into upstream
libfprint `HEAD 91012c3` and fwupd master, 2026-07-24) picked elanmoc over the other three
MOC families for concrete reasons:

| driver | SDCP | framing | endpoints | commands | enroll stages | umockdev corpus |
|---|---|---|---|---|---|---|
| **elanmoc** ✅ | **none (cleartext)** | **raw header, no len/CRC/seq/crypto** | 8 bulk (2–3 used) | ~13 | **fixed 9** | ✓ |
| synaptics | none | 2-layer (ACE + bmkt magic 0xFE) | 4 (+interrupt) | ~32 | 8 | ✓ |
| goodixmoc | none | 8-byte hdr + CRC8 + CRC32, ACK-then-data | 2 bulk | 12 | 8/12 | ✓ |
| egismoc | none upstream | EGIS/SIGE + 16-bit checksum + ctrl-xfer init | 3 (+interrupt) | ~13 | 10/20 | ✓ |

elanmoc wins on every axis: the wire protocol has **zero framing overhead** (a command is
literally its 2–3 `cmd_header` bytes plus a zero-padded fixed-length payload — no length
prefix, no checksum, no sequence number, no encryption, no nonce), the init handshake is
five plain commands (no control-transfer dance, no version-blob parsing), and the enroll
stage count is a small fixed number. Decisive external confirmation — Blackwing Intelligence's
*"A Touch of Pwn"* (the 2023 Windows Hello sensor research): **ELAN uses "no SDCP, cleartext
USB communication, no authentication," so "any USB device can claim to be the ELAN sensor by
spoofing its VID/PID."** That is precisely this design; we are building the documented,
inherent-to-the-hardware impersonation, not defeating a protection.

**No SDCP anywhere matters.** SDCP (Secure Device Connection Protocol) would require a
factory-provisioned device certificate we cannot forge. It is **not implemented in upstream
libfprint at all** (no `fpi-sdcp-*` in the tree; it lives only in out-of-tree egismoc forks),
so no in-tree driver ever initiates the handshake. elanmoc is cleartext by construction.

### 1.1 The exact identity — PID `04f3:0c7d`

- **`idVendor = 0x04f3` (Elan), `idProduct = 0x0c7d`.** libfprint binds strictly by
  VID/PID: `fp-context.c` `usb_device_added_cb` requires `entry->pid == pid && entry->vid
  == vid` against each driver's compiled `id_table` (no class fallback), so the impersonation
  must present this exact pair (`id_table` entry at `elanmoc.c:28`). `0x0c7d` takes the
  **default 9 enroll stages** — the per-PID overrides in `elanmoc_open` (`elanmoc.c:1069-1086`)
  are `0x0c8c→11`, `0x0c99→14`, `0x0c8d→17`, none of which is our PID.
- **`0x0c7d` is also a fwupd "dodge" PID:** fwupd's `elanfp` plugin quirk
  (`plugins/elanfp/elanfp.quirk`) claims only `0c7e, 0c82, 0c88, 0c9f, 0ca3, 0ca8, 0cb0` —
  **not `0c7d`.** So on today's stock Fedora, fwupd never binds our device, exposes no
  updatable node, and issues zero firmware traffic. See §6 for why we *still* implement the
  version bluff regardless.

### 1.2 The device is a key-value store of `user_id` strings

The single most important consequence of MOC: the "template" that a real Elan chip stores is,
to the driver, opaque. At enroll, libfprint generates a **host-authored** slot label with
`fpi_print_generate_user_id(print)` →
`"FP1-<yyyymmdd>-<finger>-<rand>-<username>"` (`fpi-print.c`), writes it to the device as the
slot's `user_id`, **and embeds the identical bytes into the `FpPrint` that fprintd persists to
disk**. At verify, the chip returns a matched slot index; the driver reads that slot's
`user_id` back and compares it (`fp_print_equal`, byte-for-byte on the stored blob) against its
gallery. Nothing secret ever lives only on the device. Our emulator therefore only has to
**remember and echo `user_id` byte strings** — no biometric logic, no image transport, no
NBIS minutiae. The authoritative copy can live host-side and the guest-visible slot store is a
cache.

**Reboot stability is free.** fprintd persists each finger at
`/var/lib/fprint/<user>/elanmoc/<device_id>/<finger>` as `fp_print_serialize` bytes (a `"FP3"`
header + a GVariant carrying our `user_id`). For a stored print to still verify after a
reboot, three things must be identical: the **driver name** (fixed by our PID → `elanmoc`), the
**device_id** (elanmoc calls no `fpi_device_set_device_id` anywhere in `elanmoc.c`, and our
`iSerialNumber` is `0`, so libfprint's constant default applies and the id is stable by
construction — another reason elanmoc beats goodixmoc/synaptics, which derive identity from the
device; this rests on the absence of a setter, inferred not cited to fp-device core), and the
**`user_id` blob** (held both in fprintd's on-disk print and our per-VM store). All three hold
automatically.

## 2. Architecture — the FIDO split, reused verbatim

The mechanism/policy seam is identical to the shipped stock-tier FIDO gadget (M14 Stage C),
which is why the controller needs **no new work**: its engine already forwards interrupt *and*
bulk data endpoints generically through `handle_transfer(EpAddr, Transfer)`, and the held-IN
("NAK analogue") discipline the FIDO pipe relies on is exactly what a finger-wait read needs.

```
guest (stock, zero components):
  xhci-plat → USB core → libusb → libfprint elanmoc driver → fprintd → GNOME/GDM/PAM
        │ bulk EP0x01 (cmd OUT), EP0x83 (reply IN), EP0x84 (finger-wait IN)
libkrun (third_party):   BulkPipe  ← generic multi-endpoint bulk gadget (MECHANISM, new)
        │ EP-tagged frames over a UNIX socket (--moc-socket)
limina-vmm (worker):     moc_usb.rs ← elanmoc descriptors/identity, binds the socket
        │
limina (supervisor):     moc/ ← elanmoc protocol state machine + slot store + Touch ID (POLICY)
                               └ sep::verify()  → LAContext biometric evaluatePolicy
```

Why the split lands where it does — same logic as FIDO:

- **Policy (protocol + verify + store) must be in the supervisor.** The match decision needs
  the Secure Enclave / `LAContext`, which only works from the **Apple-Development-signed**
  supervisor (ad-hoc signing can't reach the enclave — the TCC/`limina-tcc-adhoc-accessibility`
  class). The per-VM `user_id` slot store belongs next to the existing
  `fido-credentials.json` in the managed VM's bundle dir.
- **Mechanism (the USB bus + a dumb bulk pipe) is in libkrun.** It knows nothing about
  elanmoc; it shuttles bulk packets. Upstreamable, exactly like `HidReportPipe`.
- **Identity (descriptors) is authored in the worker** (`moc_usb.rs`), hardcoded like
  `fido_usb.rs` hardcodes the FIDO descriptors — so **enumeration is independent of the
  supervisor connection**. A guest that boots before the supervisor connects still sees the
  reader; commands simply get no reply until the supervisor attaches (fprintd retries), just as
  CTAPHID INIT retries today.

### 2.1 libkrun `BulkPipe` — the new mechanism (generalizes `HidReportPipe`)

`report_pipe.rs` already does 90% of this for a single fixed-size HID interrupt endpoint pair.
`BulkPipe` is the same idea widened on three axes the fingerprint reader needs:

1. **Bulk, not HID.** Vendor-specific interface (class 0xFF), no HID report semantics; the
   caller supplies the full descriptor set (`DeviceDescriptors`), so `BulkPipe` is identity-free.
2. **Variable-length frames.** elanmoc replies range 0–97 bytes; commands up to 128. Frames are
   length-delimited on the socket, not padded to a fixed report size.
3. **Multiple IN endpoints, each independently held.** The reader has two IN endpoints with
   different semantics — `0x83` (immediate replies) and `0x84` (finger-wait). `BulkPipe` keeps a
   **per-endpoint** held-IN slot + FIFO (the existing single `held_in`/`queued` becomes a small
   map keyed by `EpAddr`). The bounded-to-one-held-per-endpoint and supersede-on-new-IN rules
   carry over unchanged (they are what keeps open/close churn from leaking transfers).

Everything else — the `Completion` one-shot, drop-stalls-the-transfer, complete-outside-the-lock
discipline, `reset()` clearing held/queued — is reused as-is. The FIDO `HidReportPipe` stays
(agent-tier and USB-tier FIDO both use it); `BulkPipe` is additive.

**Socket wire framing (worker ↔ supervisor).** A frame is `[ep: u8][kind: u8][len: u16 LE][payload]`:

- Guest→host: every bulk-OUT TD on `0x01` is forwarded as `{ep: 0x01, kind: DATA, payload}`.
- Host→guest: the supervisor emits `{ep: 0x83|0x84, kind: DATA, payload}` for a reply, or
  `{ep, kind: STALL, len: 0}` to **error a held IN** (no data). `BulkPipe` completes the
  matching endpoint's held IN with the bytes (or a stall), or queues DATA if the guest hasn't
  issued the read yet.

The `kind` byte is a correction from the bare FIDO socket (a fixed-64-byte data-only stream):
the fingerprint policy genuinely needs to **stall** a transfer — an enroll Touch-ID decline and
any protocol error must fail the guest's held `0x84` read, not deliver bytes (see §5). Without a
stall opcode there is no clean way to signal failure, so it is part of the framing from the start.

**Concurrency note (from validation).** elanmoc's command SSM only ever has **one** transfer
outstanding at a time (it posts the `0x83`/`0x84` IN read *after* the `0x01` OUT completes, and
task SSMs issue commands sequentially — `elanmoc.c` `fp_cmd_run_state`). So the two IN endpoints
are never concurrently held in any real flow; the per-endpoint held/queue map is safe but not
load-bearing (a single endpoint-keyed slot would suffice). It is kept per-endpoint for clarity
and to stay robust against open/close churn. Two facts the map must still handle: a `0x83` reply
can arrive **before** the guest posts its read (queue it — the FIFO drains on the next read), and
a superseding read stalls a stale hold (the `HidReportPipe` rule, reused).

**Transport dependency — Immediate Data (IDT) on Normal TRBs (found in live validation).** The
elanmoc commands are tiny (2–4 bytes for the open sequence), and Linux's `xhci-hcd` carries such
small bulk-OUT payloads **inline in the TRB parameter field** (`IDT=1`, xHCI §4.11.7) instead of
pointing at a DMA buffer. Our xHCI's data-TD reader originally ignored the IDT bit and always
treated `parameter` as a guest address — so the very first open command (`40 ff 00`) was read as
garbage from whatever address the immediate bytes happened to form, the engine saw an unknown
command and stalled, and `elanmoc_open` failed with *"endpoint stalled or request not supported"*.
The BulkPipe/HID gadgets never hit this because CTAPHID/HID transfers are large enough to use DMA
buffers. Fixed in `patches/libkrun/0100-*` (gather the ≤8 immediate bytes from the parameter field
when `IDT` is set; IN transfers never set it), regression-guarded by the
`immediate_data_out_delivers_inline_bytes_not_a_dma_read` xHCI unit test. This is a **general xHCI
correctness fix**, not fingerprint-specific — any future small-bulk-OUT gadget depends on it.

### 2.2 The Touch ID primitive — a new biometric-only SEP shim

Today `SepKey::sign(msg, reason)` prompts Touch ID and returns an ECDSA signature. A fingerprint
*match* needs no crypto the guest checks — just a boolean "a finger the enclave trusts was
presented." So we add **two** cdecls to `crates/limina/swift/fido_sep.swift` and wrappers in
`sep.rs`: `limina_sep_verify` (prompt) and `limina_sep_has_touchid` (the capability gate).

```swift
@_cdecl("limina_sep_has_touchid")  // 1 = a Touch ID sensor is present (hardware, not availability)
public func limina_sep_has_touchid() -> Int32 {
    let ctx = LAContext()
    _ = ctx.canEvaluatePolicy(.deviceOwnerAuthenticationWithBiometrics, error: nil) // primes biometryType
    return ctx.biometryType == .touchID ? 1 : 0
}

@_cdecl("limina_sep_verify")       // 1 = matched, 0 = declined/failed/error
public func limina_sep_verify(_ reasonPtr: UnsafePointer<CChar>) -> Int32 {
    let reason = String(cString: reasonPtr)
    let ctx = LAContext()
    // biometrics-only: a passcode fallback would misrepresent "a finger matched".
    let sem = DispatchSemaphore(value: 0)          // evaluatePolicy is ASYNC (completion handler),
    var ok = false                                  // unlike sign()'s synchronous CryptoKit call —
    ctx.evaluatePolicy(.deviceOwnerAuthenticationWithBiometrics,                // so a real
                       localizedReason: reason) { success, _ in ok = success; sem.signal() }
    sem.wait()                                       // semaphore is REQUIRED, not the sketch's fake spin
    return ok ? 1 : 0
}
```

Two corrections validation surfaced, both baked in above:

- **`evaluatePolicy` is asynchronous.** It returns immediately and fires a completion handler;
  `sign()`'s blocking shape does *not* transfer. A `DispatchSemaphore.wait()` is mandatory.
- **The capability gate is Touch ID sensor PRESENCE (`biometryType`), not `sep::available()` and
  not `canEvaluatePolicy`.** `available()` only tests `SecureEnclave.isAvailable`, which is true on
  an Apple-Silicon **desktop** (Mac mini/Studio — SEP present, *no Touch ID sensor*) that could
  never back a fingerprint match — so we don't gate on it. The first design gated on
  `canEvaluatePolicy(.biometrics)`, but **live validation proved that wrong**: `canEvaluatePolicy`
  answers "can a prompt succeed *right now*" and returns `systemCancel` when Touch ID is
  transiently unavailable — a **closed lid/clamshell**, or a Mac that needs a fresh password unlock
  to re-arm Touch ID — even though the real `evaluatePolicy` prompt works fine (verified: the same
  host returned `canEvaluatePolicy = false` but `evaluatePolicy success = true`). Gating on it hid
  the reader on a perfectly capable Mac. So we gate on **`biometryType == .touchID`** (sensor
  present, unaffected by momentary availability), mirroring how FIDO gates on hardware presence.
  Availability at verify time is the prompt's job: a failed `evaluatePolicy` degrades to a clean
  no-match, exactly like a real reader whose finger didn't match.
  `.deviceOwnerAuthenticationWithBiometrics` (not the `...OrWatch` or passcode variants) is the
  honest mapping: only an actual fingerprint counts as a match.

## 3. The USB device (descriptors)

Taken byte-for-byte from libfprint's umockdev recording `tests/elanmoc/device` (a real
`04f3:0c88` unit), with only `idProduct` changed to `0x0c7d`:

- **Device descriptor:** `bcdUSB 0x0200`, `bDeviceClass/SubClass/Protocol = 0/0/0` (class at
  interface), **`bMaxPacketSize0 = 8`**, `idVendor 0x04f3`, `idProduct 0x0c7d`, `bcdDevice
  0x8004`, `iManufacturer 1`, `iProduct 2`, `iSerialNumber 0`, `bNumConfigurations 1`.
- **Config descriptor:** `wTotalLength 0x0053 (83)`, `bNumInterfaces 1`, `bmAttributes 0xA0`
  (bus-powered + remote-wakeup), `bMaxPower 0x32` (100 mA).
- **Interface 0:** `bNumEndpoints 8`, **`bInterfaceClass 0xFF` (vendor-specific)**, subclass 0,
  protocol 0. (libfprint matches VID/PID only, but faithful class = 0xFF avoids `usbhid`
  claiming the interface.)
- **HID class descriptor** riding on the vendor interface: `bDescriptorType 0x21`, `bcdHID
  0x0110`, one report descriptor of `wDescriptorLength 0x15 (21)`. The 21 report-descriptor bytes
  are not in the capture; since no HID driver binds a class-0xFF interface, `GET_DESCRIPTOR(report)`
  is never issued — we include the class-descriptor block (it is part of the 83-byte total) and
  stall or synthesize a trivial report descriptor if anything ever asks.
- **8 bulk endpoints**, all `wMaxPacketSize 64`, `bInterval 1`: OUT `0x01/0x02/0x03/0x04`, IN
  `0x81/0x82/0x83/0x84`. We must **present all eight** (a descriptor walker/`lsusb` should see the
  real shape) but only service traffic on **OUT `0x01`, IN `0x83`, IN `0x84`**. `wTotalLength`
  checks out: 9 (config) + 9 (interface) + 9 (HID) + 8×7 (endpoints) = 83.
- **`speed() = UsbSpeed::Full`** (the real unit reports `speed=12` in `tests/elanmoc/device`).
  This is required, not cosmetic: **high-speed forces `bMaxPacketSize0 = 64`**, so the faithful
  `bMaxPacketSize0 = 8` is only legal at full speed. Full speed also keeps the 64-byte bulk
  endpoints valid (64 is a legal full-speed bulk max-packet). Enumerate as Full.

**`bMaxPacketSize0 = 8` — validation confirmed it is transparent.** The engine assembles whole
TDs and scatters the entire IN payload in one shot (`engine.rs` `scatter_in` / `read_data_td`),
never re-packetizing, and `EVALUATE_CONTEXT` is an accepting no-op — EP0 max-packet is never
consulted. So 8 works end to end. We ship the faithful 8; 64 remains a legal fallback (libfprint
performs no `bMaxPacketSize0` check) if wave-1 enumeration ever surprises us.

## 4. The elanmoc protocol engine (supervisor `moc/`)

Framing (`elanmoc_compose_cmd`, `elanmoc.h:57-62`): request = 3-byte `cmd_header` + zero-padded
payload out to `cmd_len`; reply = `resp_len` raw bytes, **status conventionally in `resp[1]`**
(an ACK is `resp[0]==0x40 && resp[1]==0x00`). Two framing rules the engine must honor exactly
(both from validation):

- **A `resp_len == 0` command gets NO reply frame.** For `40 ff 02` (finger-lift, `resp_len` 0)
  the driver posts *no* IN read at all (`fp_cmd_run_state` short-circuits when `cmd_len_in == 0`,
  `elanmoc.c:171-177`) — it only sends the OUT. The supervisor must **ack the OUT and emit
  nothing**; a stray queued frame would be delivered to the *next* command's `0x83` read and
  desync every subsequent exchange. `rlen=0` rows below are silent.
- **"user_id @ N" means the 95-byte *record* starts at offset N**, and the record is
  `[uuid0][uuid1][len][user_id string…]` — so the ASCII `user_id` string itself begins at
  `N + 3` (`elanmoc.c:830-834` builds it; `:904-915` reads it back). Watch the off-by-3 when the
  store parses commit/delete/reenroll.

The full command set we must answer:

| command | header | clen | rlen | reply we produce |
|---|---|---|---|---|
| get FW version | `40 19` | 2 | 2 | **`FF FF`** (see §6) |
| sensor dims | `00 0c` | 2 | 4 | synthesized `x_trace`/`y_trace` (e.g. `60 00 60 00`) |
| cal/ready status | `40 ff 00` | 3 | 2 | `40 03` (`resp[1]==0x03` ⇒ calibrated/ready) |
| enrolled count | `40 ff 04` | 3 | 2 | `40 <count>` from the slot store |
| set mode | `40 ff 14` (payload[3]=3) | 4 | 2 | ACK `40 00` |
| verify/identify | `40 ff 73` | 5 | 2 | **Touch ID** → `40 <slot>` or `40 fd` (no match) |
| finger-lift | `40 ff 02` | 3 | 0 | ACK (empty) |
| enroll capture | `40 ff 01` (idx/total/frame) | 7 | 2 | per-stage `40 00` (see §5) |
| enroll commit | `40 ff 11` (userid@5) | 128 | 2 | store `user_id`, ACK `40 00` |
| reenroll check | `40 ff 22` (userid@3) | 98 | 2 | `40 00` (not a duplicate) |
| delete | `40 ff 13` (userid@3) | 128 | 2 | drop matching slot, ACK `40 00` |
| get user_id | `43 21 00` (slot@2) | 3 | 97 | `43 00 <uuid0> <uuid1> <len> <user_id…>` |

`user_id` record layout (used by commit/delete/reenroll and returned by get-user_id):
`[0..1]` = UUID bytes (always `00 00`), `[2]` = `user_id_len`, `[3..]` = `user_id` bytes;
`ELAN_MAX_USER_ID_LEN = 92`, `ELAN_USERDATE_SIZE = 95`, `ELAN_MAX_ENROLL_NUM = 9` slots.

Two store invariants (from validation): the enrolled-count (`40 ff 04`) reply must **cap at 9** —
the reenroll check fails with `DATA_FULL` when it sees count `== 10` (`elanmoc.c:346-351`); and an
**empty slot's `43 21 00` reply is `resp[1] == 0xfe`** (`ELAN_MSG_AREA_NOT_ENOUGH`), which the list
walk skips.

### 4.1 Per-flow state machines

- **Open/init** (5 commands, all immediate on `0x83`, no Touch ID): `40 ff 00` until
  `resp[1]==0x03` → `40 ff 14`(mode 3)→ACK → `40 19`→`FF FF` → `00 0c`→dims → `40 ff 04`→count.
  Answering these five is the whole "device is present and healthy" contract.
- **List:** `40 ff 04` (count) then per occupied slot `43 21 00`(slot)→97-byte user_id record;
  an empty slot replies `resp[1]==0xfe` and is skipped.
- **Enroll:** `40 ff 04` → `40 ff 22`(reenroll check, `resp[1]=0x00`) → `40 ff 01` ×N
  (finger-wait on `0x84`) → `40 ff 11`(commit, `user_id` at offset 5). See §5 for the Touch ID
  and stage handling.
- **Verify/Identify** (one path — verify routes through identify): `40 ff 14`(mode) → `40 ff 73`
  (finger-wait on `0x84`). See §5.
- **Delete:** `40 ff 13` with the print's `user_id` at offset 3 → drop that slot → ACK. **Clear**
  is not wired in elanmoc's `class_init` (no `clear_storage` vfunc; `STORAGE_CLEAR` absent per
  `tests/elanmoc/custom.py`), so `40 ff 98` need not be handled beyond a benign ACK.

## 5. Touch ID mapping (the crux)

**Decided (2026-07-24): one Touch ID prompt per enroll, and a single logical finger.** The Mac's
Secure Enclave may hold several fingerprints, but `LAContext.evaluatePolicy` returns only a
boolean — *a* trusted finger matched, never which — so limina only ever learns "the authorized
human is present." Since we also never see or store a real print (the guest's "template" is just
libfprint's text `user_id`), the honest model is **one credential**. The reader therefore exposes a
**single enrollable slot**: this makes every flow (login, `sudo`, unlock, the post-enroll verify
test) always correct, with no finger-disambiguation guessing. See §5.1 for how "single slot" is
presented to the guest.

The finger-wait reads on `0x84` submit with **infinite timeout** in the driver
(`elanmoc.c:181-184`: `cmd_cancelable ? 0 : ELAN_MOC_CMD_TIMEOUT`), so a Touch ID sheet can take
as long as the user needs — **no keepalive, no 5-second collision** (the 5 s timeout applies only
to the immediate `0x83` replies, which we answer instantly). This is the property that makes the
whole mapping clean.

- **Enroll → one Touch ID prompt per enrollment.** The driver sends `40 ff 01` **once per stage,
  waiting for each reply before sending the next** (`elanmoc.c:392-399`) — the supervisor never
  pushes unsolicited frames. So: on the **first** `40 ff 01` the supervisor prompts Touch ID once
  (`sep::verify("Enroll your fingerprint in <VM>")`); on success it replies `40 00` to that and to
  **each** of the remaining 8 capture commands as they arrive (`resp` progresses `num_frames`) —
  the replies land near-instantly, so GNOME's enroll bar simply races to done — then stores the
  committed `user_id` in **the single slot** at `40 ff 11` (see §5.1). Rationale: enrollment *binds
  the one credential to "Touch ID,"* and one prompt proves the owner authorized it.

  **Enroll-decline encoding (must be exact — validation caught an infinite-loop trap).** A
  `40 <nonzero>` reply to `40 ff 01` is read as `RETRY_CENTER_FINGER` and **jumps back to
  `WAIT_FINGER`** (`elanmoc.c:389,396-399`) → the supervisor would re-prompt Touch ID forever; a
  zero-length read loops the same way (`:127-131`). The **only** clean-fail paths are a reply with
  `resp[0] != 0x40` (→ `fpi_ssm_mark_failed`, "Enrollment failed", `:374-379`) **or** a `STALL` on
  the held `0x84` IN (transfer error → `mark_failed`). We pin **STALL** (the `kind: STALL` socket
  frame from §2.1): it is the honest "the sensor gave up" signal and reuses the one error path
  every protocol fault needs. So a declined/failed Touch ID at enroll → supervisor sends
  `{ep: 0x84, kind: STALL}` → enrollment fails cleanly, no loop.
- **Verify/Identify → one Touch ID prompt.** On `40 ff 73` the supervisor prompts Touch ID; on
  success it returns `40 <slot>` (a matched slot index) and answers the follow-up `43 21 00` with
  that slot's stored `user_id`, so libfprint's `fp_print_equal` finds it in the gallery. On a
  **declined finger it returns `40 fd`** (`ELAN_MSG_VERIFY_ERR` → no match) — note this is a
  *clean completion*, NOT a stall: `resp[0] == 0x40` with `resp[1] == 0xfd` makes the identify
  report "no match" and finish (`elanmoc.c` `RSP_VERIFY_FAIL`), which is the correct "wrong finger"
  outcome. (A `STALL` here would instead surface as a device error, wrong for a normal no-match.)
  Reserve `STALL` for genuine faults. **Slot choice is trivial under the single-slot model
  (§5.1):** there is at most one occupied slot, so verify always returns it and always compares
  correctly — no finger-disambiguation, and none of the multi-finger mismatch the design would
  otherwise have (a real chip's *identify* returns which of N templates matched; we can't, since
  Touch ID never reports which finger, so we sidestep it by holding exactly one). Both `identify`
  (login/GDM/`pam_fprintd`, gallery-wide) and a specific `verify` (`fprintd-verify <finger>`, GNOME
  Settings' post-enroll test) resolve to the same one slot, so both always work.

### 5.1 Presenting a single slot to the guest

The store holds **one** `user_id`. The device still advertises the elan protocol's normal
capacity, but the policy caps enrollment at one, cleanly and without silent breakage:

- **Enrolled count (`40 ff 04`)** returns `0` before enrollment and **`FULL_COUNT` (10) after** —
  *not* `1`. The count is what arms the driver's `DATA_FULL` (which fires only at
  `curr_enrolled == ELAN_MAX_ENROLL_NUM + 1 == 10`, `elanmoc.c:346`), so reporting 10 when the one
  slot is occupied is exactly what makes a second enroll be refused. (Reporting `1` would *not*
  reject it — a second finger would enroll and silently overwrite. Do not "simplify" this to 1.)
- **Reenroll check (`40 ff 22`) when a slot is already occupied** returns the `DATA_FULL`
  condition (the reply that makes `elanmoc_enroll` fail with "device full" —
  `elanmoc.c:346-351`), so GNOME/`fprintd-enroll` reports "no space for a new fingerprint"
  instead of registering a second finger that would behave oddly. To *replace* the finger the
  user deletes first (`40 ff 13`) then enrolls — the natural gesture.
- **List (`40 ff 04` + `43 21 00`)** enumerates the one occupied slot (index 0); all other
  slots reply `resp[1] == 0xfe` (empty) and are skipped.
- **Delete (`40 ff 13`)** clears the slot, returning to the empty state.

This keeps the guest's mental model coherent (a reader that holds one finger, is "full" after
one enroll, and can be cleared and re-enrolled) while the host reality is simply "Touch ID = you."

## 6. fwupd defense-in-depth (belt **and** suspenders)

The "dodge PID" (§1.1) is a fact about *today's* fwupd quirk database, not a durable property: a
future `elanfp.quirk` release could add `0x0c7d`, or a guest could ship a newer fwupd than we
surveyed. If fwupd ever binds our device, an unanswered probe means log noise, re-probe churn,
and — worst — a **USB interface-claim race** between `fprintd` and `fwupd` on the same emulated
device (the one failure mode that could actually disrupt enrollment). So we harden on both fronts,
both always on:

1. **Dodge PID (surface minimization):** ship `0x0c7d`, which no fwupd plugin claims today, so the
   common case has zero fwupd surface.
2. **Maxed firmware version (durability):** answer `40 19` with **`FF FF` unconditionally**. This
   is *free* — libfprint's own init already requires answering `40 19` (it stores `fw_ver` but
   never gates on its value, and the per-PID enroll override keys on PID, not version). fwupd's
   `elanfp` plugin reads the identical command (`fu-elanfp-device.c:220-246`: `{0x40,0x19}` out on
   `ELAN_EP_CMD_OUT`=`0x01`, read on `ELAN_EP_IMG_IN`, `fu_memread_uint16(BE)`), so `FF FF` =
   version `"ffff"`. Its `setup()` runs **only** the version read — no other step precedes a
   version-gated update — so a maxed version leaves nothing further to probe. Two cautions from
   validation: (i) fwupd's endpoint `#define`s are **swapped** relative to libfprint
   (`ELAN_EP_CMD_IN=0x82`, `ELAN_EP_IMG_IN=0x83` in fwupd; the reverse in libfprint) — the version
   reply happens to land on `0x83` either way, but don't reason "fwupd reads where libfprint's
   `CMD_IN` points." (ii) The claim that fwupd's `PLAIN` version-compare treats `"ffff"` as
   greatest-than-any-catalog-release is asserted, not source-verified here; it is moot while `0c7d`
   is unbound, but should be confirmed (or softened) before we rely on it as the sole guard for a
   shared PID. The `enforce-requires` plugin flag is a second gate regardless.

Answering the version cleanly also makes any fwupd `setup()` open **short and successful** rather
than hanging, which is the best mitigation for the residual claim-race short of not being bound at
all. The L2 soak (§8) exercises exactly this: run `fwupdmgr get-devices`/`refresh` against the
live gadget and assert enrollment still works.

**Invariant:** *the gadget must look up-to-date and healthy to fwupd regardless of which PID it
presents.* Never ship a state where `40 19` is unanswered.

## 7. Enable surface & gating

- New `[hardware] fingerprint = false` (default off) in the VM schema + a `--fingerprint` CLI
  flag, parallel to `[hardware] usb`. **Opt-in on purpose:** a fingerprint reader changes the
  guest's login/unlock/`sudo` surface (GDM starts offering a fingerprint prompt), which the user
  should choose.
- `--fingerprint` **implies the xHCI controller** (it cannot work without the bus), the same way
  the reader rides the controller FIDO already uses. When both `fingerprint` and `usb`/FIDO are
  on, **both gadgets cold-plug** onto the one controller (ports 1 and 2) — additive, no conflict.
- Gated on **`limina_sep_has_touchid()`** (a Touch ID sensor is present, `biometryType`), *not*
  bare `sep::available()` and *not* `canEvaluatePolicy` — see §2.2: a SEP-but-no-Touch-ID desktop must not advertise a reader
  whose prompt can never succeed. A `LIMINA_FP_TEST_APPROVE` knob forces the capability on (and
  makes `sep::verify` return match without a sheet) for CI. No usable sensor → no reader advertised,
  graceful degrade, exactly like the FIDO capability.
- Supervisor plumbing mirrors FIDO Stage C: allocate a `--moc-socket` path (stable across a reboot
  relaunch), pass it to the worker (which binds), and `moc::serve(socket, store)` connects and
  reconnects.

## 8. Testing (RED-first, `crates/limina-test`)

- **Unit — pcapng replay oracle (the golden test), shape-normalized.** `tests/elanmoc/custom.pcapng`
  is a real capture of `list → enroll → list → verify → identify → delete`. Extract its
  command/response pairs and drive the supervisor's protocol engine as a pure function over command
  bytes. **It cannot be byte-equality against the recording** (validation): our design deliberately
  returns `FF FF` (≠ the recorded firmware version), synthesized sensor dims, and starts from an
  empty store — so the init/version/dims/count legs will differ. The oracle asserts the *shape and
  invariants* (correct `resp_len`, `resp[0]` family byte, status-byte semantics, `user_id` record
  framing, the verify→get-user_id two-step, `resp_len==0` ⇒ silence) for those, and exact bytes only
  where they are store-derived and deterministic (enroll `40 00` per stage — the capture unit is
  `0c88`, also 9 stages, so the enroll leg's stage count matches; delete/list of a store we seed to
  match). Keep the extracted corpus in the repo under the crate (the keepable-artifacts rule).
- **Unit — `BulkPipe`.** Per-endpoint held-IN supersede/queue/reset, variable-length frames,
  EP-tagged routing — the `report_pipe.rs` test set widened to two IN endpoints.
- **L2 (enhanced image, `fprintd` present — GNOME Workstation ships it).** With
  `LIMINA_FP_TEST_APPROVE=1` backing `sep::verify` (returns match without a real finger, the
  `LIMINA_FIDO_TEST_APPROVE` analogue): `fprintd-enroll` then `fprintd-verify` round-trip; assert
  the enroll stores a print and verify reports a match. Also `fprintd-list`/`-delete`.
- **L2 soak — fwupd claim-race guard.** `fwupdmgr get-devices` + `refresh` against the live gadget;
  assert no hang and that enrollment still succeeds afterward.
- **Human-validated (real Touch ID):** `fprintd-enroll`/`-verify` with a live sheet, GNOME Settings
  → Fingerprint enrollment, and GDM/`sudo` login via `authselect enable-feature with-fingerprint`.
  These block on the enclave and need someone at the Mac (the FIDO precedent).

## 9. Kernel/config

Stock guests need **nothing** new: `xhci-plat` is in kernel-modules-core, the interface is driven
in userspace by libusb, and `libfprint`/`fprintd` ship on Fedora Workstation. The test/enhanced
16 k kernel already gained `USB_XHCI_HCD`/`USB_XHCI_PLATFORM` for FIDO Stage B2; bulk endpoints
already work — no new kernel symbols. PAM integration is stock `authselect with-fingerprint`.

## 10. Wave plan

1. **`BulkPipe` mechanism** in libkrun + unit tests (multi-endpoint held-IN, variable-length).
   Oracle: a stock/test guest enumerates a vendor-class bulk device; `lsusb -v` shows the faithful
   8-endpoint elanmoc descriptor.
2. **Protocol engine + identity + Touch ID.** `moc_usb.rs` (descriptors, socket bind/pump) +
   supervisor `moc/` (state machine, slot store, `sep::verify` shim) + `--fingerprint`/`--moc-socket`
   wiring. Oracle: **pcapng replay unit test green**, and a stock guest binds `elanmoc`
   (`fprintd-list` runs, empty).
3. **End-to-end Touch ID.** `LIMINA_FP_TEST_APPROVE` L2 (enroll+verify+list+delete via fprintd),
   then human Touch ID enroll/verify + a GDM/`sudo` recipe. fwupd soak guard.
4. **Polish/docs.** Resolve §9 decisions from real use; write `docs/fingerprint-reader.md` (the
   user-facing feature doc + recipes, mirroring `docs/fido-authenticator.md`).

## 11. Open questions / decisions to confirm

The four **blocking** issues an adversarial validation pass raised are already resolved in this
doc (all policy-layer): enroll-decline `STALL` encoding (§5), the `limina_sep_has_touchid` capability
gate (§2.2/§7), `resp_len==0` reply suppression (§4), and the shape-normalized pcapng oracle (§8).
**Resolved by the user (2026-07-24):** enroll = **one Touch ID prompt** per enrollment (§5); the
reader holds a **single logical finger** (§5.1) — since `LAContext` only reports *that* a trusted
finger matched, never which, one credential is the honest, always-correct model.

What remains:

1. **Coexistence default:** when `--fingerprint` is set without `--usb`, silently enable the
   controller (proposed) — confirm that implicit-controller behavior is desired.
2. **`bMaxPacketSize0`:** resolved to the faithful `8` (validation confirmed it's transparent to the
   engine), with `64` as a fallback only if wave-1 enumeration surprises us. Noted, no action needed.
3. **fprintd + `PLAIN` vercmp citations (non-blocking):** before we lean on "login uses identify"
   (§5) and "`ffff` out-versions every catalog release" (§6) as guarantees, add a fprintd-source and
   an fwupd-vercmp citation. Both are moot in the common path, flagged so we don't ship an asserted
   premise as a verified one.
