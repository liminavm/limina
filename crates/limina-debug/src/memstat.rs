// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! What this process's memory is, read from the inside.
//!
//! `vmmap -summary`, `footprint`, `heap` and `sample` all suspend the task they read; on a
//! worker that freezes the guest for seconds. A process reading itself suspends nothing: the
//! region walk takes the map lock for reading, one region at a time, and the rest are counters.
//!
//! Every line is `<section> key=value …`, sizes in bytes, so a log of them can be graphed and a
//! UI can parse one without a schema:
//!
//! - `task`: the kernel's ledger for the whole task (`TASK_VM_INFO`). `footprint` is what
//!   Activity Monitor calls Memory; `compressed` can exceed it, because it counts compressed
//!   pages of objects charged to another task's footprint (memory shared with the GPU host).
//! - `zone`: one malloc zone (`malloc_zone_statistics`): live blocks, the bytes they ask for, and
//!   what the zone holds for them. A leak of small objects shows up here first.
//! - `regions`: the address space by what made each region (its VM tag, as vmmap names them),
//!   with resident, dirty and swapped (compressed or paged out) bytes. Untagged memory is split
//!   by whether it is shared, which is where guest RAM and GPU arenas live.
//! - `shared-sizes`: how many shared untagged regions there are of each size, so a run of
//!   leaked mappings of one size stands out from guest RAM's few large ones.
//! - `proc`: another process's footprint (`proc_pid_rusage`), for the helpers a process owns.

/// The report's `task`, `zone`, `regions` and `shared-sizes` lines.
pub fn report() -> Vec<String> {
    imp::report()
}

/// One line on the task alone, for a periodic trace that should stay short: footprint and the
/// default zone's live blocks, which are what move when something leaks.
pub fn summary() -> String {
    imp::summary()
}

/// A `proc` line for another process, by pid.
pub fn proc_line(label: &str, pid: i32) -> String {
    imp::proc_line(label, pid)
}

#[cfg(target_os = "macos")]
mod imp {
    use std::collections::BTreeMap;
    use std::ffi::{CStr, c_char, c_int, c_uint, c_void};
    use std::fmt::Write as _;

    /// `task_vm_info` through rev 7 (`<mach/task_info.h>`). The kernel fills at most the count
    /// it is handed and reports how much it filled, so an older kernel leaves the tail zero.
    #[repr(C)]
    #[derive(Default)]
    struct TaskVmInfo {
        virtual_size: u64,
        region_count: i32,
        page_size: i32,
        resident_size: u64,
        resident_size_peak: u64,
        device: u64,
        device_peak: u64,
        internal: u64,
        internal_peak: u64,
        external: u64,
        external_peak: u64,
        reusable: u64,
        reusable_peak: u64,
        purgeable_volatile_pmap: u64,
        purgeable_volatile_resident: u64,
        purgeable_volatile_virtual: u64,
        compressed: u64,
        compressed_peak: u64,
        compressed_lifetime: u64,
        phys_footprint: u64,
        min_address: u64,
        max_address: u64,
        ledger_phys_footprint_peak: i64,
        ledger_purgeable_nonvolatile: i64,
        ledger_purgeable_novolatile_compressed: i64,
        ledger_purgeable_volatile: i64,
        ledger_purgeable_volatile_compressed: i64,
        ledger_tag_network_nonvolatile: i64,
        ledger_tag_network_nonvolatile_compressed: i64,
        ledger_tag_network_volatile: i64,
        ledger_tag_network_volatile_compressed: i64,
        ledger_tag_media_footprint: i64,
        ledger_tag_media_footprint_compressed: i64,
        ledger_tag_media_nofootprint: i64,
        ledger_tag_media_nofootprint_compressed: i64,
        ledger_tag_graphics_footprint: i64,
        ledger_tag_graphics_footprint_compressed: i64,
        ledger_tag_graphics_nofootprint: i64,
        ledger_tag_graphics_nofootprint_compressed: i64,
        ledger_tag_neural_footprint: i64,
        ledger_tag_neural_footprint_compressed: i64,
        ledger_tag_neural_nofootprint: i64,
        ledger_tag_neural_nofootprint_compressed: i64,
        limit_bytes_remaining: u64,
        decompressions: i32,
        ledger_swapins: i64,
        ledger_tag_neural_nofootprint_total: i64,
        ledger_tag_neural_nofootprint_peak: i64,
    }

