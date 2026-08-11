# xnu compressor-slot semantics for madvise/deallocate on anonymous memory

Source read: **apple-oss-distributions/xnu, tag `xnu-12377.121.6`** (newest tagged release as of 2026-08-11; macOS 26.x era). Files fetched raw at that tag: `osfmk/vm/vm_object.c`, `osfmk/vm/vm_map.c`, `bsd/kern/kern_mman.c`, `osfmk/vm/vm_compressor_pager.c`, `osfmk/arm/pmap/pmap.c`, plus headers. Line numbers below are from that tag. MADV_ZERO history checked additionally at tags `xnu-10002.1.13`, `xnu-10002.81.5`, `xnu-10063.101.15`, `xnu-11215.1.10`, `xnu-11417.140.69`.

---

## Q1: Does MADV_FREE_REUSABLE do anything to pages currently in the compressor?

**Answer: YES — it frees the compressor slot AND debits the calling task's footprint ledger for each non-resident (compressed) page in the range.** This is contrary to the field observation; see "Reconciling with the field" below.

### Path

`madvise` (kern_mman.c:1428 `MADV_FREE_REUSABLE → VM_BEHAVIOR_REUSABLE`) → `mach_vm_behavior_set` → `vm_map_behavior_set` (vm_map.c:14684) → `vm_map_reusable_pages` (vm_map.c:14994) → `vm_object_deactivate_pages(object, obj_start, obj_size, 1 /*kill_page*/, TRUE /*reusable_pages*/, …)` (vm_map.c:15096) → `deactivate_a_chunk` → `deactivate_pages_in_object` (vm_object.c:2388).

### The non-resident branch (vm_object.c:2589–2624)

When `vm_page_lookup` finds no resident page:

```c
if (page_is_paged_out(object, offset)) {
        MARK_PAGE_HANDLED(*chunk_state, p);
        /*
         * If we're killing a non-resident page, then clear the page in the existence
         * map so we don't bother paging it back in if it's touched again in the future.
         */
        if ((flags & DEACTIVATE_KILL) && (object->internal) &&
            vm_object_no_shadowing(object, true)) {
                vm_object_compressor_pager_state_clr(object, offset);

                if (pmap != PMAP_NULL) {
                        /*
                         * Tell pmap that this page
                         * is no longer mapped, to
                         * adjust the footprint ledger
                         * because this page is no
                         * longer compressed.
                         */
                        pmap_remove_options(pmap, pmap_offset,
                            (pmap_offset + PAGE_SIZE), PMAP_OPTIONS_REMOVE);
                }
        }
}
```

`DEACTIVATE_KILL` is always set on this path (`kill_page=1` at vm_map.c:15096–15097). `page_is_paged_out` (vm_object.c:2338) returns true iff `object->internal && object->alive && !object->terminating && object->pager_ready` and `vm_object_compressor_pager_state_get(...) == VM_EXTERNAL_STATE_EXISTS`.

`vm_object_compressor_pager_state_clr` (vm_object.c:4718) → `vm_compressor_pager_state_clr` (vm_compressor_pager.c:897):

```c
compressor_pager_slot_lookup(pager, FALSE, offset, &slot_p);
num_slots_freed = 0;
if (slot_p && *slot_p != 0) {
        vm_decompress_result_t result = vm_compressor_free(slot_p, 0);
        ...
```

— the compressed data is genuinely freed (`vm_compressor_free`). The follow-up `pmap_remove_options(..., PMAP_OPTIONS_REMOVE)` clears the ARM PTE "compressed marker" and debits the ledgers: in `pmap_remove_range_options` (osfmk/arm/pmap/pmap.c:4216–4241):

```c
if (pmap != kernel_pmap &&
    (options & PMAP_OPTIONS_REMOVE) &&
    (ARM_PTE_IS_COMPRESSED(spte, cpte))) {
        /* one less "compressed"... */
        num_compressed++;
        if (spte & ARM_PTE_COMPRESSED_ALT) { num_alt_compressed++; }
        /* clear marker */
        write_pte_fast(cpte, ARM_PTE_EMPTY);
```

The resident branch also drops any *stale* compressor copy of a resident page (vm_object.c:2535–2537: `if (vm_object_no_shadowing(object, true)) vm_object_compressor_pager_state_clr(object, offset);`) and marks the page `vmp_reusable`; `PMAP_OPTIONS_SET_REUSABLE` moves it out of the footprint (arm pmap.c:8117ff: `pmap_ledger_credit(reusable)`, `pmap_ledger_debit(internal)`, `pmap_ledger_debit(phys_footprint)`).

### The `all_reusable` object-level flag

Compiled out. In `vm_object_deactivate_pages` (vm_object.c:2775–2800):

