//! A compute kernel from a host-built `.metallib`, run in a Linux guest over limina's Metal-level
//! GPU transport: the first step of Metal compute in the guest (docs/roadmap.md, M18).
//!
//! It loads the library's bytes into an arena and hands them to the host
//! (`CREATE_LIBRARY_DATA`), builds a pipeline for `axpy`, and runs it over a million floats with
//! its two buffers and its scalar bound by GPU address, the way MLX binds buffers and
//! `setBytes`. Every element is checked against the CPU's answer. The same kernel compiled
//! from MSL source runs as the control. Exit status 0 means both came back right.
//!
//! Usage: metal-compute-probe <axpy.metallib> <axpy.metal>

use std::time::Instant;

use limina_gpu_client::{Gpu, MIB, find_node};
use limina_gpu_wire::cmd::{self, BufRef};

const N: u32 = 1 << 20;
const A: f32 = 2.5;

// Offsets in the main arena (64 MiB; the ring takes the first 256 KiB).
const REPLY: u64 = MIB;
const INFO: u64 = MIB + 4096;
const LIB: u64 = 2 * MIB;
const LIB_MAX: u64 = 14 * MIB;
const X: u64 = 16 * MIB;
const Y: u64 = 24 * MIB;
const SCALAR: u64 = 32 * MIB;

fn x(i: u32) -> f32 {
    (i % 1000) as f32 * 0.25
}

fn y0(i: u32) -> f32 {
    1.0 - (i % 77) as f32
}

fn device_info(g: &mut Gpu) -> Result<cmd::DeviceInfo, String> {
    let reply = g.reply_slot(REPLY);
    g.push(
        cmd::QueryDevice {
            dst: BufRef::new(1, INFO),
            reply,
        },
        &[],
    );
    let r = g.reply(REPLY)?;
    if r.status != 0 {
        return Err(format!("QueryDevice: status {}", r.status));
    }
    let b = g.main.read(INFO, size_of::<cmd::DeviceInfo>());
    // SAFETY: the host wrote a DeviceInfo there before the reply; any bytes are a valid one.
    Ok(unsafe { b.as_ptr().cast::<cmd::DeviceInfo>().read_unaligned() })
}

/// Library `handle` from the metallib's bytes, staged in the main arena.
fn library_data(g: &mut Gpu, handle: u32, bytes: &[u8]) -> Result<(), String> {
    if bytes.len() as u64 > LIB_MAX {
        return Err(format!("metallib of {} bytes does not fit", bytes.len()));
    }
    g.main.write(LIB, bytes);
    let reply = g.reply_slot(REPLY);
    g.push(
        cmd::CreateLibraryData {
            handle,
            data_len: bytes.len() as u64,
            data: BufRef::new(1, LIB),
            reply,
            ..Default::default()
        },
        &[],
    );
    match g.reply(REPLY)?.status {
        0 => Ok(()),
        s => Err(format!("CreateLibraryData: status {s}")),
    }
}

/// Library `handle` compiled by the host from MSL source.
fn library_source(g: &mut Gpu, handle: u32, src: &[u8]) -> Result<(), String> {
    let reply = g.reply_slot(REPLY);
    g.push(
        cmd::CreateLibrary {
            handle,
            source_len: src.len() as u32,
            reply,
            ..Default::default()
        },
        &[src],
    );
    match g.reply(REPLY)?.status {
        0 => Ok(()),
        s => Err(format!("CreateLibrary: status {s}")),
    }
}

/// Runs `axpy` from library `library` over fresh inputs and checks every element.
fn axpy(g: &mut Gpu, library: u32, pipeline: u32, table: u32) -> Result<(), String> {
    let reply = g.reply_slot(REPLY);
    g.push(
        cmd::CreateComputePipeline {
            handle: pipeline,
            library,
            name_len: 4,
            reply,
            ..Default::default()
        },
        &[b"axpy"],
    );
    let r = g.reply(REPLY)?;
    if r.status != 0 {
        return Err(format!("CreateComputePipeline: status {}", r.status));
    }
    let xs: Vec<u8> = (0..N).flat_map(|i| x(i).to_le_bytes()).collect();
    let ys: Vec<u8> = (0..N).flat_map(|i| y0(i).to_le_bytes()).collect();
    g.main.write(X, &xs);
    g.main.write(Y, &ys);
    g.main.write(SCALAR, &A.to_le_bytes());

    let base = g.main.gpu_base;
    g.push(
        cmd::CreateArgumentTable {
            handle: table,
            max_buffer_bind_count: 3,
        },
        &[],
    );
    let bindings = [(0, base + X), (1, base + Y), (2, base + SCALAR)].map(|(index, address)| {
        cmd::AddressBinding {
            index,
            _pad: 0,
            address,
        }
    });
    g.push(
        cmd::SetAddresses { table, count: 3 },
        &[cmd::bytes_of_slice(&bindings)],
    );
    g.push(cmd::BeginCommandBuffer {}, &[]);
    g.push(cmd::BeginCompute {}, &[]);
    g.push(
        cmd::SetComputePipeline {
            handle: pipeline,
            _pad: 0,
        },
        &[],
    );
    g.push(cmd::SetArgumentTable { table, stages: 0 }, &[]);
    g.push(
        cmd::DispatchThreads {
            grid: [N, 1, 1],
            threads_per_group: [256, 1, 1],
        },
        &[],
    );
    g.push(cmd::EndEncoding {}, &[]);
    g.push(cmd::EndCommandBuffer {}, &[]);
    let start = Instant::now();
    g.commit_and_wait()?;
    let took = start.elapsed();

    let out = g.main.read(Y, N as usize * 4);
    let mut wrong = 0;
    for i in 0..N {
        let o = i as usize * 4;
        let got = f32::from_le_bytes(out[o..o + 4].try_into().unwrap());
        let want = A.mul_add(x(i), y0(i));
        if got.to_bits() != want.to_bits() && (got - want).abs() > 1e-4 * want.abs().max(1.0) {
            if wrong < 5 {
                eprintln!("element {i}: got {got}, want {want}");
            }
            wrong += 1;
        }
    }
    println!(
        "axpy from library {library}: {} of {N} elements right, commit to completion {took:?}",
        N - wrong
    );
    if wrong != 0 {
        return Err(format!("{wrong} elements wrong"));
    }
    Ok(())
}

fn run(metallib: &str, msl: &str) -> Result<(), String> {
    let lib = std::fs::read(metallib).map_err(|e| format!("{metallib}: {e}"))?;
    let src = std::fs::read(msl).map_err(|e| format!("{msl}: {e}"))?;
    let (path, mut g) = find_node().ok_or("no render node with the limina extension")?;
    let info = device_info(&mut g)?;
    let name = String::from_utf8_lossy(&info.name);
    println!(
        "{path}: {} (wire version {}, features {:#x})",
        name.trim_end_matches('\0'),
        info.version,
        info.features
    );
    if info.features & cmd::feature::LIBRARY_DATA == 0 {
        return Err("the GPU host does not take libraries from metallib data".into());
    }
    library_data(&mut g, 1, &lib)?;
    println!("metallib of {} bytes loaded", lib.len());
    axpy(&mut g, 1, 2, 3)?;
    library_source(&mut g, 4, &src)?;
    axpy(&mut g, 4, 5, 6)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: {} <axpy.metallib> <axpy.metal>", args[0]);
        std::process::exit(2);
    }
    if let Err(e) = run(&args[1], &args[2]) {
        eprintln!("FAIL: {e}");
        std::process::exit(1);
    }
    println!("OK");
}
