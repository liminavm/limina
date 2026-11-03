// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Which of our call sites allocated what is still live: a sampling census over Rust's allocator.
//!
//! A process installs [`Census`] as its `#[global_allocator]`. It forwards everything to the
//! system allocator, and while [`set_enabled`] has it on it also
//!
//! - counts allocations and frees, overall and per size class, so the net growth of Rust's own
//!   heap over a window can be held against the malloc zones' (`crate::memstat`): equal means the
//!   leak is ours, a gap means it is in C, Objective-C or a framework;
//! - records one allocation in every `sample_every`, with the return addresses of the stack that
//!   made it, in a fixed table it removes the entry from when that block is freed. What stays in
//!   the table is what stayed allocated, so after an hour the call sites holding the most
//!   long-lived samples are where the memory goes.
//!
//! Off, the cost is one relaxed load per allocation (and per free, while samples remain). The hook
//! never allocates: the table is static, the stack walk follows frame pointers (which every Apple
//! arm64 binary keeps) and checks each against the thread's stack bounds, and a sample that finds
//! no free slot is counted and dropped. Symbol names are looked up only when a report is made.
//!
//! Counting starts when the census is turned on, so a block allocated before and freed after
//! counts as a free: the net numbers are growth over the window, not the size of the heap.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering::*};

/// The global allocator that keeps the census. Install with
/// `#[global_allocator] static ALLOC: limina_debug::alloc_census::Census = Census;`.
pub struct Census;

/// One sample in every this many allocations, unless `set_enabled` is given another.
pub const DEFAULT_SAMPLE_EVERY: u64 = 256;

const DEPTH: usize = 32;
const CAP: usize = 1 << 16;
const MASK: usize = CAP - 1;
/// How many slots an insert or a lookup tries. A full neighbourhood drops the sample.
const PROBE: usize = 8;
const EMPTY: usize = 0;
const BUSY: usize = 1;

/// Size classes: 16-byte steps up to 1 KiB (indices 0..=64), then powers of two from 2 KiB.
const SMALL_STEPS: usize = 65;
const BUCKETS: usize = SMALL_STEPS + 32;

static ON: AtomicBool = AtomicBool::new(false);
static SAMPLE_EVERY: AtomicU64 = AtomicU64::new(DEFAULT_SAMPLE_EVERY);
static TICK: AtomicU64 = AtomicU64::new(0);
static STARTED: AtomicU64 = AtomicU64::new(0);
static ALLOCS: AtomicU64 = AtomicU64::new(0);
static FREES: AtomicU64 = AtomicU64::new(0);
static BYTES_IN: AtomicU64 = AtomicU64::new(0);
static BYTES_OUT: AtomicU64 = AtomicU64::new(0);
static SIZE_NET: [AtomicI64; BUCKETS] = [const { AtomicI64::new(0) }; BUCKETS];
static LIVE: AtomicU64 = AtomicU64::new(0);
static SAMPLED: AtomicU64 = AtomicU64::new(0);
static DROPPED: AtomicU64 = AtomicU64::new(0);

struct Slot {
    /// `EMPTY`, `BUSY` while an insert writes the rest, else the block's address.
    key: AtomicUsize,
    size: AtomicUsize,
    born: AtomicU64,
    frames: [AtomicUsize; DEPTH],
}

impl Slot {
    const fn new() -> Self {
        Self {
            key: AtomicUsize::new(EMPTY),
            size: AtomicUsize::new(0),
            born: AtomicU64::new(0),
            frames: [const { AtomicUsize::new(0) }; DEPTH],
        }
    }
}

static TABLE: [Slot; CAP] = [const { Slot::new() }; CAP];