```c
all_reusable = FALSE;
#if 11
        /*
         * For the sake of accurate "reusable" pmap stats, we need
         * to tell pmap about each page that is no longer "reusable",
         * so we can't do the "all_reusable" optimization.
         */
        if (size == 0) { return; }
#else
        if (reusable_page && object->internal &&
            object->vo_size != 0 && object->vo_size == size &&
            object->reusable_page_count == 0) {
                all_reusable = TRUE; ...
#endif
```

`#if 11` is always true, so the flag-setting branch is dead code. Even in the dead branch the condition is `object->vo_size == size` — the *entire* object — so a subrange of a large object could never trigger it anyway. Confirmed: for the guest-RAM scenario, only per-page processing applies.

### Preconditions that can make the whole thing a no-op

- **Preflight (hard failure, error returned):** every entry in the range must pass `vm_map_entry_is_reusable` (vm_map.c:14831 — non-malloc aliases return TRUE early, so ordinary anon mmap passes) and must be writable (`KERN_PROTECTION_FAILURE → EPERM` otherwise) (vm_map.c:15021–15033).
- **Silent no-op (returns KERN_SUCCESS!):** per-entry, the kill only runs if `vm_object_no_shadowing(object, true)` and the entry is not `iokit_acct` and has `use_pmap` (vm_map.c:15081–15106). If the object is shadowed/shadowed-flagged (e.g. after a `fork()` created COW shadows), the else-branch just bumps `vm_page_stats_reusable.reusable_pages_shared` and **succeeds without doing anything**. Note `vm_object_no_shadowing` (vm_object.c:9550) tolerates extra refs on COPY_SYMMETRIC objects — the `return false` for `extra_refs` is commented out (line 9577) — so a multi-entry clipped object still passes; only real shadow/copy objects fail.
- **Point-in-time skip:** resident pages that are wired, busy, `vmp_laundry`, or `vmp_cleaning` are skipped (vm_object.c:2450–2451). A page *mid-compression* at call time is skipped, finishes compressing afterward, and its new slot is never cleared — nothing sticky (all_reusable is dead) covers it later.
- `VM_MAP_PAGE_SHIFT(map) < PAGE_SHIFT` (4K-view map on 16K kernel): silent `KERN_SUCCESS` no-op (vm_map.c:15005–15013). Not the case for a native process.

### Reconciling with the field observation

The kernel, at this tag, does free compressor slots and debit the caller's ledger. The observed indefinite billing must come from one of (inference, ordered by likelihood for the hypervisor case):

1. **A second pmap.** `pmap_remove_options` here touches only the *calling map's* pmap. Compressed markers are per-pmap PTEs written into every internal mapping at compression time; a marker in any other pmap that mapped the page (e.g. the HV stage-2 pmap — closed-source, hypothesis only) keeps billing that mapping until *that* pmap unmaps or re-faults. Consistent with the observed ~2:1 footprint ratio.
2. **Silent-success no-op** per the shadowing/iokit branch above — `madvise` returning 0 does not prove the kill ran.
3. **Mid-compression races** (laundry/cleaning skip) leaking individual slots per call.
4. **Swapfile space:** `vm_compressor_free` frees the slot, but c_seg swapfile space is reclaimed lazily by segment compaction (vm_compressor.c — not read; low confidence on timing). Freed slots can appear to "hold swap" until compaction runs.

**Confidence: high** for the kernel behavior (every function in the chain read at the tag); **medium** for which residue mechanism explains the field numbers.

---

## Q2: MADV_ZERO — does it discard compressor slots?

**Answer: YES — compressed pages are dropped (slot freed) without faulting anything in; resident pages are zeroed in place (not freed). But unlike FRQ, there is NO pmap/ledger fix-up: phys_footprint keeps counting the dropped page as "compressed" until that VA is re-faulted.**

### Path

kern_mman.c:1444 `MADV_ZERO → VM_BEHAVIOR_ZERO` → `vm_map_zero` (vm_map.c:15260) → `vm_object_zero` (vm_object.c:3021). The vm_map.c comment states the design:

```c
/*
 * This function iterates through the entries in the requested range
 * and zeroes any resident pages in the corresponding objects. Compressed
 * pages are dropped instead of being faulted in and zeroed.
 */
```

Core loop (vm_object.c:3055–3067):

```c
/*
 * If the compressor has the page then just discard it instead
 * of faulting it in and zeroing it else zero the page if it exists.
 */
if (page_is_paged_out(object, *cur_offset_p)) {
        vm_object_compressor_pager_state_clr(object, *cur_offset_p);
} else {
        vm_object_zero_page(m);
}
```

