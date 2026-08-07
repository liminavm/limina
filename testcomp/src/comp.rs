// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva

//! Milestone 2: the Wayland frontend. Clients connect, attach `wl_shm` buffers, and their
//! pixels are composited into a venus-allocated scanout image and page-flipped.
//!
//! Structure follows smithay's own `smallvil` example at the pinned rev, which is the
//! reference for every handler shape here. Two deliberate departures:
//!
//!   * **No `smithay::desktop`** (`Space`/`Window`). That machinery tracks buffer lifetime for
//!     us, and buffer lifetime is the thing this vehicle exists to observe. Toplevels are kept
//!     in a plain `Vec` so every reference is one we took.
//!   * **No `smithay::backend::renderer`.** `on_commit_buffer_handler` and friends maintain a
//!     `RendererSurfaceState` we would not read; the shm contents are taken straight off the
//!     surface instead.
//!
//! Three obligations come back to us as a result of skipping that machinery, and each one, if
//! missed, reads as "the client drew one frame and hung":
//!
//!   1. **Initial configure.** An xdg toplevel may not attach a buffer until it is configured,
//!      so the first, bufferless commit must be answered with `send_configure()`.
//!   2. **Buffer release.** We copy the pixels out synchronously, so the buffer goes back to
//!      the client immediately; a double-buffered client blocks after two frames otherwise.
//!   3. **Frame callbacks.** Drained on commit and answered after the flip, or the client
//!      paces itself against a callback that never arrives.

use anyhow::{Context as _, Result};
use smithay::reexports::calloop::{generic::Generic, EventLoop, Interest, Mode, PostAction};
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::{wl_buffer, wl_seat, wl_surface::WlSurface};
use smithay::reexports::wayland_server::{Client, Display, DisplayHandle};
use smithay::utils::Serial;
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    with_states, BufferAssignment, CompositorClientState, CompositorHandler, CompositorState,
    SurfaceAttributes,
};
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
    XdgToplevelSurfaceData,
};
use smithay::wayland::shm::{with_buffer_contents, ShmHandler, ShmState};
use smithay::wayland::socket::ListeningSocketSource;
use smithay::{delegate_compositor, delegate_shm, delegate_xdg_shell};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::kms::{Fb, Output};
use crate::vk::{ScanoutImage, Vk};

/// The desktop background. A colour no client draws, so a capture that shows only this is
/// unambiguously "the compositor ran and no client pixels arrived" rather than a black screen
/// that could mean anything.
const BACKDROP: [f32; 4] = [0.10, 0.10, 0.25, 1.0];

pub struct Comp {
    pub display_handle: DisplayHandle,
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,

    /// Mapped toplevels, in stacking order (last is on top). A `Vec`, not a `Space`: see the
    /// module docs on why the bookkeeping stays ours.
    pub toplevels: Vec<ToplevelSurface>,

    /// Pixels pending upload, taken from surfaces at commit and consumed by the next render.
    /// Held as owned bytes because the `wl_buffer` is released back to the client the moment
    /// the copy out of it completes — holding the buffer instead would be the very
    /// "compositor keeps a reference the client cannot see" behaviour we are here to study,
    /// and doing it accidentally in the vehicle would poison every measurement.
    pending: Vec<Surface>,

    /// Frame callbacks owed to clients, answered after the flip that showed their content.
    frame_callbacks: Vec<smithay::reexports::wayland_server::protocol::wl_callback::WlCallback>,

    pub render: Render,
    start: Instant,
    pub running: bool,
}

/// One client surface's contents, copied out of its `wl_shm` buffer.
pub struct Surface {
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
}

/// The GPU + display half: allocates a scanout image per frame and flips it, exactly as the
/// `churn` mode does. Sharing that path is deliberate — M2 must not quietly present through
/// some other route than the one M1 measured.
pub struct Render {
    pub vk: Vk,
    pub out: Output,
    onscreen: Option<(Fb, ScanoutImage)>,
    pub frames: u64,
}

impl Render {
    pub fn new(vk: Vk, out: Output) -> Self {
        Render {
            vk,
            out,
            onscreen: None,
            frames: 0,
        }
    }

