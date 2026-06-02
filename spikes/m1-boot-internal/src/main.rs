// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// limina M1 boot spike — INTERNAL API variant (no C ABI).
//
// Boots Fedora-Workstation-43.raw by driving libkrun's internal Rust crates directly:
// build a `VmResources`, call `vmm::builder::build_microvm`, and run our OWN
// `EventManager` loop — exactly what the C `krun_start_enter` does internally, minus
// the C marshalling and minus libkrun's ctx_cfg orchestration (which we reimplement
// here; for M1 that's just firmware + disk + serial + machine config).
//
// This is the architecture-validating spike for the decision to vendor libkrun and
// use its internal APIs (no C). Same boot path as ../m1-boot, proven via the C ABI.
//
// Usage: boot-internal <firmware.fd> <disk.raw> <console_out> <ram_mib> <readonly 0|1> <input_fifo>

use std::fs::OpenOptions;
use std::os::unix::io::IntoRawFd;

use crossbeam_channel::unbounded;
use devices::virtio::block::{CacheType, ImageType, SyncMode};
use polly::event_manager::EventManager;
use vmm::resources::{SerialConsoleConfig, VmResources};
use vmm::vmm_config::block::BlockDeviceConfig;
use vmm::vmm_config::firmware::FirmwareConfig;
use vmm::vmm_config::machine_config::VmConfig;

fn main() {
    env_logger::Builder::from_default_env()
        .filter_level(log::LevelFilter::Error)
        .init();

    let args: Vec<String> = std::env::args().collect();
    if args.len() != 7 {
        eprintln!(
            "usage: {} <firmware.fd> <disk.raw> <console_out> <ram_mib> <readonly 0|1> <input_fifo>",
            args[0]
        );
        std::process::exit(2);
    }
    let firmware = &args[1];
    let disk = &args[2];
    let console = &args[3];
    let ram_mib: usize = args[4].parse().expect("ram_mib");
    let read_only = args[5] != "0";
    let in_fifo = &args[6];

    eprintln!(
        "[spike] firmware={firmware} disk={disk} console={console} ram={ram_mib} ro={read_only}"
    );

    // --- Console fds (same trick as the C spike) ---------------------------------
    // Output -> a file we read afterwards. Input <- a FIFO opened O_RDWR so it's
    // kqueue-pollable and never EOFs. Leak both fds (the VM owns them for its life).
    let out_fd = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(console)
        .expect("open console out")
        .into_raw_fd();
    let in_fd = OpenOptions::new()
        .read(true)
        .write(true) // O_RDWR
        .open(in_fifo)
        .expect("open input fifo")
        .into_raw_fd();

    // --- Build VmResources directly (no ctx_id, no setters-over-C) ----------------
    let mut vmr = VmResources::default();

    vmr.set_vm_config(&VmConfig {
        vcpu_count: Some(4),
        mem_size_mib: Some(ram_mib),
        ht_enabled: Some(false),
        cpu_template: None,
    })
    .expect("set_vm_config");

    vmr.set_firmware_config(FirmwareConfig {
        path: firmware.into(),
    });

    vmr.add_block_device(BlockDeviceConfig {
        block_id: "root".to_string(),
        cache_type: CacheType::Writeback,
        disk_image_path: disk.to_string(),
        disk_image_format: ImageType::Raw,
        is_disk_read_only: read_only,
        direct_io: false,
        sync_mode: SyncMode::Full,
    })
    .expect("add_block_device");

    // Replace the output-dropped implicit firmware serial with our own captured one.
    vmr.disable_implicit_console = true;
    vmr.serial_consoles.push(SerialConsoleConfig {
        input_fd: in_fd,
        output_fd: out_fd,
    });

    // --- Run it: build_microvm + our own event loop (this is the whole point) -----
    let mut event_manager = EventManager::new().expect("EventManager::new");
    let (worker_sender, _worker_receiver) = unbounded();

    eprintln!("[spike] calling vmm::builder::build_microvm ...");
    let _vmm = match vmm::builder::build_microvm(&vmr, &mut event_manager, None, worker_sender) {
        Ok(vmm) => vmm,
        Err(e) => {
            eprintln!("[spike] build_microvm failed: {e:?}");
            std::process::exit(1);
        }
    };
    eprintln!("[spike] microVM built; entering our own event loop (Ctrl-C / kill to stop)");

    // OUR loop — not krun_start_enter's. (Guest PSCI shutdown still reaches
    // Vmm::stop -> libc::exit inside the vmm crate; taming that for in-process
    // control is a separate, deliberate patch — out of scope for this boot spike.)
    loop {
        if let Err(e) = event_manager.run() {
            eprintln!("[spike] EventManager loop error: {e:?}");
            std::process::exit(1);
        }
    }
}
