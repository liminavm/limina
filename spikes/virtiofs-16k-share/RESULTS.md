# virtiofs "Connection refused" on the 7.1.4 guest kernel — root cause + fix (2026-07-21)

## Symptom

Booting the F44 enhanced image (EFI path, guest kernel `7.1.4-limina16k`) with
`--share tools=<dir>:ro`, the guest sees the virtio-fs device and tag but
`mount -t virtiofs limina-tools /mnt` fails:

    mount: /mnt: fsconfig() failed: Connection refused.

Worker log (RUST_LOG=info) shows nothing about the share. The L1 share test
(microVM, bundled libkrunfw 6.12 kernel) is green.

## Root cause (one line)

**libkrun's virtio-fs worker reports `len=0` in every virtio used-ring element
(`queue.add_used(&mem, head.index, 0)` in
`third_party/libkrun/src/devices/src/virtio/fs/worker.rs:245`); Linux 7.1 added a
virtio-fs protocol verification that rejects any response whose used length doesn't
carry the reply (`virtio_fs_verify_response`, new between v7.0.1 and v7.1), so a
≥7.1 guest fails EVERY FUSE request — starting with FUSE_INIT — with -EIO.**

Nothing else is wrong: the host server parses INIT fine and replies success
(instrumented run shows `init: major=7 minor=45 … fs.init OK … replying OK`); the
guest kernel simply never accepts the reply because `virtqueue_get_buf()` returns
the used length 0:

    virtio-fs: response too short (0)        # guest pr_warn, fs/fuse/virtio_fs.c (v7.1.4:766-768)

(The pr_warn spells it "virtio-fs" with a dash — a dmesg grep for "virtiofs|fuse"
misses it, which is why the first forensics pass saw only the SELinux line.)

## Answer to Q1 — where ECONNREFUSED comes from

Chain, with citations into the fetched v7.1.4 sources (kept in this dir):

1. Guest sends FUSE_INIT (opcode 26, len 104 = 40 InHeader + 64 fuse_init_in) during
   `virtio_fs_fill_super` → `fuse_send_init` (async path, `virtio_fs-7.1.4.c:1614`,
   `inode-7.1.4.c:1589`).
2. Host replies OK (80 bytes written) but adds the used element with **len 0**.
3. Guest `virtio_fs_requests_done_work` → `virtio_fs_verify_response` fails
   (`len < sizeof(fuse_out_header)`) → forces `req->out.h.error = -EIO; len = 16`
   (`virtio_fs-7.1.4.c:845-846`). Observed directly with the fuse tracepoints:
   `fuse_request_end: … opcode 26 (FUSE_INIT) len 16 error -5`.
4. `process_init_reply(error=-5)` → `fc->conn_error = 1` (`inode-7.1.4.c:1356-1357`).
5. Still inside `fsconfig(FSCONFIG_CMD_CREATE)` → get_tree, SELinux's
   `sb_set_mnt_opts` does a getxattr("security.selinux") of the root inode — the
   first synchronous FUSE request — which hits `fuse_get_req`:
   `err = -ECONNREFUSED; if (fc->conn_error)` (`dev-7.1.4.c:218-222`). dmesg:
   `SELinux: (dev virtiofs, type virtiofs) getxattr errno 111`.
6. That error propagates out of fsconfig → `mount: fsconfig() failed: Connection refused`.

On a non-SELinux guest the mount would appear to succeed and every subsequent
operation would return ECONNREFUSED instead.

Why the worker log is silent: the server-side is genuinely happy (it replied OK);
and even genuine `reply_error()` replies log nothing (server.rs:1469). Only the
guest complains, in dmesg, with a dash-spelled prefix.

## Answer to Q2 — A/B matrix (all runs on the same host binaries, same day)