    /// Composite `surfaces` over the backdrop into a fresh scanout image and flip to it.
    pub fn present(&mut self, surfaces: &[Surface]) -> Result<()> {
        let (w, h) = self.out.size();
        let img = self.vk.scanout_image(w, h)?;
        self.vk.clear(&img, BACKDROP)?;

        for s in surfaces {
            // Clip to the output: a client is free to ask for a surface larger than the
            // screen, and vkCmdCopyBufferToImage on an out-of-bounds region is undefined
            // behaviour, not a polite error.
            let cw = s.width.min(w);
            let ch = s.height.min(h);
            self.vk
                .upload(&img, &s.pixels, cw, ch, s.stride, 0, 0)
                .context("compositing a client surface")?;
        }

        let fb = self
            .out
            .import(unsafe { std::os::fd::BorrowedFd::borrow_raw(img.dmabuf) }, crate::desc_of(&img))?;
        unsafe { libc::close(img.dmabuf) };

        match self.onscreen {
            None => self.out.set_crtc(&fb)?,
            Some(_) => self.out.flip(&fb)?,
        }
        // The flip has completed, so the outgoing buffer is off-glass and every reference to
        // it can go — same release discipline as `churn`, for the same reason.
        if let Some((old_fb, old_img)) = self.onscreen.replace((fb, img)) {
            self.out.release(old_fb);
            self.vk.destroy_image(&old_img);
        }
        self.frames += 1;
        Ok(())
    }
}

impl Drop for Render {
    fn drop(&mut self) {
        if let Some((fb, img)) = self.onscreen.take() {
            self.out.release(fb);
            self.vk.destroy_image(&img);
        }
    }
}

impl Comp {
    pub fn run(vk: Vk, out: Output, frames: Option<u64>) -> Result<()> {
        let mut event_loop: EventLoop<Comp> = EventLoop::try_new().context("calloop")?;
        let display: Display<Comp> = Display::new().context("wayland display")?;
        let dh = display.handle();

        let mut state = Comp {
            compositor_state: CompositorState::new::<Self>(&dh),
            xdg_shell_state: XdgShellState::new::<Self>(&dh),
            shm_state: ShmState::new::<Self>(&dh, vec![]),
            display_handle: dh,
            toplevels: Vec::new(),
            pending: Vec::new(),
            frame_callbacks: Vec::new(),
            render: Render::new(vk, out),
            start: Instant::now(),
            running: true,
        };

        // `new_auto` needs XDG_RUNTIME_DIR, which a `sudo` shell over non-login ssh does not
        // have — main() sets one rather than letting this fail deep inside libwayland.
        let socket = ListeningSocketSource::new_auto().context(
            "creating the wayland socket (is XDG_RUNTIME_DIR set and writable?)",
        )?;
        let name = socket.socket_name().to_os_string();
        let handle = event_loop.handle();
        handle
            .insert_source(socket, |stream, _, state: &mut Comp| {
                if let Err(e) = state
                    .display_handle
                    .insert_client(stream, Arc::new(ClientState::default()))
                {
                    log::warn!("rejecting client: {e}");
                }
            })
            .map_err(|e| anyhow::anyhow!("inserting the socket source: {e}"))?;
        handle
            .insert_source(
                Generic::new(display, Interest::READ, Mode::Level),
                |_, display, state: &mut Comp| {
                    // SAFETY: the display is owned by the event source and never dropped here.
                    unsafe { display.get_mut().dispatch_clients(state)? };
                    Ok(PostAction::Continue)
                },
            )
            .map_err(|e| anyhow::anyhow!("inserting the display source: {e}"))?;

        // Paint the backdrop before any client connects, so the run is visibly alive even if
        // nothing ever attaches — and so a capture can tell "no client" from "no compositor".
        state.render.present(&[])?;

        println!("COMPOSITOR READY socket={}", name.to_string_lossy());

        while state.running {
            event_loop
                .dispatch(Some(Duration::from_millis(100)), &mut state)
                .context("event loop")?;
            state.display_handle.flush_clients().ok();
            if let Some(limit) = frames {
                if state.render.frames >= limit {
                    break;
                }
            }
        }
        println!("COMPOSITOR DONE frames={}", state.render.frames);
        Ok(())
    }