    /// `vm_region_submap_info_64` (`<mach/vm_region.h>`, which packs it to 4).
    #[repr(C, packed(4))]
    #[derive(Default, Clone, Copy)]
    struct SubmapInfo {
        protection: c_int,
        max_protection: c_int,
        inheritance: c_uint,
        offset: u64,
        user_tag: c_uint,
        pages_resident: c_uint,
        pages_shared_now_private: c_uint,
        pages_swapped_out: c_uint,
        pages_dirtied: c_uint,
        ref_count: c_uint,
        shadow_depth: u16,
        external_pager: u8,
        share_mode: u8,
        is_submap: c_int,
        behavior: c_int,
        object_id: u32,
        user_wired_count: u16,
        flags: u16,
        pages_reusable: c_uint,
        object_id_full: u64,
        pages_wired: u32,
        wire_tag: u16,
        padding0: u16,
    }

    const TASK_VM_INFO: c_uint = 22;
    const SM_SHARED: u8 = 4;
    const SM_TRUESHARED: u8 = 5;
    const SM_SHARED_ALIASED: u8 = 7;
    const SM_EMPTY: u8 = 3;
    const RUSAGE_INFO_V4: c_int = 4;

    unsafe extern "C" {
        static mach_task_self_: c_uint;
        static vm_page_size: usize;
        fn task_info(task: c_uint, flavor: c_uint, info: *mut i32, count: *mut c_uint) -> c_int;
        fn mach_vm_region_recurse(
            task: c_uint,
            address: *mut u64,
            size: *mut u64,
            depth: *mut c_uint,
            info: *mut i32,
            count: *mut c_uint,
        ) -> c_int;
        fn malloc_get_all_zones(
            task: c_uint,
            reader: *const c_void,
            addresses: *mut *mut usize,
            count: *mut c_uint,
        ) -> c_int;
        fn malloc_get_zone_name(zone: *mut libc::malloc_zone_t) -> *const c_char;
    }

    fn task_vm_info() -> Option<TaskVmInfo> {
        let mut info = TaskVmInfo::default();
        let mut count = (size_of::<TaskVmInfo>() / size_of::<i32>()) as c_uint;
        // SAFETY: a buffer of `count` naturals, which the kernel fills at most.
        let kr = unsafe {
            task_info(
                mach_task_self_,
                TASK_VM_INFO,
                &mut info as *mut TaskVmInfo as *mut i32,
                &mut count,
            )
        };
        (kr == 0).then_some(info)
    }

    pub(super) fn task_line() -> String {
        let Some(t) = task_vm_info() else {
            return "task unavailable".into();
        };
        let c = |v: i64| v.max(0);
        format!(
            "task footprint={} footprint_peak={} resident={} internal={} external={} \
             reusable={} compressed={} compressed_peak={} graphics={} graphics_compressed={} \
             graphics_nofootprint={} media={} neural={} network={} purgeable_nonvolatile={} \
             swapins={} decompressions={} regions={} virtual={}",
            t.phys_footprint,
            c(t.ledger_phys_footprint_peak),
            t.resident_size,
            t.internal,
            t.external,
            t.reusable,
            t.compressed,
            t.compressed_peak,
            c(t.ledger_tag_graphics_footprint),
            c(t.ledger_tag_graphics_footprint_compressed),
            c(t.ledger_tag_graphics_nofootprint),
            c(t.ledger_tag_media_footprint),
            c(t.ledger_tag_neural_footprint),
            c(t.ledger_tag_network_nonvolatile),
            c(t.ledger_purgeable_nonvolatile),
            c(t.ledger_swapins),
            t.decompressions.max(0),
            t.region_count,
            t.virtual_size,
        )
    }

