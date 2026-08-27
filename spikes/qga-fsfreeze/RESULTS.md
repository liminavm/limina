# fsfreeze across the suspend bracket — measured, and rejected

**Conclusion: limina must never hold a filesystem freeze across the M9.2 suspend bracket.**
A frozen root blocks the guest's *own* suspend path, so the bracket cannot complete; and once
the root is frozen, a freeze started from inside the guest cannot be undone from inside the
guest. `guest-fsfreeze-*` is only safe as a *host-driven, short-held* bracket around a
**disk-level** operation, which limina does not have a consumer for yet.

## What the guest does when you suspend a frozen VM

Measured 2026-08-26, `Fedora-Workstation-44.enhanced.raw` (kernel `7.1.8-limina16k.4`, btrfs
root on `/dev/vda3[/root]`), `cargo xtask run`-equivalent EFI+venus boot, suspend armed by
default. `fsfreeze -f /` in the guest, then `SIGTSTP` to the supervisor:

```
bracket: SIGTSTP received; pulsing the guest suspend button
bracket: guest did not quiesce within 20s (holdouts: [(5, "virtio_balloon", 15),
  (3, "hvc0", 15), (25, "virtio_snd", 15), (1, "eth0", 15), (19, "vsock", 15),
  (18, "input0", 15), (34, "virtio_i2c", 15), (4, "virtio_rng", 15), (18, "input2", 15),
  (2, "root", 15), (18, "input1", 15)]); waking it and aborting the suspend (VM lives on)
suspend bracket did not complete within 60s (guest could not quiesce); the VM keeps running
```

**Every** virtio device is a holdout — not a slow straggler but a suspend that never started.
The guest's s2idle entry needs to write (journal, systemd-suspend's own bookkeeping), and
`sb_start_write` on a frozen superblock blocks it. The abort path is sound: the bracket wakes
the guest and the VM keeps running, exactly as designed for a guest that will not quiesce.

## A freeze is not recoverable from inside the guest

Both of these hang indefinitely over a *pre-established* ssh session (no new connection
involved), and were still hung when the VM was torn down minutes later:

- `timeout 5 sudo -n touch /var/tmp/x` — `timeout` never gets to fire; the block is in `sudo`.
- `sudo -n fsfreeze -u /` — the thaw itself cannot run, for the same reason.

So `sudo` is unusable on a frozen root. Anything that would drive a freeze must be able to
drive the matching thaw over a channel that touches no disk — which is precisely
`qemu-guest-agent`: it stays answerable while frozen (it disables its own logging), and while
frozen it accepts only `guest-ping`, `guest-info`, `guest-sync`, `guest-sync-delimited`,
`guest-fsfreeze-status`, `guest-fsfreeze-thaw` (`qemu/qga/main.c`, `ga_freeze_allowlist`,
read 2026-08-26).

## What the bracket freeze would have bought anyway

Nothing for a *completed* suspend: the snapshot captures guest RAM, so page cache and disk are
coherent by construction, and s2idle entry syncs filesystems first. The only delta is on an
**abandoned** suspend (`--discard-suspend`, or a snapshot that fails to restore), where the
next cold boot replays a journal instead of mounting clean. That is worth strictly less than
a suspend that cannot complete at all.

## If a freeze is ever wired up

Three thaw paths are all mandatory, because `qemu-ga` **never** auto-thaws on Linux — not on
channel reopen, not on agent restart (`main.c`, verified 2026-08-26):

1. After the operation, on every path including every abort.
2. On **attach**, unconditionally, whenever `guest-fsfreeze-status` says frozen. `ga_set_frozen`
   writes `qga.state.isfrozen` **to disk**, and `check_is_frozen()` reads it at startup — so a
   guest that cold-boots off a disk frozen at snapshot time comes up with filesystems *not*
   frozen but the agent still refusing every non-allowlisted RPC. Only an explicit thaw clears it.
3. On resume, since a snapshot taken while frozen restores a frozen guest.
