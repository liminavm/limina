# venus + libtest teardown SIGSEGV — minimal repro & analysis

## TL;DR

A wgpu **Vulkan device on venus** (`libvulkan_virtio.so`), created and dropped
**inside a libtest `#[test]`**, SIGSEGVs as the test's worker thread exits. The
*identical* code as a plain binary is clean, and the same test under **lavapipe**
is clean.

Root cause (from the core backtrace): venus registers a thread-specific-storage
destructor — `tss_create(&vn_tls_key, vn_tls_free)` in `vn_common.c`. libtest runs
each test on a worker thread that **exits while the process keeps running**, so
glibc's `__nptl_deallocate_tsd` invokes `vn_tls_free` for that thread — but by then
the thing it dereferences is gone (the venus per-thread/instance state was torn
down when `vkDestroyInstance`/`vkDestroyDevice` ran as wgpu dropped), so it jumps
into freed/unmapped memory.

This is **not** an application bug: it only fires when a venus-touching *non-main*
thread exits before the process does — exactly what a test runner does.

## The matrix

|                     | venus (`libvulkan_virtio.so`) | lavapipe (`libvulkan_lvp.so`) |
| ------------------- | ----------------------------- | ----------------------------- |
| `cargo test`        | **SIGSEGV**                   | clean                         |
| `cargo run` (binary)| clean                         | clean                         |

The two axes that matter: **venus** (lavapipe registers no such fatal TSD
destructor) and **libtest** (a worker thread that `pthread_exit`s mid-process; a
binary does the work on the main thread and exits via a different path).

## The backtrace (core dump, elfutils via `coredumpctl`)

```
Stack trace of thread 61654:                         # the libtest worker thread, exiting
#0  0x0000fffec2935300 n/a (n/a + 0x0)               # <-- jump into unmapped/freed code
#1  __GI___nptl_deallocate_tsd (libc.so.6 + 0x8afd0) # glibc running this thread's TSD destructors

Stack trace of thread 61653:                         # the main thread
#3  __pthread_clockjoin_ex (libc.so.6)
#4  std::sys::pal::unix::thread::Thread::join
#5  test::run_tests::RunningTest::join               # libtest joining the worker
#6  test::console::run_tests_console
#7  test::test_main
#9  teardown::main
```

Frame `#0` resolves to **no loaded module** (`n/a + 0x0`): the destructor target is
no longer mapped. Frame `#1`, `__nptl_deallocate_tsd`, is glibc walking the exiting
thread's pthread-key destructors and calling one of them. That destructor is
venus's `vn_tls_free` (registered via `tss_create`), and it (or the per-thread
state it frees) is gone — so the call faults.

## Why it happens

1. wgpu creates a Vulkan instance + device. The venus ICD initialises its
   thread-local state and, on first use, `tss_create(&vn_tls_key, vn_tls_free)`
   registers a TSD destructor; the calling thread's TSD slot is set non-NULL.
2. wgpu drops the device + instance → `vkDestroyDevice` / `vkDestroyInstance` tear
   down venus's per-thread/instance state.
3. The **libtest worker thread** finishes the test and exits. glibc
   (`__nptl_deallocate_tsd`) walks the thread's still-registered, still-non-NULL
   TSD slots and calls `vn_tls_free` — which now dereferences freed/unmapped
   memory → **SIGSEGV**.

A plain binary runs the work on the **main thread**, which doesn't `pthread_exit`
mid-process; the TSD destructor either runs while venus state is still valid or via
a process-exit path that doesn't fault. lavapipe registers no equivalent fatal
destructor, so it's clean either way.

## Likely fix layer — venus (mesa)

The dangling call is venus's own TSD destructor, so the fix belongs in venus
(`src/virtio/vulkan/vn_common.c`, the `vn_tls_key` / `vn_tls_free` lifecycle).
Candidate fixes for that session to evaluate:

- Make `vn_tls_free` safe to run after the instance/device it relates to has been
  destroyed (NULL the TSD value on teardown, or guard the destructor against freed
  state), so a late thread-exit destructor is a no-op rather than a fault.
- Or tie the TSD key's lifetime so it cannot fire into freed venus state.

A secondary suspect is the **Vulkan loader's** ICD unload / teardown ordering
(whether the ICD's code can be unmapped while threads with pending TSD destructors
are still alive), but the faulting destructor is venus's.

## How to run

From this directory (it's its own workspace — stock crates.io winit/wgpu, no ghost
code). Run tests **one at a time**: a SIGSEGV takes down the whole process.

```sh
export WAYLAND_DISPLAY=wayland-0 XDG_RUNTIME_DIR=/run/user/$(id -u)   # only t1/t2 need a display

# venus (default ICD): the minimal repro — headless device only, no winit
cargo test --test teardown t0_headless_device_only -- --nocapture     # → SIGSEGV

# venus: same crash with a window + surface + present (the original ghost context)
cargo test --test teardown t1_create_drop   -- --nocapture            # → SIGSEGV
cargo test --test teardown t2_present_once   -- --nocapture           # → SIGSEGV

# CONTROL 1 — lavapipe: clean (point VK_DRIVER_FILES at your lavapipe ICD)
VK_DRIVER_FILES=/usr/share/vulkan/icd.d/lvp_icd.aarch64.json \
  cargo test --test teardown t0_headless_device_only                  # → ok

# CONTROL 2 — same work as a binary on venus: clean
cargo run                                                             # → "dropped cleanly"
```

Get the backtrace from the core (gdb on some aarch64 kernels can't unwind — use
elfutils via systemd-coredump instead):

```sh
cargo test --test teardown t0_headless_device_only      # crashes, writes a core
coredumpctl info teardown                                # elfutils backtrace
```

`t0` is the canonical minimal repro: **no winit, no window, no surface, no
present** — just a headless wgpu Vulkan device created and dropped in a `#[test]`.
`t1`/`t2` add a window/surface/present only to show those don't matter.

## Environment captured here

- Driver: `/usr/lib64/libvulkan_virtio.so` (venus), Vulkan API `1.4.348`, aarch64;
  adapter `Virtio-GPU Venus (Apple M4 Pro)`.
- **The deployed driver was *not* built from the local `~/Projects/mesa` checkout**
  — the `vn_common.c:384` line reference is corroboration from that tree and may
  differ from the running build; the authoritative evidence is the core backtrace
  from the deployed `libvulkan_virtio.so`. The `vn_tls_key` / `vn_tls_free`
  mechanism is a stable part of venus.
- `wgpu 29.0.3`, `winit 0.30.13` (pinned to match ghost-ui), libtest from std.

## ghost-ui context

`frontends/ghost-ui/harness/tests/windowed.rs` hits this at teardown and works
around it by `std::process::exit(0)` after its assertions (the test's goal — real
frames presented — is verified before teardown). This crate isolates the bug from
all ghost code so it can be fixed at the venus layer.