    /// Every malloc zone: (name, statistics).
    fn zones() -> Vec<(String, libc::malloc_statistics_t)> {
        let mut addrs: *mut usize = std::ptr::null_mut();
        let mut count: c_uint = 0;
        // SAFETY: our own task with the local reader (NULL): libmalloc hands back its own array.
        let kr = unsafe {
            malloc_get_all_zones(mach_task_self_, std::ptr::null(), &mut addrs, &mut count)
        };
        if kr != 0 || addrs.is_null() {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count as usize {
            // SAFETY: `count` entries, each a live zone of this process.
            let zone = unsafe { *addrs.add(i) } as *mut libc::malloc_zone_t;
            if zone.is_null() {
                continue;
            }
            let mut stats = libc::malloc_statistics_t {
                blocks_in_use: 0,
                size_in_use: 0,
                max_size_in_use: 0,
                size_allocated: 0,
            };
            // SAFETY: a live zone; the statistics are counters the zone keeps.
            unsafe { libc::malloc_zone_statistics(zone, &mut stats) };
            // SAFETY: a zone's name is a C string it owns for its lifetime, or NULL.
            let name = unsafe {
                let p = malloc_get_zone_name(zone);
                if p.is_null() {
                    format!("{zone:p}")
                } else {
                    CStr::from_ptr(p)
                        .to_string_lossy()
                        .replace(char::is_whitespace, "_")
                }
            };
            out.push((name, stats));
        }
        out
    }

    fn zone_lines() -> Vec<String> {
        zones()
            .into_iter()
            .map(|(name, s)| {
                format!(
                    "zone name={name} blocks={} in_use={} held={} peak_in_use={}",
                    s.blocks_in_use, s.size_in_use, s.size_allocated, s.max_size_in_use
                )
            })
            .collect()
    }

    #[derive(Default)]
    struct Bucket {
        regions: u64,
        virtual_bytes: u64,
        resident: u64,
        dirty: u64,
        swapped: u64,
        reusable: u64,
    }

    /// vmmap's names for the VM tags (`<mach/vm_statistics.h>`), for the ones a worker has.
    fn tag_name(tag: u32) -> std::borrow::Cow<'static, str> {
        let name = match tag {
            1 => "malloc",
            2 => "malloc_small",
            3 => "malloc_large",
            4 => "malloc_huge",
            6 => "realloc",
            7 => "malloc_tiny",
            8 => "malloc_large_reusable",
            9 => "malloc_large_reused",
            10 => "analysis_tool",
            11 => "malloc_nano",
            12 => "malloc_medium",
            13 => "malloc_prob_guard",
            20 => "mach_msg",
            21 => "iokit",
            22 => "vm_reclaim",
            30 => "stack",
            31 => "guard",
            32 => "shared_pmap",
            33 => "dylib",
            34 => "objc_dispatchers",
            35 => "unshared_pmap",
            40 => "appkit",
            41 => "foundation",
            42 => "coregraphics",
            43 => "coreservices",
            51 => "layerkit",
            52 => "cgimage",
            54 => "coregraphics_data",
            55 => "coregraphics_shared",
            56 => "coregraphics_framebuffers",
            57 => "coregraphics_backingstores",
            60 => "dyld",
            61 => "dyld_malloc",
            66 => "glsl",
            68 => "coreimage",
            70 => "imageio",
            73 => "os_alloc_once",
            74 => "libdispatch",
            75 => "accelerate",
            76 => "coreui",
            82 => "swift_runtime",
            83 => "swift_metadata",
            88 => "iosurface",
            89 => "libnetwork",
            90 => "audio",
            91 => "videobitstream",
            92 => "cm_xpc",
            94 => "cm_memorypool",
            99 => "sanitizer",
            100 => "ioaccelerator",
            104 => "colorsync",
            108 => "debug",
            109 => "neural",
            240..=255 => return format!("application_specific_{}", tag - 239).into(),
            other => return format!("tag_{other}").into(),
        };
        name.into()
    }

    fn is_shared(mode: u8) -> bool {
        matches!(mode, SM_SHARED | SM_TRUESHARED | SM_SHARED_ALIASED)
    }

