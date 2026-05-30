# 02 — macOS Hypervisor.framework (and why not Virtualization.framework)

> **Scope.** How `limina`'s VMM layer (libkrun on aarch64 macOS) drives Apple's
> **Hypervisor.framework (HVF)** directly: VM/vCPU lifecycle, guest-memory
> mapping, VM-exit handling (MMIO / WFx / HVC-PSCI / sysreg traps), the GIC
> (Apple in-kernel `hv_gic` with a userspace GICv3 fallback) + ARM virtual-timer
> plumbing, IPIs/kicks and shutdown, plus
> entitlements/codesigning and a deliberate comparison with Apple's higher-level
> **Virtualization.framework (Vz)**. This is the layer beneath everything else
> (display, net, balloon, USB); the decision here — *use raw HVF via libkrun, not
> Vz* — is what lets `limina` patch the stack for dynamic memory, USB passthrough,
> and custom virtio devices.

> **Verification.** Confirmed against the local clone at `git` HEAD `07a3f40`
> (~v1.18). Every libkrun citation below is a real `path:line` that was read
> directly. Paths are relative to `~/Projects/limina/third_party/libkrun/`.
> A couple of items I did not exhaustively trace (every userspace-GICv3
> distributor MMIO register, balloon `madvise` granularity) are tagged
> **`[VERIFY]`**. Apple public-API / hardware facts are from Apple's documented
> surface.

---

## 1. Why this layer exists

On Apple Silicon there are exactly two sanctioned ways to run a hardware-virtualized guest:

1. **Hypervisor.framework (HVF)** — a thin C API (`<Hypervisor/Hypervisor.h>`)
   exposing EL2: create a VM address space, create vCPUs, map host memory into
   guest IPA, run a vCPU, decode why it exited. **You** build the GIC, the timer
   plumbing, PSCI, the device model and the boot protocol. This is what QEMU's
   `hvf` accel and **libkrun** use.
2. **Virtualization.framework (Vz)** — a high-level Obj-C/Swift framework that
   *is* a turnkey VMM: Linux device model (virtio block/net/console/rng/balloon/
   gpu/input/fs), `vmnet` integration, a bootloader, Rosetta x86 translation,
   and (recent macOS) a built-in display + guest clipboard path. You configure;
   Apple runs it.

`limina` rides on **HVF via libkrun**. The rest of this doc documents the HVF
surface libkrun actually uses and justifies the choice.

---

## 2. What exists today

### 2.1 How libkrun reaches HVF: runtime dlopen, not link-time

libkrun does **not** link `Hypervisor.framework` at build time. The raw `hv_*`
symbols live in a generated FFI module (`src/hvf/src/bindings.rs`, e.g.
`hv_vcpu_create`:4373, `hv_vm_create`:4650, `hv_vm_map`:4656) and the whole
framework is **`dlopen`-ed lazily at runtime** via `libloading`:

```rust
static HVF: LazyLock<libloading::Library> = LazyLock::new(|| unsafe {
    libloading::Library::new(
        "/System/Library/Frameworks/Hypervisor.framework/Versions/A/Hypervisor",
    ).unwrap()
});
```
(`src/hvf/src/lib.rs:233-238`)

