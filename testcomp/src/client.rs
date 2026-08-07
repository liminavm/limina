// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva

//! A minimal `wl_shm` client, used as the compositor's counterparty.
//!
//! It is ours rather than a stock client (`weston-simple-shm` and friends) for two reasons,
//! and the second is the important one:
//!
//!   * It draws a **known** four-quadrant pattern, so the gate can assert pixel values at
//!     sample points rather than "something appeared". The colours are deliberately unlike the
//!     compositor's backdrop and unlike `churn`'s gradient, so a capture cannot confuse a
//!     client frame with a compositor clear.
//!   * M3 has to kill a client at a precise moment mid-lifetime — after a buffer is committed
//!     and before it is released — to reach the lifetime cases in
//!     `spikes/venus-churn-retention/buffer-lifetime-matrix.md`. That needs a client whose
//!     behaviour we dictate, so writing it now is preparation, not a shortcut.
//!
//! It is still a *real* client: real socket, real `wl_shm` pool over a real memfd, real
//! `xdg_surface` configure round-trip. Nothing about the path is simulated.

use anyhow::{Context, Result};
use std::os::fd::AsFd;
use wayland_client::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{delegate_noop, Connection, Dispatch, QueueHandle};

/// Which kind of buffer the client hands the compositor.
///
/// The two are not interchangeable for what M3 measures. `Shm` pixels are copied out and the
/// buffer stops mattering; `Dmabuf` pixels stay in *this process's* venus allocation, which the
/// compositor imports — creating the cross-context host-side reference that
/// `spikes/venus-churn-retention/buffer-lifetime-matrix.md` is about. Only the dmabuf arm can
/// reach those cases; the shm arm remains because it is the simpler thing to bisect against
/// when the dmabuf arm misbehaves.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Shm,
    Dmabuf,
    /// M3c: the buffer is a **classic vrend** resource allocated through gbm, not a venus one.
    /// Same protocol path as `Dmabuf` — the difference is entirely in who owns the host-side
    /// IOSurface, which is the asymmetry `buffer-lifetime-matrix.md` §3 is about. See
    /// `crate::gbm`.
    Gbm,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Shm => "shm",
            Kind::Dmabuf => "dmabuf",
            Kind::Gbm => "gbm",
        }
    }
}

/// Refuse to run the gbm arm when it would be **vacuous**.
///
/// `MESA_LOADER_DRIVER_OVERRIDE` selects gbm's backing driver too. Under `zink` a gbm buffer is a
/// zink→venus **blob** — a venus allocation wearing a gbm API — so the whole arm would silently
/// re-run M3b's symmetric venus↔venus case while its name, its logs and its results all claimed
/// to be testing the asymmetric vrend one. That is the invariance failure `CLAUDE.md` describes,
/// pre-armed; the same guard is why `vkclassicimport.py` refuses to run under zink.
///
/// This is necessary but **not sufficient**: it only proves the client's *intent*. The
/// load-bearing confirmation is host-side — the worker must log the import resolving through a
/// vrend-owned IOSurface. See `teardown-matrix.sh`.
fn require_classic_gbm() -> Result<()> {
    let override_ = std::env::var("MESA_LOADER_DRIVER_OVERRIDE").unwrap_or_default();
    anyhow::ensure!(
        override_ == "virtio_gpu",
        "MESA_LOADER_DRIVER_OVERRIDE is {:?}, not \"virtio_gpu\" — under anything else gbm hands \
         back a venus blob and the gbm arm tests the venus path while claiming to test vrend",
        override_
    );
    Ok(())
}

/// The four quadrants, row-major (top-left, top-right, bottom-left, bottom-right), as RGB.
/// Saturated primaries so a sample point is unambiguous even if something downstream
/// dithers, and so a channel swap (the classic ARGB/ABGR confusion) is immediately visible
/// rather than merely "wrong-looking".
pub const QUADRANTS: [[u8; 3]; 4] = [
    [255, 0, 0],   // red
    [0, 255, 0],   // green
    [0, 0, 255],   // blue
    [255, 255, 0], // yellow
];

