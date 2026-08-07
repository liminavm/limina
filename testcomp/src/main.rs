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

mod kms;
mod vk;

use anyhow::{Context, Result};
use kms::{FbDesc, Output};
use std::os::fd::BorrowedFd;

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
        other => {
            anyhow::bail!("unknown mode {other:?} — expected `probe` or `churn <frames>`")
        }
    }
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

    for frame in 0..frames {
        let img = vk.scanout_image(w, h)?;
        // A visibly changing colour, so a human watching the console can tell a live run from
        // a wedged one without instrumentation.
        let t = frame as f32 / frames.max(1) as f32;
        vk.clear(&img, [t, 1.0 - t, 0.5, 1.0])?;

        let fb = out.import(unsafe { BorrowedFd::borrow_raw(img.dmabuf) }, desc_of(&img))?;
        out.set_crtc(&fb)?;

        out.release(fb);
        close_dmabuf(&img);
        vk.destroy_image(&img);

        if frame % 50 == 0 {
            log::info!("frame {frame}/{frames}");
        }
    }

    // The line the harness parses, in kmschurn.py's shape so the same parser reads both and the
    // two vehicles stay directly comparable. `created=` is the load-bearing field: a retention
    // number means nothing without evidence that buffers were actually allocated. Printed only
    // on the success path, so a truncated run cannot be mistaken for a complete one.
    println!("CHURN DONE churn frames={frames} created={frames}");
    Ok(())
}

fn desc_of(img: &vk::ScanoutImage) -> FbDesc {
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