unsafe impl GlobalAlloc for Census {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded as given.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() && ON.load(Relaxed) {
            on_alloc(p as usize, layout.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded as given.
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() && ON.load(Relaxed) {
            on_alloc(p as usize, layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // Before the free: once the system has the block back another thread can be handed the
        // same address and sample it.
        on_free(ptr as usize, layout.size());
        // SAFETY: forwarded as given.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        on_free(ptr as usize, layout.size());
        // SAFETY: forwarded as given.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if ON.load(Relaxed) {
            if p.is_null() {
                // The old block is still live; count it back (its sample, if it had one, is gone).
                on_count(layout.size(), 1);
            } else {
                on_alloc(p as usize, new_size);
            }
        }
        p
    }
}

fn bucket(size: usize) -> usize {
    if size <= 1024 {
        size.div_ceil(16)
    } else {
        let log2 = (usize::BITS - (size - 1).leading_zeros()) as usize; // ceil(log2(size)), >= 11
        (SMALL_STEPS + log2 - 11).min(BUCKETS - 1)
    }
}

/// The size class's upper bound, for the report.
fn bucket_bytes(b: usize) -> u64 {
    if b < SMALL_STEPS {
        (b as u64 * 16).max(16)
    } else {
        1u64 << (b - SMALL_STEPS + 11)
    }
}

#[inline]
fn on_count(size: usize, sign: i64) {
    if sign > 0 {
        ALLOCS.fetch_add(1, Relaxed);
        BYTES_IN.fetch_add(size as u64, Relaxed);
    } else {
        FREES.fetch_add(1, Relaxed);
        BYTES_OUT.fetch_add(size as u64, Relaxed);
    }
    SIZE_NET[bucket(size)].fetch_add(sign, Relaxed);
}

#[inline]
fn on_alloc(ptr: usize, size: usize) {
    on_count(size, 1);
    let every = SAMPLE_EVERY.load(Relaxed).max(1);
    if TICK.fetch_add(1, Relaxed).is_multiple_of(every) {
        sample(ptr, size);
    }
}

#[inline]
fn on_free(ptr: usize, size: usize) {
    if ON.load(Relaxed) {
        on_count(size, -1);
    }
    if LIVE.load(Relaxed) != 0 {
        forget(ptr);
    }
}

fn hash(ptr: usize) -> usize {
    ((ptr >> 4) as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) as usize >> (usize::BITS - 16)
}

#[inline(never)]
fn sample(ptr: usize, size: usize) {
    let mut frames = [0usize; DEPTH];
    walk(&mut frames);
    let h = hash(ptr);
    for i in 0..PROBE {
        let slot = &TABLE[(h + i) & MASK];
        if slot
            .key
            .compare_exchange(EMPTY, BUSY, Acquire, Relaxed)
            .is_ok()
        {
            slot.size.store(size, Relaxed);
            slot.born.store(now(), Relaxed);
            for (f, v) in slot.frames.iter().zip(frames) {
                f.store(v, Relaxed);
            }
            slot.key.store(ptr, Release);
            LIVE.fetch_add(1, Relaxed);
            SAMPLED.fetch_add(1, Relaxed);
            return;
        }
    }
    DROPPED.fetch_add(1, Relaxed);
}

fn forget(ptr: usize) {
    let h = hash(ptr);
    for i in 0..PROBE {
        let slot = &TABLE[(h + i) & MASK];
        if slot.key.load(Relaxed) == ptr
            && slot
                .key
                .compare_exchange(ptr, EMPTY, AcqRel, Relaxed)
                .is_ok()
        {
            LIVE.fetch_sub(1, Relaxed);
            return;
        }
    }
}

/// The return addresses of the calling stack, innermost first, by its frame-pointer chain.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[inline(never)]
fn walk(out: &mut [usize; DEPTH]) {
    // arm64e return addresses carry a pointer authentication code above the 47-bit user space.
    const ADDR: usize = (1 << 47) - 1;
    let mut fp: usize;
    // SAFETY: reads the frame-pointer register.
    unsafe {
        std::arch::asm!("mov {}, x29", out(reg) fp, options(nomem, nostack, preserves_flags))
    };
    // SAFETY: these read the calling thread's own descriptor; neither allocates.
    let (hi, size) = unsafe {
        let me = libc::pthread_self();
        (
            libc::pthread_get_stackaddr_np(me) as usize,
            libc::pthread_get_stacksize_np(me),
        )
    };
    let lo = hi.saturating_sub(size);
    let mut n = 0;
    while n < DEPTH && fp >= lo && fp.saturating_add(16) <= hi && fp.is_multiple_of(8) {
        // SAFETY: `fp` is an aligned address inside this thread's stack, 16 bytes clear of its top.
        let (next, lr) = unsafe { (*(fp as *const usize), *((fp + 8) as *const usize)) };
        let lr = lr & ADDR;
        if lr == 0 {
            break;
        }
        out[n] = lr;
        n += 1;
        if next <= fp {
            break;
        }
        fp = next;
    }
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
fn walk(_out: &mut [usize; DEPTH]) {}

#[cfg(target_os = "macos")]
#[repr(C)]
struct Timebase {
    numer: u32,
    denom: u32,
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut Timebase) -> i32;
    fn _dyld_get_image_header(index: u32) -> *const libc::c_void;
}

#[cfg(target_os = "macos")]
fn now() -> u64 {
    // SAFETY: reads a commpage clock; no syscall, no allocation.
    unsafe { mach_absolute_time() }
}

#[cfg(not(target_os = "macos"))]
fn now() -> u64 {
    0
}

/// `now()` ticks to seconds.
#[cfg(target_os = "macos")]
fn ticks_to_secs(t: u64) -> f64 {
    let mut tb = Timebase { numer: 0, denom: 0 };
    // SAFETY: fills the struct.
    unsafe { mach_timebase_info(&mut tb) };
    t as f64 * tb.numer as f64 / tb.denom.max(1) as f64 / 1e9
}

#[cfg(not(target_os = "macos"))]
fn ticks_to_secs(_t: u64) -> f64 {
    0.0
}

/// Whether the census is on.
pub fn enabled() -> bool {
    ON.load(Relaxed)
}

/// Turn the census on (zeroing its counters, keeping one sample in every `sample_every`) or off
/// (dropping every sample). Turning it on when it is on changes nothing.
pub fn set_enabled(on: bool, sample_every: u64) {
    if on {
        if ON.load(Relaxed) {
            return;
        }
        for c in [&ALLOCS, &FREES, &BYTES_IN, &BYTES_OUT, &SAMPLED, &DROPPED] {
            c.store(0, Relaxed);
        }
        for b in &SIZE_NET {
            b.store(0, Relaxed);
        }
        SAMPLE_EVERY.store(sample_every.max(1), Relaxed);
        STARTED.store(now(), Relaxed);
        ON.store(true, Release);
    } else {
        ON.store(false, Release);
        for slot in &TABLE {
            let k = slot.key.load(Relaxed);
            if k > BUSY && slot.key.compare_exchange(k, EMPTY, AcqRel, Relaxed).is_ok() {
                LIVE.fetch_sub(1, Relaxed);
            }
        }
    }
}

/// One live sample, copied out of the table.
struct Sample {
    size: usize,
    born: u64,
    frames: [usize; DEPTH],
}

fn live_samples() -> Vec<Sample> {
    let mut out = Vec::with_capacity(LIVE.load(Relaxed) as usize);
    for slot in &TABLE {
        let k = slot.key.load(Acquire);
        if k <= BUSY {
            continue;
        }
        let s = Sample {
            size: slot.size.load(Relaxed),
            born: slot.born.load(Relaxed),
            frames: std::array::from_fn(|i| slot.frames[i].load(Relaxed)),
        };
        // Freed (and maybe resampled) while we copied: skip rather than mix two samples.
        if slot.key.load(Acquire) == k {
            out.push(s);
        }
    }
    out
}

/// A demangled name with its generic arguments elided: `<Vec<…> as Extend<…>>::extend`, not
/// three hundred characters of iterator adapter types. A turbofish (`::<…>`) goes entirely.
fn shorten(name: &str) -> String {
    let mut out = String::with_capacity(name.len().min(160));
    let mut depth = 0usize;
    // The depth at which a turbofish being skipped opened, if one is.
    let mut skipping: Option<usize> = None;
    for (i, c) in name.char_indices() {
        match c {
            '<' => {
                if skipping.is_none() && name[..i].ends_with("::") {
                    skipping = Some(depth);
                    out.truncate(out.len() - 2);
                }
                depth += 1;
                if skipping.is_none() && depth <= 2 {
                    out.push(c);
                    if depth == 2 {
                        out.push('…');
                    }
                }
            }
            // The arrow of a `fn() -> T` closes nothing.
            '>' if name[..i].ends_with('-') => {
                if skipping.is_none() && depth < 2 {
                    out.push(c);
                }
            }
            '>' => {
                depth = depth.saturating_sub(1);
                if skipping == Some(depth) {
                    skipping = None;
                } else if skipping.is_none() && depth < 2 {
                    out.push(c);
                }
            }
            _ if skipping.is_none() && depth < 2 => out.push(c),
            _ => {}
        }
    }
    out
}

/// Frames that are the allocator itself, not who called it.
fn is_allocator_frame(name: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "<limina_debug::alloc_census::Census as",
        "limina_debug::alloc_census::on_alloc",
        "limina_debug::alloc_census::sample",
        "limina_debug::alloc_census::walk",
        "__rust_",
        "__rdl_",
        "__rustc::",
        "alloc::",
        "<alloc::",
        "core::",
        "<core::",
    ];
    PREFIXES.iter().any(|p| name.starts_with(p))
}

/// How many call sites a report names, and how many frames of each.
const REPORT_SITES: usize = 12;
const REPORT_FRAMES: usize = 8;
/// A sample this old is long-lived: what a leak leaves and a cache's churn does not.
const AGED_SECS: f64 = 600.0;

/// The census's report lines: `census …`, `census-sizes …`, then one `census-site …` per call
/// site, the most long-lived samples first.
pub fn report() -> Vec<String> {
    let on = ON.load(Relaxed);
    let now = now();
    let on_for = if on {
        ticks_to_secs(now.saturating_sub(STARTED.load(Relaxed)))
    } else {
        0.0
    };
    let (allocs, frees) = (ALLOCS.load(Relaxed), FREES.load(Relaxed));
    let (bin, bout) = (BYTES_IN.load(Relaxed), BYTES_OUT.load(Relaxed));
    let mut lines = vec![format!(
        "census on={} on_for_s={on_for:.0} sample_every={} allocs={allocs} frees={frees} \
         net_blocks={} net_bytes={} samples_live={} samples_taken={} samples_dropped={} table={CAP}",
        on as u8,
        SAMPLE_EVERY.load(Relaxed),
        allocs as i64 - frees as i64,
        bin as i64 - bout as i64,
        LIVE.load(Relaxed),
        SAMPLED.load(Relaxed),
        DROPPED.load(Relaxed),
    )];
    if !on {
        return lines;
    }

    let mut sizes: Vec<(usize, i64)> = SIZE_NET
        .iter()
        .enumerate()
        .map(|(b, n)| (b, n.load(Relaxed)))
        .filter(|&(_, n)| n > 0)
        .collect();
    sizes.sort_by_key(|&(_, n)| std::cmp::Reverse(n));
    let mut line = "census-sizes".to_string();
    for (b, n) in sizes.into_iter().take(12) {
        line += &format!(" le{}=+{n}", bucket_bytes(b));
    }
    lines.push(line);

    #[derive(Default)]
    struct Site {
        n: u64,
        bytes: u64,
        aged: u64,
        oldest: f64,
    }
    let mut names = symbols::Cache::default();
    let mut sites: std::collections::HashMap<Vec<String>, Site> = Default::default();
    for s in live_samples() {
        let key: Vec<String> = s
            .frames
            .iter()
            .take_while(|&&a| a != 0)
            .map(|&a| names.name(a))
            .filter(|n| !is_allocator_frame(n))
            .take(REPORT_FRAMES)
            .collect();
        let age = ticks_to_secs(now.saturating_sub(s.born));
        let site = sites.entry(key).or_default();
        site.n += 1;
        site.bytes += s.size as u64;
        if age >= AGED_SECS {
            site.aged += 1;
        }
        site.oldest = site.oldest.max(age);
    }
    let mut sites: Vec<_> = sites.into_iter().collect();
    sites.sort_by_key(|(_, s)| std::cmp::Reverse((s.aged, s.n)));
    for (rank, (frames, s)) in sites.into_iter().take(REPORT_SITES).enumerate() {
        lines.push(format!(
            "census-site rank={} samples={} bytes={} aged={} oldest_s={:.0} at={}",
            rank + 1,
            s.n,
            s.bytes,
            s.aged,
            s.oldest,
            if frames.is_empty() {
                "?".to_string()
            } else {
                frames.join(" < ")
            }
        ));
    }
    lines
}

/// Names for return addresses: the executable's own symbol table (an executable exports almost
/// nothing, so `dladdr` would name the nearest export, wrongly), `dladdr` for everything else.
mod symbols {
    #[derive(Default)]
    pub(super) struct Cache {
        seen: std::collections::HashMap<usize, String>,
        #[cfg(target_os = "macos")]
        exe: Option<Option<super::macho::Symtab>>,
    }

