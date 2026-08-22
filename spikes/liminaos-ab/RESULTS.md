# LiminaOS A/B rollback and recovery, on real KRUN_EFI

`rollback-recovery.sh` drives a LiminaOS guest through a failed update and back out again, on the
real boot firmware. Rollback was already green under TCG; this exists because under TCG the
component doing the recovering is *TCG's* systemd-boot. **That the two agreed is a result, not a
non-event** — they are not the same code executing, and it was genuinely unknown beforehand.

Both legs pass. Host-side reads are taken with the guest powered off.

## Leg R — rollback from a corrupt payload

The payload installs with **`RC=0`**: the directory transport has no integrity check, and the
payload's own `SHA256SUMS` was regenerated over the corrupt bytes so the guest's `sha256sum -c`
guard passes too. Everything downstream is therefore genuinely exercised.

Boot-count sequence, one host-side read per power cycle:

```
after install:        liminaos_0.1.efi  liminaos_0.2+3-0.efi
cycle 2 (no login):   liminaos_0.1.efi  liminaos_0.2+2-1.efi
cycle 3 (no login):   liminaos_0.1.efi  liminaos_0.2+1-2.efi
cycle 4 (no login):   liminaos_0.1.efi  liminaos_0.2+0-3.efi
cycle 5 (BOOTED 0.1): liminaos_0.1.efi  liminaos_0.2+0-3.efi
```

Every rename survived the power cycle, so the ESP write reaches the disk on real firmware, and the
exhausted entry sorted last rather than being refused.

Cause of each failed boot, from the guest's own console:

```
systemd-veritysetup-generator: Using data device …4dac6778… and hash device …110a3b52… for usr
device-mapper: verity: 254:4: data block 0 is corrupted
Buffer I/O error on dev dm-0, logical block 0, async page read
→ Entering emergency mode.  Cannot open access to console, the root account is locked.
```

Independently, host-side and before any boot was attempted, the freshly-installed slot scored
`CORRUPT` while the good slot scored `VERIFIED` on the same pass — so the verdict does not rest on
log greps, and the checker is not failing open.

## Leg C — recovery

```
ProtectVersion source : PROTECT_USRLIB:0.1     (%A expanded from a real version)
0.3 install exit      : 0
version after reboot  : 0.3    usrhash=097500cc…3a99526b    /usr = /dev/mapper/usr squashfs
ESP  before → after   : [0.1, 0.2+0-3] → [0.1, 0.3]
usr slots at the end  : liminaos_usr_0.1, liminaos_usr_0.3
final: 0.3 boots next VERIFIED · 0.1 FALLBACK VERIFIED · both blessed · 0 failed units
```

`InstancesMax=2` forces an eviction here, and version-sorting alone would have dropped 0.1 and kept
the dead 0.2 — leaving a known-bad fallback immediately after recovering from it. `ProtectVersion=%A`
is the only thing preventing that, and it held on **all three** transfers (ESP, usr, usr-verity).
The blast radii differ and must be scored separately: on the ESP a failure gives a bad *fallback*;
on the partition transfers it means installing over the mounted, running `/usr`.

## The finding that was not being looked for

Immediately after a **successful** rollback:

```
boots next  0.1   blessed     liminaos_0.1.efi       VERIFIED
FALLBACK    0.2   EXHAUSTED   liminaos_0.2+0-3.efi   CORRUPT
```

The machine has recovered, runs fine, reports zero failed units — and its only fallback is both
exhausted and corrupt. It is one bad block in the running slot away from having nothing to boot,
and **nothing inside the guest can observe this**: verity validates blocks on read and a dormant
slot is read by nothing; sysupdate reasons about versions, to which two slots present *is* the
healthy shape; there is no failed unit, because the degradation is not in the running system.

The obligation is therefore host-side by construction. Reporting a recovery as an unqualified
success is the defect; the moment to run the offline check is immediately after a rollback, while
the user still has a working system and can re-update. Compounding it, a verity failure parks the
guest in an emergency shell that a locked root makes unusable — so boot counting is the only
recovery path, with nothing behind it.

## What this does not show

Each cycle was power-cycled **by the host**. A verity failure leaves the guest parked in an
unusable emergency shell, so it does not reboot itself. Nothing currently implements host-side
detection and cycling, so "rollback works" is not yet "a bad update self-heals".

## Open

**How much of `/usr` does the boot path actually read?** A payload corrupted at data block 0 rolls
back; one corrupted 50 MiB in boots clean and gets blessed — same image class, opposite outcomes,
separated only by where the damage sat. If the read-on-boot set is small, verity + A/B is a
*tampering* defence being quoted as a *corruption* defence. Measurable: we own the virtio-blk
backend, so a read-trace across a boot gives it. The answer is a fraction plus which regions, not a
byte count — it is a property of this `/usr` layout and this unit set, and enabling one early unit
moves it.

## Traps this run paid for

- **A marker pattern can crowd out its own evidence.** The first evidence grep also matched
  `dracut` and `emergency`; the shutdown flood then pushed the `device-mapper: verity` line out of
  the tail, archiving a record indistinguishable from any initrd failure — the exact collapse the
  split scoring exists to prevent. Match the cause, not the aftermath.
- **The guest's userland is small: a missing binary returns BLANK, not an error.** Checks piped
  through `sed` came back silently empty. Guest-side assertions use shell builtins only.
- **A bare `target/debug/limina-vmm` has no `com.apple.security.hypervisor` entitlement** and HVF
  refuses it, which presents as a broken build. Prefer `target/Limina.app`; otherwise
  `crates/limina-vmm/sign.sh <profile>` signs in place without relinking under another session's VM.
- **Payload trees are not distinguishable by listing** — the filename encodes the *intended* root
  hash, so good and corrupt trees hold identically-named files. Discriminate on content (squashfs
  magic `hsqs` at offset 0). The harness refuses to start if the defect under test is absent.
- **Do not read the disk while the VM has it open**; take host-side reads between boots.
- **`dm-verity activated` is evidence about the hash tree, not the image.** This payload shipped a
  byte-identical, intact tree over corrupt data, so activation had nothing to object to and the
  failure came at the first data read.