/// `park`: commit one frame and then stop drawing, holding the buffer committed and (with a
/// `--hold-buffers` compositor) unreleased, so a `SIGKILL` from outside lands in a *known*
/// state. Without it the kill would land at a random point in the render loop, which is the
/// same experiment run with an uncontrolled variable.
pub fn run(
    width: i32,
    height: i32,
    hold: std::time::Duration,
    kind: Kind,
    park: bool,
) -> Result<()> {
    let conn = Connection::connect_to_env()
        .context("connecting to the compositor (is WAYLAND_DISPLAY set?)")?;
    let display = conn.display();
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    display.get_registry(&qh, ());

    let mut app = App::default();
    // Two round-trips: the first delivers the globals, the second the bindings' own events.
    queue.roundtrip(&mut app).context("registry roundtrip")?;
    let compositor = app.compositor.clone().context("no wl_compositor")?;
    let wm_base = app.wm_base.clone().context("no xdg_wm_base")?;

    let surface = compositor.create_surface(&qh, ());
    let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg_surface.get_toplevel(&qh, ());
    toplevel.set_title("limina-testcomp client".into());
    // Commit with no buffer first: the protocol requires waiting for the configure this
    // provokes before attaching anything.
    surface.commit();
    queue.roundtrip(&mut app).context("configure roundtrip")?;

    // Kept in scope for the whole run: the venus image backing a dmabuf buffer must outlive
    // every frame the compositor composites from it.
    let _vk_backing;
    let buffer = match kind {
        Kind::Shm => {
            let shm = app.shm.clone().context("no wl_shm")?;
            let stride = width * 4;
            let size = (stride * height) as usize;
            let file = memfd(size)?;
            let map = unsafe {
                std::slice::from_raw_parts_mut(
                    libc::mmap(
                        std::ptr::null_mut(),
                        size,
                        libc::PROT_READ | libc::PROT_WRITE,
                        libc::MAP_SHARED,
                        std::os::fd::AsRawFd::as_raw_fd(&file),
                        0,
                    ) as *mut u8,
                    size,
                )
            };
            paint(map, width as u32, height as u32, stride as u32);

            let pool = shm.create_pool(file.as_fd(), size as i32, &qh, ());
            // Xrgb8888 to match the scanout format end to end; an Argb8888 client would
            // exercise a blend the compositor does not do, and the mismatch would show up as a
            // wrong sample value that looks like a bug in the copy.
            pool.create_buffer(0, width, height, stride, wl_shm::Format::Xrgb8888, &qh, ())
        }
        Kind::Dmabuf | Kind::Gbm => {
            let dmabuf = app.dmabuf.clone().context(
                "compositor has no zwp_linux_dmabuf_v1 — the dmabuf arm needs a compositor \
                 built with the M3 global",
            )?;
            let (buffer, backing) = match kind {
                Kind::Gbm => gbm_buffer(&dmabuf, &qh, width, height)?,
                _ => dmabuf_buffer(&dmabuf, &qh, width, height)?,
            };
            _vk_backing = backing;
            buffer
        }
    };

    app.surface = Some(surface.clone());
    app.buffer = Some(buffer.clone());
    app.size = (width, height);

    app.park = park;
    app.draw(&qh);
    queue.roundtrip(&mut app).context("first frame roundtrip")?;

    println!("CLIENT COMMITTED {width}x{height} kind={}", kind.name());

    // Render continuously off the compositor's frame callbacks, which is what a real client
    // does and what M3 needs: a client that must be killed *mid-lifetime*, with a buffer
    // committed and in flight, rather than one parked on a single frame.
    let deadline = std::time::Instant::now() + hold;
    if park {
        // Parked: nothing will arrive, so a blocking dispatch would sit past the deadline.
        // Idle instead, with the buffer committed — this is the state a SIGKILL is meant to
        // catch, and staying in it is the whole job.
        println!("CLIENT PARKED pid={}", std::process::id());
        while std::time::Instant::now() < deadline && !app.closed {
            queue.flush().ok();
            queue.dispatch_pending(&mut app).ok();
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    } else {
        while std::time::Instant::now() < deadline && !app.closed {
            queue
                .blocking_dispatch(&mut app)
                .context("dispatching client events")?;
        }
    }
    println!("CLIENT DONE frames={}", app.frames);
    Ok(())
}

/// Churn `count` **distinct** dmabufs through the compositor, one at a time.
///
/// The one-buffer client cannot separate a working import cache from a leaking one: a single
/// import is a single import either way. Churning distinct buffers makes the difference
/// cumulative — a compositor that evicts holds one at a time, one that does not holds `count` —
/// which is what turns the host-side census and `owned unmapped` into instruments with a scale.
///
/// Each iteration allocates, commits, **waits for the frame callback** (so the compositor has
/// presented and released it), then destroys the `wl_buffer` and frees the venus image. That
/// destroy is what should fire `buffer_destroyed` on the other side; whether it does is the
/// measurement.
pub fn run_churn(width: i32, height: i32, count: u32, kind: Kind) -> Result<()> {
    anyhow::ensure!(
        kind != Kind::Shm,
        "churn needs a dmabuf-class buffer; shm pixels are copied out and nothing is retained"
    );
    let conn = Connection::connect_to_env()
        .context("connecting to the compositor (is WAYLAND_DISPLAY set?)")?;
    let display = conn.display();
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    display.get_registry(&qh, ());

    let mut app = App::default();
    queue.roundtrip(&mut app).context("registry roundtrip")?;
    let compositor = app.compositor.clone().context("no wl_compositor")?;
    let wm_base = app.wm_base.clone().context("no xdg_wm_base")?;
    let dmabuf = app
        .dmabuf
        .clone()
        .context("compositor has no zwp_linux_dmabuf_v1")?;

    let surface = compositor.create_surface(&qh, ());
    let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg_surface.get_toplevel(&qh, ());
    toplevel.set_title(format!("limina-testcomp {} churn", kind.name()));
    surface.commit();
    queue.roundtrip(&mut app).context("configure roundtrip")?;

    app.surface = Some(surface.clone());
    app.size = (width, height);
    // Parked: the churn loop drives commits by hand, one per buffer, rather than letting the
    // callback re-`draw` the *previous* buffer.
    app.park = true;

    for i in 0..count {
        let (buffer, backing) = match kind {
            Kind::Gbm => gbm_buffer(&dmabuf, &qh, width, height),
            _ => dmabuf_buffer(&dmabuf, &qh, width, height),
        }
        .with_context(|| format!("allocating churn buffer {i}"))?;

        let before = app.frames;
        surface.attach(Some(&buffer), 0, 0);
        surface.damage(0, 0, width, height);
        surface.frame(&qh, ());
        surface.commit();

        // Wait for the frame callback: it means the compositor presented this buffer and is
        // done reading it. Releasing the backing before that would be a use-after-free that
        // happens to work — the same discipline `churn` applies to page flips.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while app.frames == before && std::time::Instant::now() < deadline {
            queue
                .blocking_dispatch(&mut app)
                .context("waiting for the frame callback")?;
        }
        anyhow::ensure!(
            app.frames != before,
            "compositor never answered the frame callback for buffer {i} — it is stalled, and \
             a churn measurement taken now would describe nothing"
        );

        // The destroy that should evict the compositor's import.
        buffer.destroy();
        drop(backing);
        queue.flush().ok();

        if i % 10 == 0 {
            log::info!("churned {i}/{count}");
        }
    }
    queue.roundtrip(&mut app).context("final roundtrip")?;

    // `created=` in kmschurn's spirit: a retention number means nothing without evidence that
    // the buffers were actually allocated and handed over.
    println!(
        "CLIENT CHURN DONE buffers={count} created={count} kind={}",
        kind.name()
    );
    Ok(())
}