    impl Cache {
        pub(super) fn name(&mut self, addr: usize) -> String {
            if let Some(n) = self.seen.get(&addr) {
                return n.clone();
            }
            let n = super::shorten(&self.lookup(addr));
            self.seen.insert(addr, n.clone());
            n
        }

        #[cfg(target_os = "macos")]
        fn lookup(&mut self, addr: usize) -> String {
            // A return address points after the call; look up the call itself.
            let pc = addr.saturating_sub(1);
            let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
            // SAFETY: dladdr fills `info` with pointers into dyld's own tables.
            if unsafe { libc::dladdr(pc as *const libc::c_void, &mut info) } == 0 {
                return format!("{addr:#x}");
            }
            let base = info.dli_fbase as usize;
            // SAFETY: image 0 is the main executable.
            let exe_base = unsafe { super::_dyld_get_image_header(0) } as usize;
            if base == exe_base {
                let table = self
                    .exe
                    .get_or_insert_with(|| super::macho::Symtab::of(exe_base));
                if let Some((name, off)) = table.as_ref().and_then(|t| t.find(pc)) {
                    return format!("{name}+{off:#x}");
                }
            }
            if !info.dli_sname.is_null() {
                // SAFETY: a C string in the image's string table.
                let raw = unsafe { std::ffi::CStr::from_ptr(info.dli_sname) }.to_string_lossy();
                let off = pc - info.dli_saddr as usize;
                return format!("{:#}+{off:#x}", rustc_demangle::demangle(&raw));
            }
            let image = if info.dli_fname.is_null() {
                "?".into()
            } else {
                // SAFETY: a C string dyld owns.
                let path = unsafe { std::ffi::CStr::from_ptr(info.dli_fname) }.to_string_lossy();
                path.rsplit('/').next().unwrap_or("?").to_string()
            };
            format!("{image}+{:#x}", pc - base)
        }