`vm_object_zero_page` does `pmap_zero_page(phys_page_num)` — in-place zero of any resident page, including wired ones; the page stays resident and stays billed. Non-present, non-compressed offsets are left alone (next fault is zero-fill anyway).

### The ledger asymmetry (important)

`vm_object_zero` makes **no** `pmap_remove_options` call after clearing a slot (contrast Q1). The stale ARM compressed marker stays in the PTE, so `internal_compressed`/`phys_footprint` remain charged. The billing self-corrects only on re-fault: `pmap_enter_options_internal` (arm/pmap/pmap.c:6632–6645) replacing a compressed marker does:

```c
if (ARM_PTE_IS_COMPRESSED(spte, pte_p)) {
        /* One less "compressed" */
        pmap_ledger_debit(pmap, task_ledgers.internal_compressed, amount);
        if (spte & ARM_PTE_COMPRESSED_ALT) {
                pmap_ledger_debit(pmap, task_ledgers.alternate_accounting_compressed, amount);
        } else if (!skip_footprint_debit) {
                pmap_ledger_debit(pmap, task_ledgers.phys_footprint, amount);
        }
```

So: MADV_ZERO frees compressor/swap storage immediately, but the task's footprint for those pages stays stale until touch (or a later unmap of the VA). (High confidence — both sides read directly.)

### Restrictions (all checked in source; no entitlement gate anywhere)

- Entry preflight `vm_map_zero_entry_preflight` (vm_map.c:15230): must be writable, must NOT be executable, `used_for_jit`, or `vme_xnu_user_debug` (`KERN_PROTECTION_FAILURE → EPERM`); `needs_copy` (COW pending) or submap → `KERN_NO_ACCESS → ENOTSUP` ("Zeroing for copy on write isn't yet supported").
- Object preflight `vm_object_zero_preflight` (vm_object.c:2963): anonymous only (`!object->internal → KERN_PROTECTION_FAILURE`); `object->shadow != NULL || object->vo_copy != NULL → KERN_NO_ACCESS`.
- `VM_MAP_PAGE_SHIFT(map) < PAGE_SHIFT → KERN_NO_ACCESS` (vm_map.c:15278) — explicit error, not silent no-op.
- Wired pages: allowed; zeroed in place.
- Holes in the range: `VMRL_SH_STREAM_NO_HOLES` lock → error on unallocated subranges.

### Version

`case MADV_ZERO` is present in `xnu-10063.101.15` (macOS **14.4**) and absent in `xnu-10002.81.5` (macOS 14.3.x) → **introduced in macOS 14.4**, present through 15.x (`xnu-11417.140.69`) and 26.x. The SDK header comment "zero pages without faulting in additional pages" matches the implementation. **Confidence: high.**

---

## Q3: Subrange mach_vm_deallocate of a large anonymous object — are pages and compressor slots freed?

**Answer: NO — nothing in the object is freed when only a subrange is deleted. The task's *billing* is fixed (pmap remove clears PTEs and compressed markers), but the resident pages and compressor slots in the deleted offset range stay in the surviving vm_object until its last reference drops.**

### What the delete path actually does (vm_map.c:8517 `vm_map_delete_handle_entry`)

Step 3 ("Cleanup the pmap"), for an ordinary user anon entry:

```c
} else if ((VME_OBJECT(entry) != VM_OBJECT_NULL) || map->pmap == kernel_pmap) {
        /* Remove translations associated with this range ... */
        pmap_remove(map->pmap, remove_start, remove_end);   // vm_map.c:8612
}
```

`pmap_remove` ⇒ `pmap_remove_options(pmap, start, end, PMAP_OPTIONS_REMOVE)` (arm/pmap/pmap.c:4402–4407), which per Q1's snippet clears both valid PTEs and compressed markers, debiting `internal`, `internal_compressed`, and `phys_footprint`. So **the deleting task stops being billed**, including for compressed copies — but only in *its own* pmap.

Step 4 unlinks the entry into a zap list; disposal runs `_vm_map_entry_free` (vm_map.c:672–696):

```c
if (own_obj) {
        ...
        vm_object_deallocate(VME_OBJECT(entry));   // vm_map.c:681
}
```

**There is no page-freeing call for the deleted offset range.** `vm_object_page_remove` (vm_object.c:5603) exists but its only caller in the whole VM is `vm_object_coalesce` (vm_object.c:5751) — it is never invoked from the deallocate path, regardless of `ref_count`. There is no ref_count==1 fast path that trims a sub-object range.

### Refcounting: why the object survives

Clipping a map entry copies it via `_vm_map_entry_copy(original, own_obj=true)`, which takes `vm_object_reference(VME_OBJECT(original))` (vm_map.c:646–651). So after clipping a large mapping into [keep][delete][keep], the object has one reference per entry; deleting the middle entry's reference leaves `ref_count >= 1` and the object alive — with the deleted range's resident pages still on its queues and its compressor slots still allocated.