    /// Composite whatever clients have committed, flip, then answer their frame callbacks.
    ///
    /// Order matters: a callback says "the content you committed has been shown", so sending
    /// it before the flip would be a lie the client then paces itself against.
    fn render_and_notify(&mut self) {
        let surfaces = std::mem::take(&mut self.pending);
        if let Err(e) = self.render.present(&surfaces) {
            log::error!("present: {e:#}");
            self.running = false;
            return;
        }
        let ms = self.start.elapsed().as_millis() as u32;
        for cb in self.frame_callbacks.drain(..) {
            cb.done(ms);
        }
    }
}

impl CompositorHandler for Comp {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        // The initial configure: until it is sent, a toplevel is not allowed to attach a
        // buffer, so a compositor that skips this waits forever for content that protocol
        // forbids the client to send.
        if let Some(toplevel) = self
            .toplevels
            .iter()
            .find(|t| t.wl_surface() == surface)
            .cloned()
        {
            let configured = with_states(surface, |states| {
                states
                    .data_map
                    .get::<XdgToplevelSurfaceData>()
                    .map(|d| d.lock().unwrap().initial_configure_sent)
                    .unwrap_or(false)
            });
            if !configured {
                toplevel.send_configure();
                return;
            }
        }

        let taken = with_states(surface, |states| {
            let mut attrs = states.cached_state.get::<SurfaceAttributes>();
            let attrs = attrs.current();
            // `buffer` and `frame_callbacks` aggregate across commits and must be drained by
            // whoever processes them — leaving them in place double-counts.
            let callbacks = std::mem::take(&mut attrs.frame_callbacks);
            let buffer = attrs.buffer.take();
            (buffer, callbacks)
        });
        let (buffer, callbacks) = taken;
        self.frame_callbacks.extend(callbacks);

        match buffer {
            Some(BufferAssignment::NewBuffer(buffer)) => {
                match read_shm(&buffer) {
                    Ok(surface) => self.pending.push(surface),
                    Err(e) => log::warn!("client buffer not readable as shm: {e:#}"),
                }
                // Released immediately: the pixels are ours now, in our own allocation. A
                // compositor that holds the buffer past this point is holding a reference the
                // client cannot see — the exact shape this vehicle exists to detect elsewhere.
                buffer.release();
            }
            Some(BufferAssignment::Removed) => {}
            None => return,
        }

        self.render_and_notify();
    }
}

/// Copy a `wl_shm` buffer's pixels out into memory we own.
fn read_shm(buffer: &wl_buffer::WlBuffer) -> Result<Surface> {
    let surface = with_buffer_contents(buffer, |ptr, len, data| {
        let offset = data.offset as usize;
        let stride = data.stride as u32;
        let height = data.height as u32;
        let needed = stride as usize * height as usize;
        // Trust the pool's length, not the client's arithmetic: a short pool with a large
        // declared height is a read past the mapping, which is a SIGBUS, not an error return.
        let avail = len.saturating_sub(offset).min(needed);
        let src = unsafe { std::slice::from_raw_parts(ptr.add(offset), avail) };
        Surface {
            pixels: src.to_vec(),
            width: data.width as u32,
            height,
            stride,
        }
    })
    .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    Ok(surface)
}

impl BufferHandler for Comp {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for Comp {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl XdgShellHandler for Comp {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        log::info!("toplevel mapped");
        self.toplevels.push(surface);
    }

    /// Popups are accepted so a client that creates one is not disconnected, but nothing is
    /// drawn for them — M2's subject is the buffer path, and a menu would only add surfaces
    /// whose absence from the capture we would then have to explain.
    fn new_popup(&mut self, _surface: PopupSurface, _positioner: PositionerState) {}

    fn grab(&mut self, _surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {}

    fn reposition_request(
        &mut self,
        _surface: PopupSurface,
        _positioner: PositionerState,
        _token: u32,
    ) {
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        self.toplevels.retain(|t| t != &surface);
        log::info!("toplevel destroyed, {} left", self.toplevels.len());
    }
}

delegate_compositor!(Comp);
delegate_shm!(Comp);
delegate_xdg_shell!(Comp);

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}