/// Allocate a venus image, paint the quadrants into it, and wrap it in a `wl_buffer` over
/// `zwp_linux_dmabuf_v1`.
///
/// The allocation is `Vk::scanout_image` — the *same* dedicated-and-exported venus path the
/// compositor's own scanout buffers take. That is deliberate: the host resolves a cross-context
/// import by looking the exporter's resource up and aliasing its bytes
/// (`vkr_device_memory.c:319`), and only a resource that reached the host allocator has an
/// IOSurface for it to find. An ordinary guest-side allocation would import *successfully* via
/// the host's fallback ladder and arm no reference at all.
///
/// The returned `Vk` must outlive the buffer: it owns the device the image was allocated from.
fn dmabuf_buffer(
    dmabuf: &linux_dmabuf::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
    qh: &QueueHandle<App>,
    width: i32,
    height: i32,
) -> Result<(wl_buffer::WlBuffer, Backing)> {
    use std::os::fd::{AsRawFd, BorrowedFd};

    let vk = crate::vk::Vk::new(crate::WANT_DRIVER)
        .context("opening venus in the client (is VK_DRIVER_FILES set?)")?;
    let img = vk
        .scanout_image(width as u32, height as u32)
        .context("allocating the client's venus image")?;

    // Paint through the GPU rather than a CPU map: the image is device-local, and the point is
    // for the compositor to read pixels this process's GPU wrote — a host-side copy would be a
    // different path from the one under test.
    let stride = width as u32 * 4;
    let mut pixels = vec![0u8; stride as usize * height as usize];
    paint(&mut pixels, width as u32, height as u32, stride);
    vk.clear(&img, [0.0, 0.0, 0.0, 1.0])
        .context("transitioning the client image")?;
    vk.upload(&img, &pixels, width as u32, height as u32, stride, 0, 0)
        .context("painting the client image")?;

    let params = dmabuf.create_params(qh, ());
    params.add(
        unsafe { BorrowedFd::borrow_raw(img.dmabuf) },
        0,
        img.offset,
        img.stride,
        (img.modifier >> 32) as u32,
        (img.modifier & 0xffff_ffff) as u32,
    );
    // `create_immed` rather than `create`: it returns the buffer synchronously instead of
    // through an event, so a failure surfaces as a protocol error on this call rather than as a
    // buffer that silently never arrives.
    let buffer = params.create_immed(
        width,
        height,
        DRM_FORMAT_XRGB8888,
        linux_dmabuf::zwp_linux_buffer_params_v1::Flags::empty(),
        qh,
        (),
    );
    params.destroy();

    log::info!(
        "client dmabuf {width}x{height} stride={} modifier={:#x} fd={}",
        img.stride,
        img.modifier,
        img.dmabuf.as_raw_fd(),
    );

    Ok((buffer, Backing::Venus { vk, img }))
}