        #[cfg(not(target_os = "macos"))]
        fn lookup(&mut self, addr: usize) -> String {
            format!("{addr:#x}")
        }
    }
}

/// The defined symbols of a loaded Mach-O image, read from its mapped `__LINKEDIT`.
#[cfg(target_os = "macos")]
mod macho {
    const MH_MAGIC_64: u32 = 0xfeed_facf;
    const LC_SEGMENT_64: u32 = 0x19;
    const LC_SYMTAB: u32 = 0x2;
    const N_STAB: u8 = 0xe0;
    const N_TYPE: u8 = 0x0e;
    const N_SECT: u8 = 0x0e;

    pub(super) struct Symtab {
        /// (address, demangled name), sorted by address.
        syms: Vec<(usize, String)>,
    }

    fn rd<T: Copy>(at: usize) -> T {
        // SAFETY: callers pass addresses inside the image's mapped header and __LINKEDIT.
        unsafe { std::ptr::read_unaligned(at as *const T) }
    }

    impl Symtab {
        pub(super) fn of(base: usize) -> Option<Self> {
            if rd::<u32>(base) != MH_MAGIC_64 {
                return None;
            }
            let ncmds = rd::<u32>(base + 16);
            let mut cmd = base + 32;
            let (mut text_vm, mut link_vm, mut link_off) = (None, None, 0u64);
            let mut symtab = None;
            for _ in 0..ncmds {
                let (kind, size) = (rd::<u32>(cmd), rd::<u32>(cmd + 4));
                if kind == LC_SEGMENT_64 {
                    let name: [u8; 16] = rd(cmd + 8);
                    let vmaddr = rd::<u64>(cmd + 24);
                    if name.starts_with(b"__TEXT\0") {
                        text_vm = Some(vmaddr);
                    } else if name.starts_with(b"__LINKEDIT\0") {
                        link_vm = Some(vmaddr);
                        link_off = rd::<u64>(cmd + 40);
                    }
                } else if kind == LC_SYMTAB {
                    symtab = Some((rd::<u32>(cmd + 8), rd::<u32>(cmd + 12), rd::<u32>(cmd + 16)));
                }
                cmd += size as usize;
            }
            let (text_vm, link_vm, (symoff, nsyms, stroff)) = (text_vm?, link_vm?, symtab?);
            let slide = base as u64 - text_vm;
            let linkedit = (slide + link_vm - link_off) as usize;
            let mut syms = Vec::with_capacity(nsyms as usize);
            for i in 0..nsyms as usize {
                let nl = linkedit + symoff as usize + i * 16;
                let (strx, ntype, value) = (rd::<u32>(nl), rd::<u8>(nl + 4), rd::<u64>(nl + 8));
                if ntype & N_STAB != 0 || ntype & N_TYPE != N_SECT || value == 0 {
                    continue;
                }
                let at = linkedit + stroff as usize + strx as usize;
                // SAFETY: a NUL-terminated name in the mapped string table.
                let raw = unsafe { std::ffi::CStr::from_ptr(at as *const libc::c_char) }
                    .to_string_lossy();
                let raw = raw.strip_prefix('_').unwrap_or(&raw);
                syms.push((
                    (value + slide) as usize,
                    format!("{:#}", rustc_demangle::demangle(raw)),
                ));
            }
            syms.sort_unstable_by_key(|&(a, _)| a);
            Some(Self { syms })
        }

