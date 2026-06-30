// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The SAME work as the `t0_headless_device_only` test — a headless wgpu Vulkan
//! device, created and dropped — but as a plain binary instead of a libtest test.
//! On venus this exits cleanly ("dropped cleanly"); the identical code inside a
//! `#[test]` SIGSEGVs. That contrast is the whole point: the crash is in how the
//! device's teardown interacts with libtest's process/thread shutdown, not in the
//! drop itself.
//!
//!   cargo run    # venus → "dropped cleanly", exit 0
//!   cargo test   # venus → SIGSEGV
fn main() {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        force_fallback_adapter: false,
        compatible_surface: None,
    }))
    .expect("request adapter");
    eprintln!(
        "adapter: backend={:?} driver={}",
        adapter.get_info().backend,
        adapter.get_info().driver
    );
    let (device, queue) =
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
            .expect("request device");
    eprintln!("created headless device; dropping...");
    drop(queue);
    drop(device);
    drop(instance);
    eprintln!("dropped cleanly — no crash as a binary");
}