/// M3c's allocator: a **classic vrend** client buffer, handed over the same `linux-dmabuf` path.
///
/// The protocol traffic is identical to `dmabuf_buffer`'s — same `create_params`/`add`/
/// `create_immed`, same format — and that is deliberate: the *only* variable between the two arms
/// is which host renderer owns the storage. Anything else differing would confound the comparison.
///
/// Refuses to run when the loader override would make the buffer a venus blob (see
/// `require_classic_gbm`), because a vacuous pass here is worse than a failure: it looks like
/// evidence.
fn gbm_buffer(
    dmabuf: &linux_dmabuf::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
    qh: &QueueHandle<App>,
    width: i32,
    height: i32,
) -> Result<(wl_buffer::WlBuffer, Backing)> {
    use std::os::fd::BorrowedFd;

    require_classic_gbm()?;

    let gbm = crate::gbm::Gbm::open().context("opening gbm in the client")?;
    let bo = gbm
        .create(width as u32, height as u32, DRM_FORMAT_XRGB8888)
        .context("allocating the client's gbm buffer")?;
    gbm.paint(&bo, width as u32, height as u32)
        .context("painting the client's gbm buffer")?;

    let params = dmabuf.create_params(qh, ());
    params.add(
        // SAFETY: `bo.fd` is open and owned by `bo`, which outlives this call.
        unsafe { BorrowedFd::borrow_raw(bo.fd) },
        0,
        bo.offset,
        bo.stride,
        (bo.modifier >> 32) as u32,
        (bo.modifier & 0xffff_ffff) as u32,
    );
    let buffer = params.create_immed(
        width,
        height,
        DRM_FORMAT_XRGB8888,
        linux_dmabuf::zwp_linux_buffer_params_v1::Flags::empty(),
        qh,
        (),
    );
    params.destroy();

    // The modifier is worth logging on its own: vrend's IOSurface backing makes the storage
    // LINEAR, so anything else here means the buffer did NOT take the path this arm is testing.
    log::info!(
        "client gbm buffer {width}x{height} stride={} offset={} modifier={:#x}",
        bo.stride,
        bo.offset,
        bo.modifier,
    );

    Ok((buffer, Backing::Gbm { gbm, bo: Some(bo) }))
}