    /// Walk the address space once: buckets by kind, and the sizes of the shared untagged
    /// regions. Submaps (the shared cache) are counted as one region each and not descended:
    /// their pages belong to every process.
    fn regions() -> (BTreeMap<String, Bucket>, BTreeMap<u32, u64>) {
        // SAFETY: a read of a constant the kernel sets at exec.
        let page = unsafe { vm_page_size } as u64;
        let mut buckets: BTreeMap<String, Bucket> = BTreeMap::new();
        let mut shared_sizes: BTreeMap<u32, u64> = BTreeMap::new();
        let mut addr: u64 = 0;
        loop {
            let mut size: u64 = 0;
            let mut depth: c_uint = 0;
            let mut info = SubmapInfo::default();
            let mut count = (size_of::<SubmapInfo>() / size_of::<i32>()) as c_uint;
            // SAFETY: our own task; `info` is `count` naturals.
            let kr = unsafe {
                mach_vm_region_recurse(
                    mach_task_self_,
                    &mut addr,
                    &mut size,
                    &mut depth,
                    &mut info as *mut SubmapInfo as *mut i32,
                    &mut count,
                )
            };
            if kr != 0 || size == 0 {
                break;
            }
            let info = info;
            let key = if info.is_submap != 0 {
                "submap".to_string()
            } else if info.user_tag == 0 {
                match info.share_mode {
                    m if is_shared(m) => "shared".to_string(),
                    SM_EMPTY => "reserved".to_string(),
                    _ => "private".to_string(),
                }
            } else {
                tag_name(info.user_tag).into_owned()
            };
            let b = buckets.entry(key).or_default();
            b.regions += 1;
            b.virtual_bytes += size;
            if info.is_submap == 0 {
                b.resident += info.pages_resident as u64 * page;
                b.dirty += info.pages_dirtied as u64 * page;
                b.swapped += info.pages_swapped_out as u64 * page;
                b.reusable += info.pages_reusable as u64 * page;
                if info.user_tag == 0 && is_shared(info.share_mode) {
                    *shared_sizes.entry(63 - size.leading_zeros()).or_default() += 1;
                }
            }
            match addr.checked_add(size) {
                Some(next) => addr = next,
                None => break,
            }
        }
        (buckets, shared_sizes)
    }

    fn region_lines() -> Vec<String> {
        let (buckets, shared_sizes) = regions();
        let mut by_weight: Vec<_> = buckets.into_iter().collect();
        // Heaviest first: what the reader wants is at the top.
        by_weight.sort_by_key(|(_, b)| std::cmp::Reverse(b.dirty + b.swapped));
        let mut lines: Vec<String> = by_weight
            .into_iter()
            .map(|(kind, b)| {
                format!(
                    "regions kind={kind} count={} virtual={} resident={} dirty={} swapped={} \
                     reusable={}",
                    b.regions, b.virtual_bytes, b.resident, b.dirty, b.swapped, b.reusable
                )
            })
            .collect();
        if !shared_sizes.is_empty() {
            let mut line = "shared-sizes".to_string();
            for (log2, n) in shared_sizes {
                let _ = write!(line, " {}={n}", human_pow2(log2));
            }
            lines.push(line);
        }
        lines
    }

    /// `2^log2` bytes as a size bucket's name: `16K`, `4M`, `1G`.
    fn human_pow2(log2: u32) -> String {
        match log2 {
            0..10 => format!("{}", 1u64 << log2),
            10..20 => format!("{}K", 1u64 << (log2 - 10)),
            20..30 => format!("{}M", 1u64 << (log2 - 20)),
            _ => format!("{}G", 1u64 << (log2 - 30)),
        }
    }

    pub(super) fn report() -> Vec<String> {
        let mut lines = vec![task_line()];
        lines.extend(zone_lines());
        lines.extend(region_lines());
        lines
    }

    pub(super) fn summary() -> String {
        let (footprint, compressed) =
            task_vm_info().map_or((0, 0), |t| (t.phys_footprint, t.compressed));
        let (blocks, in_use) = zones().iter().fold((0u64, 0u64), |(b, u), (_, s)| {
            (b + s.blocks_in_use as u64, u + s.size_in_use as u64)
        });
        format!(
            "summary footprint={footprint} compressed={compressed} malloc_blocks={blocks} \
             malloc_in_use={in_use}"
        )
    }

