# vrend planar-YUV transfer — the bound and the access disagree

**Status:** RED reproduced on this host (2026-09-05), fix pending.
**Backlog entry:** `docs/hardening-backlog.md` §"GPU / vrend — a planar-YUV transfer is bounded
with gallium's blocksize and performed with the format table's GL triple".
**Reported by:** the Rust-virglrenderer session, which found it making the composite planar
decode-target corpus replayable. The Rust implementation is unaffected.

## What the defect is

`vrend_formats.c` registers the four planar YUV formats with a four-byte GL triple
(`GL_RGBA8, GL_RGBA, GL_UNSIGNED_BYTE`) so a converted planar blob can be sampled as RGBA.
`util_format_get_blocksize()` for those same formats is **1**. Every bound in the transfer path
is computed from the second number and every access is performed with the first, so **the access
is four times the bound**. It overruns for any iov size, a correctly sized one included: the
guard admits `w*h` bytes and GL reads `w*h*4`.

This is one instance of a general rule: **a bound and the access it guards must be derived from
one description of the format, never from two.** vrend keeps two — gallium's `util_format_*` and
`tex_conv_table` — and they disagree for exactly the formats where one was added late.

## The probe

`planar-transfer-probe.c` drives virglrenderer's own public API, the same entry points rutabaga
calls. It makes the over-read **fault deterministically** instead of depending on a sanitizer or
on whatever happens to follow a malloc:

```
mmap 2 pages → mprotect the second PROT_NONE → place the iov's 6144 honest NV12 bytes
so they END exactly at the page boundary
```

A correct transfer touches 6144 bytes. The buggy one asks GL for 64 rows × ROW_LENGTH(64)
texels × 4 bytes = 16384, walks 10240 bytes into the guard page, and takes SIGBUS.

Run it with `./run-probe.sh`, which supplies the worker's host-GL env and runs both directions as
separate processes — a caught fault leaves the GL driver's state untrustworthy for a second test.

## Measured on this host

| | result |
|---|---|
| NV12 `resource_create` | **succeeds** — `64x64 PIPE_FORMAT_Y8_U8V8_420_UNORM two-plane EGL-backed (IOSurface id 607)` |
| `TRANSFER_TO_HOST`, correctly sized iov | **SIGBUS** — the over-read reaches the guard page |
| `TRANSFER_FROM_HOST` | returns `-1`, allocating nothing |
| host vrend GL flavour | `gl_version 31 - es profile enabled` — **GLES** |

Two of those rows change the shape of the bug from what the source alone suggested:

**The create succeeds, so the transfer path is genuinely reachable here.** The Apple last-line
refusal in `vrend_resource_iosurface_init_planes` does not fire: the planar IOSurface allocates,
and a guest that consults the sampler bitmask gets exactly the resource it asked for.

**This host runs GLES, so the readback direction cannot overflow here.** `vrend_transfer_send_getteximage`
— which mallocs `w*h` and lets `glGetTexImage` write `w*h*4` into it, a heap buffer overflow — is
reached only when `!vrend_state.use_gles`. On GLES the readback goes to
`vrend_transfer_send_readonly`, which allocates nothing and returns `-1` for an iov that does not
match the attached one. The overflow remains a real defect in the shared code for any desktop-GL
host, and `GL_ARB_robustness` would bound it there (`glGetnTexImageARB` takes a size), but it is
**not** reachable on limina's macOS host. `USE_GLES` is not incidental — `GPU_COEXIST_FLAGS` sets
it because without it epoxy routes desktop-GL calls to Apple's OpenGL framework and
`virgl_egl_init` dies in `glFlush`.

So on this host the live defect is the **out-of-bounds read** on the upload direction, not a write.

## Reachability

A hostile guest reaches it directly: the sampler bitmask advertises NV12/NV21 unconditionally
(`add_sampler_only_formats` in `vrend_build_format_list_common`), which is the guest's permission
to create the resource, and nothing in `vrend_renderer_transfer_internal` refuses a planar format.
Under the two-tier guarantee a stock guest is a configuration we do not control, so that alone
earns the fix.

A benign guest's exposure is narrower than it first looks, and the reason matters for the fix
shape: **the operation has no correct meaning today in either direction.** A guest that CPU-writes
a decode target hands the host NV12 bytes which are then uploaded as RGBA; a guest that CPU-reads
one gets RGBA bytes back interpreted as NV12. Both are garbage before any bounds question. So
refusing the transfer cannot regress anything that currently works — it converts silent garbage
plus a memory-safety hole into a loud, safe failure.

Our own decode path never enters `transfer_internal` at all: decode targets skip staging
(`patches/mesa-guest/0013`) and the host fills them through `vrend_resource_upload_guest_pixels`
and `writeback_plane_to_guest`, both of which bound-check with `vrend_read_from_iovec` and refuse
on short.
