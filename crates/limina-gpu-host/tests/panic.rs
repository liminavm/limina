// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! A panic on any GPU host thread ends the process. A ring thread that unwound alone would leave
//! its context looking alive with nothing behind it, its fences waiting for good; an exit is what
//! the worker watches for and reports as every context lost. Its own test binary, since it sets
//! the environment the GPU host inherits.

use std::path::Path;
use std::time::Duration;

use limina_gpu_host::client::GpuHost;

#[test]
fn a_panic_on_any_gpu_host_thread_ends_it() {
    // SAFETY: the only test in this process, and nothing else reads the environment meanwhile.
    unsafe { std::env::set_var("LIMINA_GPU_HOST_TEST_PANIC", "1") };
    let host = GpuHost::spawn(Path::new(env!("CARGO_BIN_EXE_limina-gpu-host"))).expect("spawn");
    let (tx, rx) = std::sync::mpsc::channel();
    host.on_exit(move || tx.send(()).expect("the test waits for this"))
        .expect("watching the GPU host");
    rx.recv_timeout(Duration::from_secs(5))
        .expect("the GPU host lived on after a thread panicked");
}
