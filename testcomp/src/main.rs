// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva

//! limina-testcomp — a small, realistic compositor used as a test vehicle for limina's host
//! side. See `README.md` for what it is for and the licence boundary it sits behind.
//!
//! Milestone 1 (this file): KMS + a Vulkan-allocated scanout, no Wayland and no input. It
//! re-implements the `-vk` arm of `crates/limina-test/guest/kmschurn.py`, and the gate on it
//! is exactly that: **it must reproduce kmschurn's numbers.** Until the Rust vehicle and the
//! Python one agree on the same host, nothing measured here means anything, because a
//! difference could equally be a bug in the transcription.
//!
//! Milestone 2 adds the Wayland frontend; milestone 3 adds client dmabuf import, which is what
//! finally reaches the cases `spikes/venus-churn-retention/buffer-lifetime-matrix.md` needs
//! and `kmschurn.py` structurally cannot: a real client whose buffers outlive it.
//!
//! Run it on the guest console, as root, with the venus ICD selected:
//!
//! ```text
//! systemctl isolate multi-user.target
//! VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json \
//!     ./limina-testcomp churn 300
//! ```

mod client;
mod comp;
mod gbm;
mod kms;
mod vk;

use anyhow::{Context, Result};
use kms::{FbDesc, Output};
use std::os::fd::BorrowedFd;
use std::os::unix::fs::PermissionsExt;

/// The DRM device. Hard-coded rather than probed: a limina guest has exactly one, and a probe
/// would only add a way to silently drive the wrong one.
const CARD: &str = "/dev/dri/card0";

/// The Vulkan driver this vehicle is built to measure.
const WANT_DRIVER: &str = "Venus";

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_else(|| "probe".into());
    match mode.as_str() {
        "probe" => probe(),
        "churn" => {
            let frames: u32 = args
                .next()
                .unwrap_or_else(|| "300".into())
                .parse()
                .context("frame count")?;
            churn(frames)
        }
        "run" => {
            // Frames are optional: bounded for a scripted gate, unbounded when a human is
            // looking at the screen.
            let mut frames = None;
            let mut hold_buffers = false;
            let mut leak_imports = false;
            for arg in args {
                match arg.as_str() {
                    "--hold-buffers" => hold_buffers = true,
                    "--leak-imports" => leak_imports = true,
                    other => frames = other.parse::<u64>().ok(),
                }
            }
            run(frames, hold_buffers, leak_imports)
        }
        "client-churn" | "client-churn-gbm" => {
            let count: u32 = args
                .next()
                .unwrap_or_else(|| "40".into())
                .parse()
                .context("buffer count")?;
            let (mut w, mut h) = (1920, 1080);
            if let Some((a, b)) = args.next().and_then(|s| {
                s.split_once('x').map(|(a, b)| (a.to_string(), b.to_string()))
            }) {
                w = a.parse().context("width")?;
                h = b.parse().context("height")?;
            }
            let kind = if mode.ends_with("-gbm") {
                client::Kind::Gbm
            } else {
                client::Kind::Dmabuf
            };
            client::run_churn(w, h, count, kind)
        }
        "client" | "client-dmabuf" | "client-gbm" => {
            let secs: u64 = args
                .next()
                .unwrap_or_else(|| "20".into())
                .parse()
                .context("hold seconds")?;
            // Size is a knob because the *host* oracle needs it to be. A 400x300 buffer is
            // ~0.5 MiB, which sits under the noise floor of `vmmap`'s `owned unmapped` bytes;
            // the teardown tests want a buffer big enough that its retention is unambiguous.
            let (mut w, mut h) = (400, 300);
            let mut park = false;
            for arg in args {
                match arg.as_str() {
                    "--park" => park = true,
                    other => {
                        if let Some((a, b)) = other.split_once('x') {
                            w = a.parse().context("width")?;
                            h = b.parse().context("height")?;
                        }
                    }
                }
            }
            let kind = match mode.as_str() {
                "client-dmabuf" => client::Kind::Dmabuf,
                "client-gbm" => client::Kind::Gbm,
                _ => client::Kind::Shm,
            };
            client::run(w, h, std::time::Duration::from_secs(secs), kind, park)
        }
        other => anyhow::bail!(
            "unknown mode {other:?} — expected `probe`, `churn <frames>`, \
             `run [frames] [--hold-buffers] [--leak-imports]`, \
             `client|client-dmabuf|client-gbm [seconds] [WxH] [--park]`, or \
             `client-churn|client-churn-gbm [count] [WxH]`"
        ),
    }
}