/// `DRM_FORMAT_XRGB8888`, the fourcc for the one format this vehicle handles end to end.
const DRM_FORMAT_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");

/// The client's venus image and the device it came from, held for the buffer's whole lifetime.
///
/// Its `Drop` is the client's half of the release discipline: the compositor's host-side
/// reference can only be judged once the exporter has provably let go, which is the same rule
/// `churn` runs under. A `SIGKILL`ed client never runs this — which is the point of M3b, not an
/// oversight here.
pub enum Backing {
    /// M3a/M3b: the storage is a venus allocation in this process.
    Venus {
        vk: crate::vk::Vk,
        img: crate::vk::ScanoutImage,
    },
    /// M3c: the storage is a classic vrend resource. Note what this `Drop` does *not* control —
    /// the host IOSurface belongs to vrend, and the compositor's borrowed `+1` outlives
    /// everything here by design.
    Gbm {
        gbm: crate::gbm::Gbm,
        bo: Option<crate::gbm::Bo>,
    },
}

impl Drop for Backing {
    fn drop(&mut self) {
        match self {
            Backing::Venus { vk, img } => {
                // SAFETY: the exported fd is ours and this is its last use.
                unsafe { libc::close(img.dmabuf) };
                vk.destroy_image(img);
            }
            Backing::Gbm { gbm, bo } => {
                if let Some(bo) = bo.take() {
                    // SAFETY: the exported fd is ours and this is its last use. Closed before
                    // the bo goes, mirroring the venus arm's order.
                    unsafe { libc::close(bo.fd) };
                    gbm.destroy(bo);
                }
            }
        }
    }
}

/// Fill the buffer with four solid quadrants in `XRGB8888` memory order (`[B, G, R, X]`).
pub fn paint(buf: &mut [u8], width: u32, height: u32, stride: u32) {
    for y in 0..height as usize {
        let bottom = y >= height as usize / 2;
        for x in 0..width as usize {
            let right = x >= width as usize / 2;
            let [r, g, b] = QUADRANTS[bottom as usize * 2 + right as usize];
            let i = y * stride as usize + x * 4;
            buf[i] = b;
            buf[i + 1] = g;
            buf[i + 2] = r;
            buf[i + 3] = 0xff;
        }
    }
}

fn memfd(size: usize) -> Result<std::fs::File> {
    use std::os::fd::FromRawFd;
    let name = c"testcomp-shm";
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    anyhow::ensure!(fd >= 0, "memfd_create: {}", std::io::Error::last_os_error());
    let file = unsafe { std::fs::File::from_raw_fd(fd) };
    file.set_len(size as u64).context("sizing the shm pool")?;
    Ok(file)
}

#[derive(Default)]
struct App {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    dmabuf: Option<linux_dmabuf::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1>,
    wm_base: Option<xdg_shell::xdg_wm_base::XdgWmBase>,
    surface: Option<wl_surface::WlSurface>,
    buffer: Option<wl_buffer::WlBuffer>,
    size: (i32, i32),
    frames: u32,
    closed: bool,
    /// Stop drawing after the first commit; see `run`'s docs.
    park: bool,
}