| # | Vehicle | Kernel (FUSE) | Pages | Share | Result |
|---|---------|--------------|-------|-------|--------|
| 1 | EFI, enhanced scratch (host-sleep-eyeball.raw) | 7.1.4-limina16k (7.45) | 16k | ro | **FAIL** ECONNREFUSED |
| 2 | same boot | 7.1.4-limina16k (7.45) | 16k | rw | **FAIL** ECONNREFUSED |
| 3 | microVM L1 | bundled 6.12 (7.41) | 4k | rw /tmp dir | OK (LIMINA_SHARE_OK) |
| 4 | microVM L1 | test Image-16k 6.12 (7.41) | **16k** | rw | OK |
| 5 | microVM L1 | 6.12 | 4k | rw **repo dir** (the exact dir that failed in #2) | OK |
| 6 | microVM L1 | **vmlinuz-7.1.4-limina16k** (7.45) | 16k | rw + virtiofs root | **FAIL** (root pivot panic; virtiofs root itself refused) |
| 7 | #6 + fixed worker (private build) | 7.1.4 | 16k | rw + root | **OK** (root mounts, LIMINA_SHARE_OK) |
| 8 | #3 + fixed worker | 6.12 | 4k | rw | OK (no regression) |
| 9 | **original symptom**: EFI enhanced scratch + fixed worker | 7.1.4 (7.45) | 16k | ro + rw | **OK** — ro mounts/reads/refuses writes, rw mounts + guest write reaches host |

Verdict per hypothesis:
- (a) 16k guest kernel: **no** — 16k @ 6.12 passes (#4); the failing axis survives
  the transplant to the microVM path (#6). It's the kernel **version**, not page size.
- (b) EFI vs microVM: **no** — reproduces on microVM with the same kernel (#6).
- (c) read-only shares: **no** — rw fails identically (#2); even the virtiofs *root* fails (#6).
- (d) libkrun fs device: **YES** — `worker.rs:245` `add_used(…, 0)`; fix validated (#7, #8).
- (e) tag/name handling: **no** — tags enumerate fine (`/sys/fs/virtiofs/*/tag`).

Kernel-side bisect of the check (git.kernel.org, files saved here):
`virtio_fs_verify_response` absent in v6.19.10 and v7.0.1, present in v7.1 →
introduced in the 7.1 merge window. So stock F44 (6.19.10) still works today —
but any distro kernel ≥ 7.1 (and our enhanced kernels from 7.1.x on) hits this.
The L1 suite stayed green because the bundled libkrunfw kernel is 6.12.

## Answer to Q3 — proposed fix

Host-side only, in the libkrun fork (mechanism in the dependency; nothing guest-side,
so both tiers keep working — old kernels ignore the used length, new kernels require
it, and reporting the real length is what the virtio spec mandates anyway):

`third_party/libkrun/src/devices/src/virtio/fs/worker.rs`, `process_queue()` — pass
`Server::handle_message`'s return value (bytes written to the device-writable
descriptors; every reply path returns `w.bytes_written()`, no-reply ops return 0)
as the used length instead of the hardcoded `0`:

    let reply_len = match self.server.handle_message(…) {
        Ok(len) => len as u32,
        Err(e) => { error!("error handling message: {e:?}"); 0 }
    };
    queue.add_used(&self.mem, head.index, reply_len)

Exact diff: `libkrun-fs-add-used-len.patch` (in this dir). Validated RED→GREEN on
the 7.1.4 kernel (#6→#7) and regression-checked on 6.12 (#8). The `third_party/libkrun`
checkout was left **clean** (instrumentation and fix reverted); the shipped
`target/debug` binaries were never rebuilt (private `CARGO_TARGET_DIR` build,
signed with `crates/limina-vmm/sign.sh`'s recipe).

To ship: apply the patch on the `limina/*` branch, re-export the series per
`patches/libkrun/README.md`, and consider it upstreamable as an obvious fix
(upstream libkrun has the same bug; any ≥7.1 guest kernel breaks against it).
This is also a candidate for the upstreaming obvious-fixes bucket
(docs/upstreaming/00-obvious-fixes-and-security.md).

Follow-ups worth considering (not done here):
- L2 gate: the L1 share test can't catch this class (bundled 6.12 kernel); a share
  mount check inside an enhanced-image boot (7.1.x kernel) would have. The venus
  L2s already boot that kernel — a `mount -t virtiofs` probe is a cheap add.
- `reply_error()` logs nothing; a `debug!`/`warn!` there would have shortened the
  forensics considerably.
- Same-pattern audit: fs is the only libkrun device using this hand-rolled
  `add_used(…, 0)` in a request/reply protocol the guest now verifies; other devices
  (block, console) already pass real lengths.

## Repro / artifacts in this dir

- `boot-with-shares.sh` — EFI+venus boot of a scratch enhanced disk with one ro +
  one rw share, spike-private worker log (`/tmp/virtiofs-16k-share-worker.log`).
- `libkrun-fs-add-used-len.patch` — the fix.
- `vmlinuz-7.1.4-limina16k` — the failing kernel, extracted from the guest;
  direct-bootable microVM repro:
  `target/debug/limina --kernel spikes/virtiofs-16k-share/vmlinuz-7.1.4-limina16k \
   --rootfs target/test-guest/rootfs --cmdline "console=ttyAMA0 rootfstype=virtiofs rw init=/init" \
   --share testshare=<dir> --console /tmp/c.log` → panics unfixed, boots fixed.
- `inode-7.1.4.c`, `virtio_fs-7.1.4.c`, `dev-7.1.4.c`, `fuse-uapi-7.1.4.h` — the
  guest kernel sources cited above (plus `/tmp/vfs-v*.c` for the bisect).
- Guest-side oracles used: `/sys/kernel/tracing/events/fuse/` tracepoints (INIT
  reply `len 16 error -5`), `dmesg | grep "virtio-fs"` (dash!) for the
  verification warnings, `dmesg | grep SELinux` for the errno-111 surface.
- share-ro/ and share-rw/ — the trivial share dirs used throughout.