/// The compositor. Accepts clients on a Wayland socket, composites their `wl_shm` buffers
/// over a backdrop, and page-flips the result.
fn run(frames: Option<u64>, hold_buffers: bool, leak_imports: bool) -> Result<()> {
    // `ListeningSocketSource::new_auto` needs somewhere to put the socket, and a `sudo` shell
    // over a non-login ssh has no XDG_RUNTIME_DIR at all. Defaulting it here beats failing
    // deep inside libwayland with a message about a directory nobody set.
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
        let dir = "/tmp/testcomp-run";
        std::fs::create_dir_all(dir).with_context(|| format!("creating {dir}"))?;
        std::fs::set_permissions(dir, <std::fs::Permissions as PermissionsExt>::from_mode(0o700))
            .with_context(|| format!("chmod 700 {dir}"))?;
        log::info!("XDG_RUNTIME_DIR was unset; using {dir}");
        // SAFETY: single-threaded, before any thread that could read the environment starts.
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", dir) };
    }

    let out = Output::open(CARD)?;
    let vk = vk::Vk::new(WANT_DRIVER)?;
    comp::Comp::run(vk, out, frames, hold_buffers, leak_imports)
}

/// Report the output and one buffer's real layout, then exit. Run this first on any new image:
/// it is how you learn what mode you will get and whether the modifier path is live, before a
/// churn run makes those questions expensive to ask.
fn probe() -> Result<()> {
    let out = Output::open(CARD)?;
    let (w, h) = out.size();
    let vk = vk::Vk::new(WANT_DRIVER)?;

    let img = vk.scanout_image(w, h)?;
    let fb = out.import(unsafe { BorrowedFd::borrow_raw(img.dmabuf) }, desc_of(&img))?;
    println!(
        "PROBE {w}x{h} mod={:#x} stride={} offset={}",
        img.modifier, img.stride, img.offset
    );

    out.release(fb);
    close_dmabuf(&img);
    vk.destroy_image(&img);
    Ok(())
}

/// THE REPRODUCER. A fresh scanout buffer per frame — allocate, clear, import, scan out,
/// release everything — so the host allocator sees `frames` distinct buffers rather than one
/// recycled forever.
///
/// Every reference is dropped each iteration, which is the whole point: the measurement only
/// says something about the *host* if the guest has provably let go.
fn churn(frames: u32) -> Result<()> {
    let out = Output::open(CARD)?;
    let (w, h) = out.size();
    let vk = vk::Vk::new(WANT_DRIVER)?;
    log::info!("churning {frames} buffers at {w}x{h}");

    // The buffer currently scanned out. It cannot be released until a later flip has completed
    // — a page flip is asynchronous, so the outgoing buffer is still being read by the display
    // right up to the flip event. Holding exactly one is also what keeps the workload honest:
    // two buffers alive at a time, like a real double-buffered compositor.
    let mut onscreen: Option<(kms::Fb, vk::ScanoutImage)> = None;

    for frame in 0..frames {
        let img = vk.scanout_image(w, h)?;
        // A visibly changing colour, so a human watching the console can tell a live run from
        // a wedged one without instrumentation.
        let t = frame as f32 / frames.max(1) as f32;
        vk.clear(&img, [t, 1.0 - t, 0.5, 1.0])?;

        let fb = out.import(unsafe { BorrowedFd::borrow_raw(img.dmabuf) }, desc_of(&img))?;
        // KMS took its own reference at import, so the exported fd has done its job. Closing
        // it here rather than at teardown keeps at most one open at a time; deferring it would
        // exhaust the fd table long before the buffer count got interesting.
        close_dmabuf(&img);

        match onscreen {
            // The first buffer needs a modeset; every one after that is a flip.
            None => out.set_crtc(&fb)?,
            Some(_) => out.flip(&fb)?,
        }

        // The flip has completed, so the outgoing buffer is off-glass and every reference to
        // it can go. This is the release the whole measurement rests on: the host number only
        // says something if the guest has provably let go.
        if let Some((old_fb, old_img)) = onscreen.replace((fb, img)) {
            out.release(old_fb);
            vk.destroy_image(&old_img);
        }

        if frame % 50 == 0 {
            log::info!("frame {frame}/{frames}");
        }
    }

    if let Some((fb, img)) = onscreen.take() {
        out.release(fb);
        vk.destroy_image(&img);
    }

    // The line the harness parses, in kmschurn.py's shape so the same parser reads both and the
    // two vehicles stay directly comparable. `created=` is the load-bearing field: a retention
    // number means nothing without evidence that buffers were actually allocated. Printed only
    // on the success path, so a truncated run cannot be mistaken for a complete one.
    println!("CHURN DONE churn frames={frames} created={frames}");
    Ok(())
}

pub fn desc_of(img: &vk::ScanoutImage) -> FbDesc {
    FbDesc {
        width: img.width,
        height: img.height,
        stride: img.stride,
        offset: img.offset,
        modifier: img.modifier,
    }
}

/// Close the exported dmabuf fd. KMS took its own reference at import, so this is safe the
/// moment the import returns — and skipping it would exhaust the fd table long before the
/// buffer count got interesting.
fn close_dmabuf(img: &vk::ScanoutImage) {
    unsafe { libc::close(img.dmabuf) };
}