impl App {
    /// Attach, damage, ask for the next frame callback, commit. Requesting the callback
    /// *before* the commit is what makes this a loop: the compositor answers it once the
    /// content is on screen, and that answer drives the next `draw`.
    fn draw(&mut self, qh: &QueueHandle<Self>) {
        let (Some(surface), Some(buffer)) = (&self.surface, &self.buffer) else {
            return;
        };
        let (w, h) = self.size;
        // The same buffer every frame. The compositor copies out of it and releases it
        // synchronously, so reusing it is legal — and if that release ever stopped happening,
        // this client would stall, which makes it a live check on the compositor's side of
        // the contract rather than a silent assumption.
        surface.attach(Some(buffer), 0, 0);
        surface.damage(0, 0, w, h);
        surface.frame(qh, ());
        surface.commit();
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for App {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name, interface, ..
        } = event
        {
            match interface.as_str() {
                "wl_compositor" => {
                    state.compositor = Some(registry.bind(name, 4, qh, ()));
                }
                "wl_shm" => {
                    state.shm = Some(registry.bind(name, 1, qh, ()));
                }
                // Version 3: `create_immed` (added in 2) without the surface-feedback
                // machinery of 4, which the compositor's global does not implement.
                "zwp_linux_dmabuf_v1" => {
                    state.dmabuf = Some(registry.bind(name, 3, qh, ()));
                }
                "xdg_wm_base" => {
                    state.wm_base = Some(registry.bind(name, 1, qh, ()));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<xdg_shell::xdg_wm_base::XdgWmBase, ()> for App {
    fn event(
        _: &mut Self,
        wm_base: &xdg_shell::xdg_wm_base::XdgWmBase,
        event: xdg_shell::xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Answering the ping is not optional: a compositor is entitled to kill a client that
        // ignores it, which would look like a crash in whatever is being measured.
        if let xdg_shell::xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<xdg_shell::xdg_surface::XdgSurface, ()> for App {
    fn event(
        _: &mut Self,
        xdg_surface: &xdg_shell::xdg_surface::XdgSurface,
        event: xdg_shell::xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_shell::xdg_surface::Event::Configure { serial } = event {
            xdg_surface.ack_configure(serial);
        }
    }
}

impl Dispatch<xdg_shell::xdg_toplevel::XdgToplevel, ()> for App {
    fn event(
        state: &mut Self,
        _: &xdg_shell::xdg_toplevel::XdgToplevel,
        event: xdg_shell::xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_shell::xdg_toplevel::Event::Close = event {
            state.closed = true;
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for App {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        state.frames += 1;
        if !state.park {
            state.draw(qh);
        }
    }
}

delegate_noop!(App: ignore wl_compositor::WlCompositor);
delegate_noop!(App: ignore wl_surface::WlSurface);
delegate_noop!(App: ignore wl_shm::WlShm);
delegate_noop!(App: ignore wl_shm_pool::WlShmPool);
delegate_noop!(App: ignore wl_buffer::WlBuffer);
// The `format`/`modifier` advertisements are ignored: this client asks for exactly one pair and
// finds out whether it is supported from `create_immed`, which fails loudly, rather than from a
// negotiation whose absence would silently pick something else.
delegate_noop!(App: ignore linux_dmabuf::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1);
delegate_noop!(App: ignore linux_dmabuf::zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1);

/// `xdg_shell` is not in `wayland-client` itself; it lives in `wayland-protocols`.
mod xdg_shell {
    pub use wayland_protocols::xdg::shell::client::*;
}

/// `linux-dmabuf` is an *unstable* protocol, hence the `wp::linux_dmabuf::zv1` path and the
/// `unstable` feature on `wayland-protocols`.
mod linux_dmabuf {
    pub use wayland_protocols::wp::linux_dmabuf::zv1::client::*;
}