    pub(super) fn proc_line(label: &str, pid: i32) -> String {
        // SAFETY: a zeroed plain-data struct the call fills.
        let mut ri: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
        // SAFETY: `ri` is the V4 struct the flavor names.
        let rc = unsafe {
            libc::proc_pid_rusage(
                pid,
                RUSAGE_INFO_V4,
                &mut ri as *mut libc::rusage_info_v4 as *mut libc::rusage_info_t,
            )
        };
        if rc != 0 {
            return format!(
                "proc name={label} pid={pid} unavailable={}",
                std::io::Error::last_os_error()
                    .to_string()
                    .replace(' ', "_")
            );
        }
        format!(
            "proc name={label} pid={pid} footprint={} footprint_peak={} resident={} wired={}",
            ri.ri_phys_footprint,
            ri.ri_lifetime_max_phys_footprint,
            ri.ri_resident_size,
            ri.ri_wired_size
        )
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    pub(super) fn report() -> Vec<String> {
        vec!["task unavailable (not macOS)".into()]
    }
    pub(super) fn summary() -> String {
        "summary unavailable (not macOS)".into()
    }
    pub(super) fn proc_line(label: &str, pid: i32) -> String {
        format!("proc name={label} pid={pid} unavailable=not_macOS")
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    fn field(line: &str, key: &str) -> u64 {
        line.split_whitespace()
            .find_map(|kv| kv.strip_prefix(&format!("{key}=")))
            .unwrap_or_else(|| panic!("no {key} in {line}"))
            .parse()
            .unwrap()
    }

    #[test]
    fn the_report_reads_this_process() {
        let lines = report();
        let task = &lines[0];
        assert!(task.starts_with("task "), "{task}");
        assert!(field(task, "footprint") > 0, "{task}");
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("zone name=DefaultMallocZone")
                    || l.starts_with("zone name=MallocStackLogging")),
            "{lines:?}"
        );
        // A test binary has a stack and at least one malloc region.
        assert!(
            lines.iter().any(|l| l.starts_with("regions kind=stack ")),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.starts_with("regions kind=malloc")),
            "{lines:?}"
        );
    }

    #[test]
    fn the_zones_see_a_burst_of_small_blocks() {
        let before = field(&summary(), "malloc_blocks");
        let held: Vec<Box<[u8; 240]>> = (0..20_000).map(|_| Box::new([7u8; 240])).collect();
        let after = field(&summary(), "malloc_blocks");
        // Other test threads allocate too; the burst dominates them.
        assert!(after >= before + 15_000, "{before} -> {after}");
        drop(held);
    }

    unsafe extern "C" {
        static mach_task_self_: u32;
        fn mach_vm_remap(
            target: u32,
            address: *mut u64,
            size: u64,
            mask: u64,
            flags: i32,
            src_task: u32,
            src_address: u64,
            copy: i32,
            cur: *mut i32,
            max: *mut i32,
            inherit: u32,
        ) -> i32;
    }

    #[test]
    fn memory_mapped_twice_lands_in_the_shared_bucket() {
        // Guest RAM's shape: one object, two mappings (ours and the hypervisor's). A mapping only
        // this process holds counts as private, whatever MAP_SHARED says.
        let len = 64usize << 20;
        // SAFETY: an anonymous mapping we unmap below.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_ANON,
                -1,
                0,
            )
        };
        assert_ne!(p, libc::MAP_FAILED);
        // SAFETY: inside the mapping.
        unsafe { std::ptr::write_bytes(p as *mut u8, 1, len) };
        let (mut alias, mut cur, mut max) = (0u64, 0i32, 0i32);
        // SAFETY: a second, sharing mapping of the first, anywhere (flags 1), unmapped below.
        let kr = unsafe {
            mach_vm_remap(
                mach_task_self_,
                &mut alias,
                len as u64,
                0,
                1,
                mach_task_self_,
                p as u64,
                0,
                &mut cur,
                &mut max,
                0,
            )
        };
        assert_eq!(kr, 0);
        let lines = report();
        let shared = lines
            .iter()
            .find(|l| l.starts_with("regions kind=shared "))
            .unwrap_or_else(|| panic!("{lines:?}"));
        assert!(field(shared, "dirty") >= len as u64, "{shared}");
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("shared-sizes") && l.contains(" 64M=")),
            "{lines:?}"
        );
        // SAFETY: the two mappings made above.
        unsafe {
            libc::munmap(alias as *mut libc::c_void, len);
            libc::munmap(p, len);
        }
    }

    #[test]
    fn another_process_is_read_by_pid() {
        let line = proc_line("self", std::process::id() as i32);
        assert!(field(&line, "footprint") > 0, "{line}");
        assert!(proc_line("nobody", -1).contains("unavailable="));
    }
}