        /// The symbol `pc` is in, and how far into it.
        pub(super) fn find(&self, pc: usize) -> Option<(&str, usize)> {
            let i = self.syms.partition_point(|&(a, _)| a <= pc);
            let (start, name) = self.syms.get(i.checked_sub(1)?)?;
            Some((name, pc - start))
        }
    }
}

#[cfg(all(test, target_os = "macos", target_arch = "aarch64"))]
mod tests {
    use super::*;

    #[global_allocator]
    static ALLOC: Census = Census;

    /// The census is process-global; one test at a time drives it.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    // One block per element is the point: the census counts blocks.
    #[allow(clippy::vec_box)]
    #[inline(never)]
    fn leaky_site_for_the_census(n: usize) -> Vec<Box<[u8; 240]>> {
        (0..n).map(|i| Box::new([i as u8; 240])).collect()
    }

    #[test]
    fn the_site_that_keeps_its_blocks_tops_the_report() {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        set_enabled(true, 1);
        let kept = std::hint::black_box(leaky_site_for_the_census(3000));
        // Churn elsewhere that frees what it allocates must not rank.
        for _ in 0..3000 {
            drop(std::hint::black_box(vec![0u8; 240]));
        }
        let lines = report();
        set_enabled(false, DEFAULT_SAMPLE_EVERY);
        drop(kept);
        let census = &lines[0];
        assert!(census.contains("on=1"), "{lines:#?}");
        // Not necessarily first: other tests in this binary allocate while this one runs.
        let top = lines
            .iter()
            .find(|l| {
                l.starts_with("census-site ")
                    && l.contains("at=limina_debug::alloc_census::tests::leaky_site_for_the_census")
            })
            .unwrap_or_else(|| panic!("{lines:#?}"));
        let samples: u64 = top
            .split_whitespace()
            .find_map(|kv| kv.strip_prefix("samples="))
            .unwrap()
            .parse()
            .unwrap();
        assert!(samples >= 2900, "{top}");
        // Its size class is not asserted: a concurrent test frees thousands of 240-byte blocks
        // it allocated before the census was on, which is exactly what the net counts show.
        assert!(
            lines.iter().any(|l| l.starts_with("census-sizes")),
            "{lines:#?}"
        );
    }

