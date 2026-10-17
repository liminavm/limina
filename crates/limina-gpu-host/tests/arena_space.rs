//! Measurements of the GPU address space a fresh GPU host places arenas in, which restore relies
//! on reproducing. Run with `--ignored --nocapture`.

use std::path::Path;

use limina_gpu_host::client::GpuHost;

const MIB: u64 = 1 << 20;

fn fresh() -> GpuHost {
    let h = GpuHost::spawn(Path::new(env!("CARGO_BIN_EXE_limina-gpu-host"))).expect("spawn");
    h.create_context(1).expect("context");
    h
}

/// Whether every fresh GPU host places its first arena at the same address.
#[test]
#[ignore]
fn first_arena_base_across_fresh_gpu_hosts() {
    let runs: usize = std::env::var("RUNS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);
    let mut bases = std::collections::BTreeMap::<u64, usize>::new();
    for _ in 0..runs {
        let h = fresh();
        let a = h.create_arena(1, 1, MIB).expect("arena");
        *bases.entry(a.gpu_base).or_default() += 1;
    }
    eprintln!("first 1 MiB arena base over {runs} fresh hosts: {bases:x?}");
}

/// The space an arena takes and the alignment it gets, by size, up to the largest Metal makes.
#[test]
#[ignore]
fn gap_and_alignment_by_size() {
    for mib in [1u64, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024, 2048] {
        let h = fresh();
        let a = h.create_arena(1, 1, mib * MIB).expect("arena");
        let b = h.create_arena(1, 2, mib * MIB).expect("arena");
        let c = h.create_arena(1, 3, MIB).expect("arena");
        eprintln!(
            "{mib:>5} MiB: a {:#x} (align {:#x})  b {:#x} gap {:#x}  then 1 MiB {:#x} gap {:#x}",
            a.gpu_base,
            1u64 << a.gpu_base.trailing_zeros(),
            b.gpu_base,
            b.gpu_base.wrapping_sub(a.gpu_base + a.mapping.size()),
            c.gpu_base,
            c.gpu_base.wrapping_sub(b.gpu_base + b.mapping.size()),
        );
    }
}

/// A churned layout of mixed sizes, restored into fresh GPU hosts at its addresses, repeatedly.
/// Reports every refusal with the arena it was for.
#[test]
#[ignore]
fn churned_layouts_restore() {
    let rounds: u64 = std::env::var("ROUNDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);
    let sizes = [1, 2, 4, 8, 16, 32, 64, 128, 256, 512];
    let mut failures = 0;
    for round in 0..rounds {
        // A deterministic pseudo-random sequence per round.
        let mut x = round.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let h = fresh();
        let mut live = Vec::new();
        for id in 1..60u32 {
            let mib = sizes[(next() % sizes.len() as u64) as usize];
            let a = h.create_arena(1, id, mib * MIB).expect("arena");
            live.push((id, a.mapping.size(), a.gpu_base, a));
            if next() % 3 == 0 && live.len() > 1 {
                let i = (next() % live.len() as u64) as usize;
                let (gone, ..) = live.remove(i);
                h.destroy_arena(1, gone).expect("destroy");
            }
        }
        let mut want: Vec<(u32, u64, u64)> = live.iter().map(|l| (l.0, l.1, l.2)).collect();
        drop(live);
        want.sort_by_key(|w| w.2);
        let f = fresh();
        let mut kept = Vec::new();
        for &(id, size, base) in &want {
            match f.restore_arena(1, id, size, base) {
                Ok(a) => kept.push(a),
                Err(e) => {
                    failures += 1;
                    eprintln!(
                        "round {round}: arena {id} ({} MiB at {base:#x}) refused: {e}; \
                         layout {want:x?}",
                        size / MIB
                    );
                    break;
                }
            }
        }
    }
    eprintln!("{failures} of {rounds} rounds refused");
}