### When compressed slots DO get freed

Only when the object dies: last `vm_object_deallocate` → `vm_object_terminate` (vm_object.c:906) → `vm_object_reap` → `vm_object_reap_pages(object, REAP_REAP)` (vm_object.c:1690) frees resident pages, and the pager teardown frees every compressor slot via `compressor_pager_slots_chunk_free` (vm_compressor_pager.c:585–602, `vm_compressor_free(&chunk[i], flags)` per occupied slot). (Or `memory_object_destroy`/`vm_object_destroy` — same teardown.)

### Net effect of a subrange delete

- Task footprint: corrected (pmap side), for the caller's pmap only.
- Resident pages in the deleted range: **linger in the object**, unmapped and unreachable (no map entry covers those offsets). They are still dirty; the pageout scan will eventually compress this garbage into new compressor slots billed to no task (inference from the mechanism; the reap-only freeing is directly read). This matches the observed "orphaned graveyard" pattern: dead data held by a still-referenced object, reclaimable only when the object (or the task) dies.
- Existing compressor slots in the deleted range: **linger**, holding compressor pool/swap space, until object termination.

**Confidence: high** for "nothing freed until last ref" (delete path, clip refcounting, sole `vm_object_page_remove` caller, and reap path all read directly); **medium** for the garbage-recompression fate of orphaned resident pages (mechanism inferred, pageout scan not re-read for this).

---

## Q4: MADV_PAGEOUT — usable from a normal process?

**Answer: NO on any release kernel. It is gated by kernel build config (`MACH_ASSERT`, i.e. DEVELOPMENT/DEBUG kernels), not by entitlement. On a customer/RELEASE kernel it returns ENOTSUP from madvise, and even the raw Mach route returns KERN_INVALID_ARGUMENT.**

kern_mman.c:1437–1443:

```c
case MADV_PAGEOUT:
#if MACH_ASSERT
        new_behavior = VM_BEHAVIOR_PAGEOUT;
        break;
#else /* MACH_ASSERT */
        return ENOTSUP;
#endif /* MACH_ASSERT */
```

And the Mach side is gated identically, vm_map.c:14696–14700:

```c
#if MACH_ASSERT
case VM_BEHAVIOR_PAGEOUT:
        kr = vm_map_pageout(map, start, end);
        break;
#endif /* MACH_ASSERT */
```

(default → `KERN_INVALID_ARGUMENT`). On a development kernel, `vm_map_pageout` (vm_map.c:15174, itself inside `#if MACH_ASSERT`) walks the entries and calls `vm_object_pageout(object)` for each internal object — i.e. force-page-out, for VM testing. No entitlement check exists anywhere on the path; the "internal only" SDK comment means "internal Apple kernel builds". **Confidence: high.**

---

## Practical conclusion

For a balloon that wants "this subrange's contents are garbage — stop billing me, including the compressed copies," **`madvise(MADV_FREE_REUSABLE)` is the only mechanism that, per source, both frees the compressor slots of already-compressed pages and debits the calling pmap's footprint in the same pass** (`deactivate_pages_in_object`'s non-resident branch: `vm_object_compressor_pager_state_clr` + `pmap_remove_options(PMAP_OPTIONS_REMOVE)`). Three caveats from the same source: (1) success is not proof — a shadowed object (post-fork COW) makes the kill a silent `KERN_SUCCESS` no-op, so verify the entry/object state, not just the return code; (2) it is point-in-time — pages mid-compression (`vmp_laundry`/`vmp_cleaning`) are skipped and their slots created moments later are never cleared, so a second pass after compression settles is worthwhile; (3) it cleans only the *calling* pmap's compressed markers — billing held via markers in another pmap mapping the same object (e.g. a stage-2/HV mapping, if it participates in the pv list) is untouched, which is the best kernel-side candidate for the field observation that footprint never drops. `MADV_ZERO` (macOS 14.4+) also drops compressor slots without faulting, but performs *no* ledger fix-up — footprint stays stale until each page is re-touched — and it zeroes resident pages in place rather than freeing them, so it frees swap, not billing. Deallocating the subrange (`mach_vm_deallocate` or FIXED|OVERWRITE remap) fixes the caller's billing via `pmap_remove` but frees *nothing in the object* — resident pages and compressor slots in the deleted offsets linger until the object's last reference drops (`vm_object_reap`), leaking the contents system-wide (the graveyard pattern). `MADV_PAGEOUT` is unusable: compiled out of release kernels (`MACH_ASSERT` gate, ENOTSUP), no entitlement can unlock it. All read at tag `xnu-12377.121.6`.