This matters: **newer/optional symbols are probed at runtime**, so a single
libkrun binary runs across macOS versions. The nested-virt check looks up
`hv_vm_config_get_el2_supported` and tolerates its absence
(`src/hvf/src/lib.rs:210-229`); enabling nested virt looks up
`hv_vm_config_set_el2_enabled` (`lib.rs:243-256`). For `limina` this means we can
similarly probe future HVF symbols (e.g. Apple's `hv_gic_*`) without a hard
version floor.

### 2.2 Confirmed file map

| Concern | Location |
|---|---|
| Raw `hv_*` FFI (`extern "C"`) | `src/hvf/src/bindings.rs` (`hv_vcpu_create`:4373, `hv_vm_create`:4650, `hv_vm_map`:4656) |
| Safe HVF wrapper: VM, vCPU, run loop, exit decode | `src/hvf/src/lib.rs` (731 lines — the heart of this doc) |
| VMM-side vCPU: threads, TLS, emulation dispatch, WFE parking | `src/vmm/src/macos/vstate.rs` (731 lines) |
| macOS VMM module root | `src/vmm/src/macos/mod.rs` (`pub mod vstate;`) |
| aarch64 boot register/sysreg setup | `src/arch/src/aarch64/macos/{mod.rs,regs.rs,sysreg.rs}` |
| HVF MMIO bus (data-abort → device) | `src/vmm/src/device_manager/hvf/mmio.rs` |
| **GIC selection** (prefer Apple in-kernel, fall back to userspace) | `src/devices/src/legacy/irqchip.rs` (`fn create_gicv3`:42) |
| Apple **in-kernel `hv_gic` GICv3** (dlopen'd) | `src/devices/src/legacy/hvfgicv3.rs` (`HvfGicV3::new`:44, symbols :55-73, `set_irq`→`hv_gic_set_spi`:136) |
| Userspace **software GICv3** (fallback) | `src/devices/src/legacy/gicv3.rs` (`struct GicV3`:93, dist/redist MMIO decode) |
| Per-vCPU IRQ/vtimer state + kick channels | `src/devices/src/legacy/vcpu.rs` (`struct VcpuList`:68, per-CPU state:21, `set_irq_common`:29, `VTIMER_IRQ`:5) |
| Entitlements | `hvf-entitlements.plist` (repo root) |

### 2.3 Apple HVF aarch64 API surface libkrun actually calls (all confirmed)

| Function | Used at | Notes |
|---|---|---|
| `hv_vm_config_create` | `lib.rs:242` | VM config object |
| `hv_vm_config_set_el2_enabled` (optional) | `lib.rs:248` | Nested virt, M3+/macOS 15+ |
| `hv_vm_config_get_el2_supported` (optional) | `lib.rs:215` | Capability probe |
| `hv_vm_create` | `lib.rs:258` | One VM per process |
| `hv_vm_map(host_va, ipa, size, RWX)` | `lib.rs:274` | **Always maps READ\|WRITE\|EXEC** (`lib.rs:278`) |
| `hv_vm_unmap` | `lib.rs:289` | Used for remap (dynamic mappings) |
| `hv_vcpu_create` | `lib.rs:350` | **Must run on the vCPU's own thread** |
| `hv_vcpu_run` | `lib.rs:583` | Enter guest |
| `hv_vcpus_exit(&id, 1)` | `lib.rs:170` | The **kick** primitive (`vcpu_request_exit`) |
| `hv_vcpu_set_pending_interrupt(IRQ/FIQ)` | `lib.rs:189` | IRQ delivery on the **userspace-GIC** fallback path (the in-kernel `hv_gic` uses `hv_gic_set_spi` instead) |
| `hv_vcpu_set_vtimer_mask` | `lib.rs:199` | Mask/unmask the guest vtimer line |
| `hv_vcpu_get_reg / set_reg` | `lib.rs:482 / 491` | GP regs, PC, CPSR, X0… |
| `hv_vcpu_get_sys_reg / set_sys_reg` | `lib.rs:501 / 362,391…` | MPIDR, HCR_EL2, CNTHCTL_EL2, CNTV_*, ID_AA64PFR* |

**GIC backend (corrected — important):** libkrun has **two** aarch64 GIC
implementations on macOS and **prefers Apple's in-kernel `hv_gic` GICv3**, falling
back to its own userspace GICv3 only when the `hv_gic` symbols are unavailable.
The actual selection is in `build_microvm` (`src/vmm/src/builder.rs:895-899`):

```rust
// If the system supports the in-kernel GIC, use it. Otherwise, fall back to userspace.
let gic = match HvfGicV3::new(vm_resources.vm_config().vcpu_count.unwrap() as u64) {
    Ok(hvfgic) => IrqChipDevice::new(Box::new(hvfgic)),        // Apple in-kernel hv_gic
    Err(_)     => IrqChipDevice::new(Box::new(GicV3::new(vcpu_list.clone()))), // userspace
};
```
(`HvfGicV3::new` succeeds only if all `hv_gic_*` symbols resolve, `hvfgicv3.rs:52-73`;
`IrqChipDevice` is the `Box<dyn IrqChipT>` wrapper in `irqchip.rs`.)

So on macOS 15+ (which exposes `hv_gic_*`) the **in-kernel GIC is the active
path**; the software GICv3 (`gicv3.rs`) is the fallback. The `hv_gic_*` symbols
are resolved by dlopen (`hvfgicv3.rs:55-73`), consistent with §2.1. This matters
for limina: the `hv_gic` perf/maintenance win is **already in place**, not a patch
we need to write. (The comment at `lib.rs:360-361`, "when using HVF in-kernel
GICv3", refers to this path.)

### 2.4 VM / vCPU creation flow (confirmed)

1. **Entitlement gate** — `hv_vm_create` returns non-`HV_SUCCESS` (→ `Error::VmCreate`,
   `lib.rs:261`) unless the process carries `com.apple.security.hypervisor` (§2.9).
2. `HvfVm::new` (`lib.rs:241`): `hv_vm_config_create` → optionally enable EL2 →
   `hv_vm_create`. `HvfVm` is a zero-sized handle (`struct HvfVm {}`, `lib.rs:231`)
   — the VM is process-global state inside the framework.
3. **Memory:** `Vm::memory_init` (`macos/vstate.rs:111`) iterates guest memory
   regions and calls `hv_vm_map(host_addr, gpa, len)` per region
   (`vstate.rs:121-127`). `hv_vm_map` rejects GPA 0 (test note `vstate.rs:636`),
   so RAM starts at a real DRAM base.
4. **vCPU threads:** each vCPU is a host thread named `fc_vcpu N`
   (`vstate.rs:336-344`), spawned in `start_threaded`. On that thread it calls
   `init_thread_local_data` (stores a `*const Vcpu` in TLS, `vstate.rs:213`) then
   `run`. `HvfVcpu::new` (`lib.rs:334`) is called **inside** the thread because
   `hv_vcpu_create` is per-thread. It reads `cntfrq_el0` via inline asm
   (`lib.rs:341`) and writes `MPIDR_EL1 = mpidr` (`lib.rs:362`).
5. **Initial state:** `set_initial_state(entry, fdt_addr)` (`lib.rs:381`):
   - Non-nested: `CPSR = PSTATE_EL1_FAULT_BITS_64` (EL1h, DAIF masked,
     `lib.rs:456`).
   - Nested (EL2): `CPSR = EL2h`, `HCR_EL2 = HCR_EL2_BITS`, `CNTHCTL_EL2` set,
     `ID_AA64PFR0_EL1` gets EL2+GICv3 bits, SME masked out of `ID_AA64PFR1_EL1`
     (`lib.rs:382-453`).
   - `PC = entry_addr` (`lib.rs:463`), `X0 = fdt_addr` (`lib.rs:468`) — the Linux
     aarch64 boot protocol (X0 = DTB pointer).
6. **Run loop:** `Vcpu::run` (`vstate.rs:438`) waits on a per-vCPU boot channel
   for secondaries (`vstate.rs:450-454`), sets initial state, then loops calling
   `run_emulation` → `HvfVcpu::run` (`vstate.rs:460-485`).

### 2.5 VM-exit handling — the actual decode (confirmed, `lib.rs:553-730`)

There are only **three** HVF exit reasons (`lib.rs:33-35`); the run loop switches
on them first (`lib.rs:588-604`):

| `vcpu_exit.reason` | Handling |
|---|---|
| `HV_EXIT_REASON_CANCELED` (0) | A kick (`hv_vcpus_exit`) landed → `VcpuExit::Canceled`, re-enter (`lib.rs:594`). |
| `HV_EXIT_REASON_VTIMER_ACTIVATED` (2) | Set `vtimer_masked = true`, return `VtimerActivated` (`lib.rs:590-593`); VMM then raises the timer IRQ (`vstate.rs:415-418`). |
| `HV_EXIT_REASON_EXCEPTION` (1) | Decode `ESR_EL2` and sub-dispatch (below). |

For an exception, `ec = (syndrome >> 26) & 0x3f` (`lib.rs:608-609`) is matched:

| EC | Constant | Handling |
|---|---|---|
| `0x24` Data Abort | `EC_DATAABORT` | **MMIO**. Extract `iswrite`, access size `sas→len`, dest reg `srt`, fault PA from `exception.physical_address` (`lib.rs:615-650`). Sets `pending_advance_pc = true` (skips the faulting insn next entry). Write → `VcpuExit::MmioWrite(pa, buf)`; read → stash a `MmioRead{addr,srt,len}` and return `MmioRead`. The **read result is written back into the dest GP reg on the *next* `run` call** (`lib.rs:556-571`) — a deferred-completion design. |
| `0x01` WFx (WFI/WFE) | `EC_WFX_TRAP` | Read `CNTV_CTL_EL0`; if timer disabled/masked → `WaitForEvent` (block indefinitely). Else read `CNTV_CVAL_EL0`, compare to `mach_absolute_time()`: already expired → `WaitForEventExpired`; else compute a `Duration` and return `WaitForEventTimeout` (`lib.rs:705-722`). This is how idle guests sleep without burning host CPU. |
| `0x16` HVC | `EC_AA64_HVC` | → `handle_psci_request` (`lib.rs:723`). |
| `0x17` SMC | `EC_AA64_SMC` | advance PC, then `handle_psci_request` (`lib.rs:724-727`). |
| `0x18` SysReg trap | `EC_SYSTEMREGISTERTRAP` (macOS only) | Decode read/write + reg, route to `vcpu_list.handle_sysreg_read/write` (`lib.rs:652-704`). |
| `0x3c` BRK | `EC_AA64_BKPT` | `VcpuExit::Breakpoint` (`lib.rs:611-613`). |
| anything else | — | `panic!("unexpected exception")` (`lib.rs:728`). **Brittle** — a guest doing something unmodeled crashes the VMM. |

**PSCI** (`handle_psci_request`, `lib.rs:526-551`) is hand-implemented:
`PSCI_VERSION`→2, `MIGRATE_INFO_TYPE`→2, `SYSTEM_OFF`/`SYSTEM_RESET`→
`VcpuExit::Shutdown`, `CPU_ON`(`0xc400_0003`)→`VcpuExit::CpuOn(mpidr,entry,ctx)`.
Unknown function IDs `panic!` (`lib.rs:549`).

**SMP bringup:** on `CpuOn`, `run_emulation` looks up the target vCPU's boot
`Sender` and sends the entry address (`vstate.rs:371-381`); the waiting secondary
thread (parked at `vstate.rs:450-454`) wakes and boots. vCPU threads are created
up front; PSCI just releases them.

### 2.6 Interrupt model — two GIC paths

libkrun has **two** GICv3 backends and picks at runtime in `build_microvm`
(`builder.rs:895-899`, §2.3):

**Path 1 — Apple in-kernel GICv3 (`hv_gic`, the preferred/active path on macOS 15+).**
`HvfGicV3` (`hvfgicv3.rs`) dlopen's `hv_gic_create`, `hv_gic_config_*`,
`hv_gic_get_*_size`, `hv_gic_set_spi` (`:55-73`). It queries the distributor/
redistributor sizes from HVF, places the distributor and redistributors just
below `arch::MMIO_MEM_START` (`:88-94`), and creates the in-kernel GIC.
Device IRQs are raised with `hv_gic_set_spi(irq_line, true)` (`:136`). Apple's
in-kernel model handles redistributor/EOI/priority itself, so the VMM does much
less per-IRQ work than Path 2.

**Path 2 — userspace software GICv3 (`gicv3.rs`, fallback for older macOS).**
`GicV3` (`gicv3.rs:93`) models the distributor/redistributor register frames
(`GICD_CTLR`, `GICD_IROUTER[]`, `GICR_WAKER`, CoreSight IDs…) as MMIO data-aborts
on the HVF bus. On `set_irq` it routes via `gicd_irouter[irq]` to the target vCPU
and calls `vcpu_list.set_irq_common(mpid, irq)` (`gicv3.rs:400-409`). The vCPU run
loop then injects via `hv_vcpu_set_pending_interrupt` (below).

**`VcpuList` / `Vcpu`** (`vcpu.rs`) is the bridge between device threads and vCPU
threads, used by **both** paths for the WFI-wake/kick channel (and by Path 2 for
pending-IRQ queues):
- The per-vCPU state `PerCPUInterruptControllerState` (`vcpu.rs:21`) holds a
  `VecDeque<u32> pending_irqs` and an `Option<Sender<u32>> wfe_sender`.
  `set_irq_common` (`vcpu.rs:29-49`) pushes the IRQ and, if the vCPU is `Waiting`,
  **sends on the WFE channel** to wake it; if `Running`, calls `vcpu_request_exit`
  (`hv_vcpus_exit`) to kick it. `VTIMER_IRQ` is imported from
  `arch::aarch64::layout` (`vcpu.rs:5`, the standard ARM vtimer PPI = 27).
- `VcpuList` (`vcpu.rs:68`) wraps per-vCPU `Mutex<PerCPUInterruptControllerState>`
  and implements the `hvf::Vcpus` trait (`lib.rs:159-166`); `set_vtimer_irq` →
  `set_irq_common(VTIMER_IRQ)` (`vcpu.rs:116-122`).
- **Delivery (Path 2 / pending-IRQ):** before each `hv_vcpu_run`, the loop checks
  `vcpu_list.has_pending_irq(vcpuid)` and, if set, calls
  `vcpu_set_pending_irq(IRQ, true)` (`lib.rs:579-581`) →
  `hv_vcpu_set_pending_interrupt`. Parked vCPUs are woken either by the WFE
  channel send (`vcpu.rs:32`) or by `hv_vcpus_exit` (kick).
- **vtimer:** `hvf_sync_vtimer` (`lib.rs:509-524`) reads `CNTV_CTL_EL0`, raises
  the vtimer IRQ via `vcpu_list.set_vtimer_irq`, and unmasks
  (`hv_vcpu_set_vtimer_mask(false)`) once the guest stops asserting. The VMM also
  calls `set_vtimer_irq` on the `VtimerActivated` exit (`vstate.rs:415-418`).

### 2.7 Idle / WFI parking (low host CPU at idle — confirmed)

When the guest WFIs, `HvfVcpu::run` returns a `WaitForEvent*` variant
(`lib.rs:705-722`). The VMM's `Vcpu::run` then calls `wait_for_event`
(`vstate.rs:488-507`): if `vcpu_list.should_wait` (no pending IRQ,
`vcpu.rs:35-37`) it blocks on a crossbeam channel
(`receiver.recv()` or `recv_timeout(timeout)`), i.e. the **vCPU thread truly
sleeps** — no busy spin. A device raising an IRQ sends on that channel (and/or
kicks via `hv_vcpus_exit`), waking the thread. This is the basis for the
low-host-CPU-at-idle goal; worth measuring (§6).

### 2.8 Guest memory mapping

- Backing is host anonymous memory (via `GuestMemoryMmap`); `Vm::memory_init`
  maps each region with `hv_vm_map(host_va, gpa, len, RWX)` (`vstate.rs:111-131`,
  `lib.rs:267-286`). All guest RAM is mapped **RWX** unconditionally
  (`lib.rs:278`) — fine functionally, but means no W^X at the stage-2 level.
- **Dynamic remap:** `Vm::add_mapping` does `unmap` then `map`
  (`vstate.rs:133-151`) and `remove_mapping` just unmaps (`vstate.rs:153-161`),
  driven over a reply channel — the hook a balloon/shm device uses to change the
  guest physical map at runtime.
- MMIO regions are deliberately **not** mapped → touching them faults → Data
  Abort exit → device emulation (§2.5).
- **16 KiB host pages.** Apple Silicon host pages are 16 KiB; `hv_vm_map`
  alignment and any host-side `madvise` reclamation (ballooning) operate at that
  granularity. **[VERIFY]** balloon host handler `madvise` behavior in
  `src/devices/src/virtio/balloon/` (not read here). Relevant to dynamic memory
  (doc 05): a guest with a 4 KiB granule must free 4 contiguous, host-page-aligned
  guest pages before the host can reclaim a 16 KiB page.

### 2.9 Entitlements & codesigning (the #1 footgun)

`hvf-entitlements.plist` (repo root) is exactly:

```xml
<plist version="1.0"><dict>
  <key>com.apple.security.hypervisor</key><true/>
</dict></plist>
```

- **`com.apple.security.hypervisor`** is the *only* entitlement required to call
  `hv_vm_create`, and it is **freely usable** with an ad-hoc or Developer ID
  signature — no Apple provisioning needed for dev *or* distribution. Sign:
  `codesign -s - --entitlements hvf-entitlements.plist --force <binary>` (`-` =
  ad-hoc for dev; a Developer ID identity + notarization for release).
- It must be on the **executable that calls HVF**. libkrun is a dylib loaded into
  the host process, so it is **`limina`'s own binary** (and any VM-hosting worker)
  that must carry the entitlement — **not** `libkrun.dylib`. Entitlements do not
  propagate from a dylib to its host process.
- Hardened Runtime + notarization for distribution are compatible with this
  entitlement (unlike JIT, it needs no special Apple grant).
- **`vmnet` is separate and gated.** NAT/bridged `vmnet` needs
  **`com.apple.vm.networking`**, which Apple grants only via a provisioning
  profile **or** requires the process to run as **root**. This is why headless
  tooling (krunkit/vfkit) prefers **user-mode networking via gvproxy** (a
  userspace TCP/IP stack over a datagram socket — libkrun has
  `krun_add_net_unixgram`, cf. HEAD commit `07a3f40`). For `limina`: default to
  **gvproxy user-mode NAT** (no root, no gated entitlement); treat bridged/`vmnet`
  as an opt-in costing root or an Apple grant. Full matrix in the networking doc.

### 2.10 HVF limits relevant to limina

| Limit | Reality in this tree | Impact |
|---|---|---|
| Nested virt | **Conditionally supported**: probed via `hv_vm_config_get_el2_supported`, enabled via `set_el2_enabled`; only M3+/macOS 15+ (`lib.rs:208-256`). M1 Max = **no**. | No KVM-in-guest on this host. |
| In-kernel GIC | **Used by default** (`hv_gic` via `HvfGicV3`), userspace GICv3 is fallback (`builder.rs:895-899`). | Perf win already in place; no patch needed. |
| Dirty-page log | No API used; HVF historically exposes none. | Live migration/incremental snapshot hard; stop-the-world only. **[VERIFY]** macOS 26 headers for any new symbol. |
| P/E core pinning | No HVF API; vCPU = host thread, macOS schedules it. | Use QoS hints only (e.g. `QOS_CLASS_USER_INTERACTIVE`). |
| 16 KiB host pages | Inherent to Apple Silicon. | Balloon reclamation granularity; overhead accounting. |
| One VM per process | `HvfVm` is process-global. | limina = 1 VM/process; multiple VMs = multiple processes (krunkit model). |
| Unmodeled exits | `panic!` on unknown EC / PSCI fn / exit reason (`lib.rs:549,595-602,728`). | A surprising guest can abort the VMM; we may want to harden these to graceful errors. |
| Max vCPUs | **`MAX_SUPPORTED_VCPUS: u8 = 32`** (`machine_config.rs:8`) — a libkrun cap well above the M1 Max's 10 cores. The real ceiling is HVF's runtime `hv_vm_get_max_vcpu_count`, exposed as `krun_get_max_vcpus` (`libkrun/src/lib.rs:2027-2034`). | limina should query `krun_get_max_vcpus` at runtime and pick ≤ core count; no patch needed. |

---

## 3. How it works end to end

**Virtio RX interrupt + MMIO round trip:**

```
Host device thread ── data ready, set_irq → hv_gic_set_spi (in-kernel path)
   │                              OR  VcpuList.set_irq_common (userspace path) [vcpu.rs:29]
   │  send on the vCPU's WFE channel if Waiting, else hv_vcpus_exit kick   [vcpu.rs:37-47]
   ▼
vCPU thread wakes (recv) or returns from hv_vcpu_run as CANCELED        [lib.rs:594]
   │  userspace path: next run() has_pending_irq → hv_vcpu_set_pending_interrupt [lib.rs:579-581]
   │  hv_vcpu_run → guest takes the IRQ
   ▼
Guest reads GIC IAR / virtio-mmio regs ──► Data Abort exit              [lib.rs:615]
   │  EC_DATAABORT: read → stash MmioRead, return MmioRead(pa,len)
   │  VMM mmio_bus.read fills the buffer                                 [vstate.rs:386-391]
   │  NEXT run(): result written into dest GP reg                        [lib.rs:556-571]
   ▼
Guest WFIs ──► EC_WFX_TRAP ──► WaitForEvent[Timeout] ──► thread parks   [lib.rs:705-722]
```

**Timer:**

```
Guest programs CNTV_CVAL_EL0 (pass-through vtimer).
Vtimer fires while running ──► HV_EXIT_REASON_VTIMER_ACTIVATED          [lib.rs:590]
   │  vtimer_masked=true; VMM set_vtimer_irq                            [vstate.rs:415-418]
   │  next entry: hvf_sync_vtimer raises IRQ, unmasks when guest done   [lib.rs:509-524]
```

**Secondary CPU bringup:**

```
Boot vCPU: PSCI CPU_ON (HVC) ──► VcpuExit::CpuOn(mpidr,entry,ctx)       [lib.rs:542-548]
   │  run_emulation sends `entry` on target vCPU's boot channel         [vstate.rs:371-381]
   ▼
Pre-spawned secondary thread (parked) recv()s entry, set_initial_state, runs [vstate.rs:450-458]
```

**Shutdown:**

```
Guest PSCI SYSTEM_OFF/RESET (HVC) ──► VcpuExit::Shutdown                [lib.rs:536-541]
   │  run_emulation → VcpuEmulation::Stopped → vcpu.exit(OK), writes exit_evt [vstate.rs:407-410,475-477,511-519]
```

---

## 4. Options inventory for limina (HVF backend strategy)

### Option A — Reuse libkrun's HVF backend as-is (do nothing)
- **Pros:** Boots Linux on Apple Silicon today; PSCI/software-GIC/vtimer/MMIO/SMP
  all implemented; virtio gpu/net/fs/balloon for free; fastest to milestone 1.
- **Cons:** Inherit the `panic!`-on-surprise brittleness (`lib.rs:549,728`);
  RWX-everywhere stage-2 mappings; no vCPU QoS tuning.
- **Fit:** Excellent for milestone 1; good default.

### Option B — libkrun + targeted patches (HVF kept, improved) **← recommended**
- **Pros:** Keep the working base, patch only what limina needs:
  (a) harden the `panic!` exit paths into recoverable errors;
  (b) QoS hints (`QOS_CLASS_USER_INTERACTIVE`) on `fc_vcpu` threads for desktop feel;
  (c) balloon host handler made 16 KiB-aware for dynamic memory (doc 05);
  (d) net-new USB passthrough riding this backend.
  (The `hv_gic` in-kernel GIC is **already the default** and the vCPU cap is 32
  (`machine_config.rs:8`) — neither needs a patch.)
- **Cons:** Maintaining a fork; rebase cost.
- **Fit:** Best long-term — we explicitly may patch libkrun, and dynamic memory +
  USB need it.

### Option C — Write our own HVF VMM (replace libkrun)
- **Pros:** Total control, no rebase tax.
- **Cons:** Reimplement GIC, PSCI, vtimer, virtio transport, boot protocol,
  balloon, GPU — months; loses virglrenderer/rutabaga integration.
- **Fit:** Poor. Rejected unless libkrun proves unworkable.

### Option D — Apple Virtualization.framework (Vz) instead of HVF
- **What Vz hands you turnkey:** Linux boot, virtio block/net/console/rng/
  **balloon**/**gpu**/input/**virtiofs**, **vmnet** NAT/bridged without a TCP/IP
  stack, **Rosetta** for x86-64 Linux binaries, and (recent macOS) display +
  keyboard/mouse + **guest clipboard** via the Spice agent. Much of limina's list
  for free.
- **Why limina does NOT use Vz (decisive):**
  1. **Not patchable** — closed Apple framework; cannot add custom virtio
     devices, custom guest drivers/agents, or change device behavior. limina's
     mandate depends on patching the VMM/virgl/rutabaga/guest.
  2. **No host-USB passthrough** to Linux guests (only USB mass-storage attach).
     limina wants real libusb/usbredir passthrough → must build it on HVF+libkrun.
  3. **Coarse dynamic-memory control** — Vz balloon gives limited programmatic
     min..max policy + host reclamation semantics; limina wants the fine-grained
     take/return we get by owning the balloon handler (Option B-c).
  4. **Fixed GPU** — limina wants the virglrenderer/rutabaga + Venus/Vulkan path
     libkrun exposes and we can patch for 3D.
  5. **Footprint** — heavier framework + device threads vs a lean, trimmable VMM.
  6. **No networking escape** — Vz still needs `com.apple.vm.networking`/root for
     vmnet, so it does not dodge the entitlement problem; it only hides the stack.
- **Pros (honest):** Far less code; vmnet/clipboard/Rosetta/display free; Apple
  maintains it. Great *if* limina shrank to "plain Linux desktop, no custom devices."
- **Fit:** **Rejected as primary.** limina's differentiators (USB, custom 3D,
  fine-grained dynamic memory, patchable agents) are exactly what Vz forbids. Keep
  Vz as a UX reference and a fallback if an HVF limit proves fatal.

---

## 5. Recommendation

**Pursue Option B: libkrun's HVF backend, kept and selectively patched.**

It boots Linux on Apple Silicon today (fastest to milestone 1), preserves the
virglrenderer/rutabaga GPU and virtio model we need, and — unlike Vz — lets us add
USB passthrough, fine-grained dynamic memory, and custom guest agents.

**Milestone-1 plan (boot `Fedora-Workstation-43.raw`):**
1. **Sign the `limina` host binary** with `hvf-entitlements.plist`
   (`com.apple.security.hypervisor`). Gating step — without it `hv_vm_create` →
   `Error::VmCreate` (`lib.rs:261`).
2. Boot the raw image via libkrun's EFI/disk path; default to **gvproxy user-mode
   NAT** (`krun_add_net_unixgram`) so no root / no `com.apple.vm.networking`.
3. Confirm the HVF run loop, PSCI SMP bringup, vtimer, and WFI parking behave for
   a Fedora guest (M1 Max, non-nested).

**Likely libkrun patches (post-milestone):**
- **Harden exits:** turn the `panic!`s at `lib.rs:549` (unknown PSCI),
  `lib.rs:595-602` (unknown exit reason), `lib.rs:728` (unknown EC) into logged,
  recoverable errors — a desktop VMM should not abort on a surprising guest.
- **Balloon:** 16 KiB-aware `madvise` reclamation + min..max policy (doc 05); read
  `src/devices/src/virtio/balloon/` first.
- **vCPU QoS:** set `QOS_CLASS_USER_INTERACTIVE` on the `fc_vcpu` threads
  (`vstate.rs:336-344`).
- **USB:** add a virtio-usb / usbredir path (separate doc) on this backend.
- **GIC / vCPU cap:** nothing to do — `hv_gic` in-kernel GICv3 is already the
  default (`builder.rs:895-899`) and `MAX_SUPPORTED_VCPUS` is 32 (`machine_config.rs:8`).

---

## 6. Open questions / things to prototype

1. **vCPU count** — libkrun's `MAX_SUPPORTED_VCPUS = 32` (`machine_config.rs:8`)
   is well above the hardware; the real ceiling is `krun_get_max_vcpus`
   (`hv_vm_get_max_vcpu_count`). Query it and decide limina's default (≤ 10 = 8P+2E);
   evaluate whether scheduling the 2 E-cores as vCPUs helps interactive feel
   (HVF gives no P/E pinning, only QoS hints).
2. **WFI idle host-CPU cost** — measure host CPU at guest idle; confirm `fc_vcpu`
   threads truly park on the crossbeam channel (`vstate.rs:494-506`) with no spin.
3. **GIC: confirm `hv_gic` is actually selected on macOS 26** — `HvfGicV3::new`
   succeeds (`builder.rs:897`) only if all `hv_gic_*` symbols resolve; if it
   `Err`s the build falls back to the userspace `GicV3` (`builder.rs:899`).
   Benchmark IRQ latency if it falls back.
4. **Balloon reclamation on 16 KiB pages** — prove the host returns memory when
   the guest inflates; measure granularity loss; decide guest kernel granule
   (16 KiB guest granule aligns 1:1 with host pages → cleaner reclamation).
5. **Dirty tracking on macOS 26** — check current `Hypervisor.framework` headers
   for any new dirty-log/snapshot API before committing to a snapshot design.
6. **Entitlement propagation** — confirm signing only the `limina` executable (not
   the dylib) suffices, including any helper/worker process model.
7. **vmnet vs gvproxy** — confirm krunkit's approach in the clone; validate
   unprivileged gvproxy NAT for the Fedora guest; defer bridged.
8. **Exit-path hardening** — enumerate which ECs/PSCI fns a real Fedora desktop
   actually triggers, so we know which `panic!`s (`lib.rs:549,728`) are reachable.
9. **Nested virt** — confirmed unavailable on M1 Max (`set_el2_enabled` only on
   M3+/macOS 15+); affects any "Docker with KVM" guest expectation.

---

## 7. References

**Local source (confirmed `path:line`, relative to `third_party/libkrun/`):**
- `src/hvf/src/lib.rs` — safe HVF wrapper + vCPU run loop / exit decode (the core):
  HVF dlopen (233), exit-reason consts (33-35), EC consts (95-101),
  `vcpu_request_exit`/kick (168-177), `vcpu_set_pending_irq` (179-196),
  `vcpu_set_vtimer_mask` (198-206), `check_nested_virt` (210-229),
  `HvfVm::new` (241-265), `map_memory`/`hv_vm_map` RWX (267-286),
  `unmap_memory` (288-295), `HvfVcpu::new` (334-379), `set_initial_state` (381-474),
  `hvf_sync_vtimer` (509-524), `handle_psci_request` (526-551),
  `run`/decode (553-730).
- `src/hvf/src/bindings.rs` — raw `hv_*` FFI (`hv_vcpu_create`:4373, `hv_vm_create`:4650, `hv_vm_map`:4656).
- `src/vmm/src/macos/vstate.rs` — `Vm`/`Vcpu`: `memory_init` (111-131),
  `add_mapping`/`remove_mapping` (133-161), TLS (213-241), `start_threaded` (331-355),
  `run_emulation` (358-435), `run` loop (438-486), `wait_for_event` (488-507),
  `exit` (511-519); `hv_vm_map` rejects GPA 0 (636).
- `src/vmm/src/builder.rs:895-899` — macOS GIC selection (prefer `HvfGicV3`,
  fall back to userspace `GicV3`); `build_microvm` also spawns vCPUs and `VcpuList::new`:800.
- `src/devices/src/legacy/hvfgicv3.rs` — Apple in-kernel GICv3 (`HvfGicV3::new`:52,
  dlopen'd `hv_gic_*` symbols :55-71, base placement :88-94, `set_irq`→`hv_gic_set_spi`:136).
- `src/devices/src/legacy/gicv3.rs` — userspace software GICv3 fallback
  (`struct GicV3`:93, `MAXIRQ=1020`:14, `set_irq`→`set_irq_common`:400-409).
- `src/devices/src/legacy/irqchip.rs` — `IrqChipT`/`IrqChipDevice` GIC wrapper trait.
- `src/devices/src/legacy/vcpu.rs` — `struct Vcpu`(PerCPU…):21 (pending_irqs + wfe_sender),
  `set_irq_common`:29-49, `should_wait`:51-57, `struct VcpuList`:68, vtimer→`set_irq_common(VTIMER_IRQ)`:116-122.
- `src/vmm/src/vmm_config/machine_config.rs:8` — `MAX_SUPPORTED_VCPUS = 32`.
- `src/libkrun/src/lib.rs:2027-2034` — `krun_get_max_vcpus` → `hv_vm_get_max_vcpu_count`.
- `src/vmm/src/device_manager/hvf/mmio.rs` — HVF MMIO device bus / registration.
- `hvf-entitlements.plist` — `com.apple.security.hypervisor`.
- `Cargo.toml:14` — `src/hvf` is a workspace member.

**Apple / external:**
- Hypervisor.framework reference — Apple Developer (`hv_vm_*`, `hv_vcpu_*`,
  `hv_gic_*`, `hv_vm_config_*`).
- Entitlements — `com.apple.security.hypervisor`, `com.apple.vm.networking`.
- Virtualization.framework reference — Apple Developer (`VZVirtualMachine`,
  virtio gpu/fs/balloon, Rosetta on Linux, clipboard).
- QEMU `target/arm/hvf/` — reference HVF aarch64 exit handling / PSCI / vtimer.
- gvproxy / vfkit / krunkit (containers project) — user-mode networking patterns.
