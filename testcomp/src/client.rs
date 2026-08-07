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

pub fn run(width: i32, height: i32, hold: std::time::Duration) -> Result<()> {
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
    let shm = app.shm.clone().context("no wl_shm")?;
    let wm_base = app.wm_base.clone().context("no xdg_wm_base")?;

    let surface = compositor.create_surface(&qh, ());
    let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg_surface.get_toplevel(&qh, ());
    toplevel.set_title("limina-testcomp client".into());
    // Commit with no buffer first: the protocol requires waiting for the configure this
    // provokes before attaching anything.
    surface.commit();
    queue.roundtrip(&mut app).context("configure roundtrip")?;

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
    // Xrgb8888 to match the scanout format end to end; an Argb8888 client would exercise a
    // blend the compositor does not do, and the mismatch would show up as a wrong sample
    // value that looks like a bug in the copy.
    let buffer = pool.create_buffer(0, width, height, stride, wl_shm::Format::Xrgb8888, &qh, ());
    app.surface = Some(surface.clone());
    app.buffer = Some(buffer.clone());
    app.size = (width, height);

    app.draw(&qh);
    queue.roundtrip(&mut app).context("first frame roundtrip")?;

    println!("CLIENT COMMITTED {width}x{height}");

    // Render continuously off the compositor's frame callbacks, which is what a real client
    // does and what M3 needs: a client that must be killed *mid-lifetime*, with a buffer
    // committed and in flight, rather than one parked on a single frame.
    let deadline = std::time::Instant::now() + hold;
    while std::time::Instant::now() < deadline && !app.closed {
        queue
            .blocking_dispatch(&mut app)
            .context("dispatching client events")?;
    }
    println!("CLIENT DONE frames={}", app.frames);
    Ok(())
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
    wm_base: Option<xdg_shell::xdg_wm_base::XdgWmBase>,
    surface: Option<wl_surface::WlSurface>,
    buffer: Option<wl_buffer::WlBuffer>,
    size: (i32, i32),
    frames: u32,
    closed: bool,
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
        state.draw(qh);
    }
}

delegate_noop!(App: ignore wl_compositor::WlCompositor);
delegate_noop!(App: ignore wl_surface::WlSurface);
delegate_noop!(App: ignore wl_shm::WlShm);
delegate_noop!(App: ignore wl_shm_pool::WlShmPool);
delegate_noop!(App: ignore wl_buffer::WlBuffer);

/// `xdg_shell` is not in `wayland-client` itself; it lives in `wayland-protocols`.
mod xdg_shell {
    pub use wayland_protocols::xdg::shell::client::*;
}