    #[test]
    fn off_drops_every_sample_and_stops_counting() {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        set_enabled(true, 1);
        let kept = leaky_site_for_the_census(100);
        set_enabled(false, DEFAULT_SAMPLE_EVERY);
        assert_eq!(LIVE.load(Relaxed), 0);
        let before = ALLOCS.load(Relaxed);
        let more = leaky_site_for_the_census(100);
        assert_eq!(ALLOCS.load(Relaxed), before);
        assert_eq!(report().len(), 1);
        drop((kept, more));
    }

    #[test]
    fn names_lose_their_generic_arguments() {
        assert_eq!(
            shorten(
                "<alloc::vec::Vec<alloc::boxed::Box<[u8; 240]>> as alloc::vec::spec_extend::SpecExtend<alloc::boxed::Box<[u8; 240]>, I>>::spec_extend+0x1f"
            ),
            "<alloc::vec::Vec<…> as alloc::vec::spec_extend::SpecExtend<…>>::spec_extend+0x1f"
        );
        assert_eq!(
            shorten(
                "core::iter::adapters::map::map_fold::<i32, Box<[u8; 240]>, ()>::{closure#0}+0x27"
            ),
            "core::iter::adapters::map::map_fold::{closure#0}+0x27"
        );
        assert_eq!(
            shorten("limina_vmm::krun::run+0x10"),
            "limina_vmm::krun::run+0x10"
        );
        assert_eq!(
            shorten(
                "test::short::<core::result::Result<(), String>, fn() -> core::result::Result<(), String>>+0x17"
            ),
            "test::short+0x17"
        );
    }

    #[test]
    fn size_classes_cover_every_size() {
        assert_eq!(bucket(0), 0);
        assert_eq!(bucket(1), 1);
        assert_eq!(bucket(240), 15);
        assert_eq!(bucket_bytes(bucket(240)), 240);
        assert_eq!(bucket_bytes(bucket(1024)), 1024);
        assert_eq!(bucket_bytes(bucket(1025)), 2048);
        assert_eq!(bucket_bytes(bucket(1 << 20)), 1 << 20);
        assert_eq!(bucket(usize::MAX / 2), BUCKETS - 1);
    }
}
