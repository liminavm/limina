// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva

//! The Wayland frontend. Clients connect, attach `wl_shm` or `linux-dmabuf` buffers, and their
//! pixels are composited into a venus-allocated scanout image and page-flipped.
//!
//! Structure follows smithay's own `smallvil` example at the pinned rev, which is the
//! reference for every handler shape here. Two deliberate departures:
//!
//!   * **No `smithay::desktop`** (`Space`/`Window`). That machinery tracks buffer lifetime for
//!     us, and buffer lifetime is the thing this vehicle exists to observe. Toplevels are kept
//!     in a plain `Vec` so every reference is one we took.
//!   * **No `smithay::backend::renderer`.** `on_commit_buffer_handler` and friends maintain a
//!     `RendererSurfaceState` we would not read; the buffer contents are taken straight off the
//!     surface instead.
//!
//! Three obligations come back to us as a result of skipping that machinery, and each one, if
//! missed, reads as "the client drew one frame and hung":
//!
//!   1. **Initial configure.** An xdg toplevel may not attach a buffer until it is configured,
//!      so the first, bufferless commit must be answered with `send_configure()`.
//!   2. **Buffer release.** Released once our read of it has provably completed — immediately
//!      for shm (the pixels are copied into memory we own), after the GPU copy's fence for
//!      dmabuf. A double-buffered client blocks after two frames otherwise.
//!   3. **Frame callbacks.** Drained on commit and answered after the flip, or the client
//!      paces itself against a callback that never arrives.
//!
//! ## M3: the dmabuf import cache is the thing under test
//!
//! A `wl_shm` client's pixels stop being the client's the moment they are copied out. A dmabuf
//! client's do not: the compositor holds a `VkImage` aliasing the client's buffer, and on a
//! limina guest that import reaches across virtio-gpu contexts into the host, where vkr parks a
//! **borrowed `+1`** on the exporter's IOSurface (`vkr_device_memory.c:794`).
//!
//! Caching that import across frames is what a real compositor does, and it is also what makes
//! the holder exist at teardown — without a live import there is no cross-context reference for
//! `buffer-lifetime-matrix.md`'s cases to be about. The obligation that comes with it is
//! **eviction**: `buffer_destroyed` must destroy the import, or the vehicle leaks on its own
//! account and every host-side number it produces is measuring testcomp rather than limina.

use anyhow::{Context as _, Result};
use smithay::backend::allocator::{Buffer as _, Format, Fourcc, Modifier};
use smithay::reexports::calloop::{generic::Generic, EventLoop, Interest, Mode, PostAction};
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason, ObjectId};
use smithay::reexports::wayland_server::protocol::{wl_buffer, wl_seat, wl_surface::WlSurface};
use smithay::reexports::wayland_server::{Client, Display, DisplayHandle, Resource as _};
use smithay::utils::Serial;
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    with_states, BufferAssignment, CompositorClientState, CompositorHandler, CompositorState,
    SurfaceAttributes,
};
use smithay::wayland::dmabuf::{
    get_dmabuf, DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier,
};
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
    XdgToplevelSurfaceData,
};
use smithay::wayland::shm::{with_buffer_contents, ShmHandler, ShmState};
use smithay::wayland::socket::ListeningSocketSource;
use smithay::{delegate_compositor, delegate_dmabuf, delegate_shm, delegate_xdg_shell};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::kms::{Fb, Output};
use crate::vk::{Imported, ImportedImage, ScanoutImage, Vk};

/// The desktop background. A colour no client draws, so a capture that shows only this is
/// unambiguously "the compositor ran and no client pixels arrived" rather than a black screen
/// that could mean anything.
const BACKDROP: [f32; 4] = [0.10, 0.10, 0.25, 1.0];

/// The harness touches this (in the GUEST) when it wants `--hog-mib` to fire. See `maybe_hog`.
const HOG_TRIGGER: &str = "/tmp/limina-testcomp-hog";

pub struct Comp {
    pub display_handle: DisplayHandle,
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,
    pub dmabuf_state: DmabufState,
    /// Held for the run, never read: dropping the handle is what would retire the global, so
    /// this field *is* the advertisement's lifetime.
    #[allow(dead_code)]
    pub dmabuf_global: DmabufGlobal,

    /// Mapped toplevels, in stacking order (last is on top). A `Vec`, not a `Space`: see the
    /// module docs on why the bookkeeping stays ours.
    pub toplevels: Vec<ToplevelSurface>,

    /// Content pending composition, taken from surfaces at commit and consumed by the next
    /// render.
    pending: Vec<Content>,

    /// Frame callbacks owed to clients, answered after the flip that showed their content.
    frame_callbacks: Vec<smithay::reexports::wayland_server::protocol::wl_callback::WlCallback>,

    /// Never release a client's dmabuf back to it. The teardown tests in
    /// `buffer-lifetime-matrix.md` have to `SIGKILL` a client with a buffer *committed and
    /// unreleased*, and at full speed that window is a few hundred microseconds wide — far too
    /// narrow to hit from a shell script. Holding the buffer widens it to "as long as you like".
    ///
    /// It is a deliberate protocol violation in the sense that a real compositor would not do
    /// it, but it is not a *lie*: the client genuinely cannot reuse a buffer we are still
    /// reading, which is exactly the state being reproduced.
    pub hold_buffers: bool,

    pub render: Render,
    start: Instant,
    pub running: bool,

    /// PATH 4 of the teardown matrix. Once at least one client dmabuf is imported — so the
    /// context under test is holding a borrowed host `+1` — allocate this many MiB in one go,
    /// which limina's host GPU-memory budget refuses and answers by setting **this** context
    /// FATAL (`vkr_budget_kills_context`). A deterministic ring-FATAL, with no need to provoke a
    /// real ring fault.
    ///
    /// The hog goes in the COMPOSITOR and not a client on purpose: the budget kills the context
    /// that over-allocates, and the borrowed references live on the importer's side. Hogging from
    /// a client would kill a context holding nothing and measure a teardown nobody asked about.
    hog_mib: Option<u64>,
    hogged: bool,
}

/// What a client committed, ready to be composited.
///
/// The two arms differ in exactly the way M3 is about. `Shm` owns its bytes, so the client's
/// buffer is already free by the time this exists. `Dmabuf` owns nothing but a handle: the
/// pixels are still in the client's allocation, the GPU reads them directly, and the buffer
/// cannot go back until that read has retired.
enum Content {
    Shm(Surface),
    Dmabuf(wl_buffer::WlBuffer),
}

/// A cached import's handles, copied out so the cache is not borrowed across the copy.
struct ImportHandles {
    image: ash::vk::Image,
    width: u32,
    height: u32,
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

    /// Imported client dmabufs, keyed by the `wl_buffer` that named them.
    ///
    /// **This cache is the host-side holder under test** — see the module docs. It is also the
    /// vehicle's own obligation: entries are evicted in `buffer_destroyed`, and anything left
    /// here at exit is testcomp leaking rather than limina.
    imports: HashMap<ObjectId, ImportedImage>,

    /// Imports created and evicted over the run. Printed at exit: a retention measurement means
    /// nothing without evidence that the guest both took and released its references, which is
    /// the same rule `churn`'s `created=` field exists for.
    pub imported: u64,
    pub evicted: u64,

    /// Set once the mode has been programmed, so `import_failed` can be reported without
    /// pretending a frame was presented.
    pub import_failures: u64,

    /// **The RED injection.** Never evict, and never sweep at exit — hold every import for the
    /// process's lifetime.
    ///
    /// This exists to satisfy the rule at the bottom of `README.md`: a vehicle that has never
    /// reproduced a failure in a class cannot certify the absence of one, because a green result
    /// and a blind oracle are the same observation. Churning distinct client dmabufs against
    /// this flag retains one host-side `+1` per buffer, so the census and `owned unmapped` are
    /// shown to *move* before their stillness is allowed to mean anything.
    pub leak_imports: bool,
}

impl Render {
    pub fn new(vk: Vk, out: Output, leak_imports: bool) -> Self {
        Render {
            vk,
            out,
            onscreen: None,
            frames: 0,
            imports: HashMap::new(),
            imported: 0,
            evicted: 0,
            import_failures: 0,
            leak_imports,
        }
    }

    /// How many client imports are cached right now — the count of borrowed host `+1`s this
    /// context is holding, which is what makes a teardown measurement mean anything.
    pub fn imports_live(&self) -> usize {
        self.imports.len()
    }

    /// Import a client's dmabuf, or return the cached import if this buffer is already known.
    ///
    /// Lazy rather than eager in `dmabuf_imported`: the `wl_buffer` that keys this cache does
    /// not exist yet at that point (`ImportNotifier::successful` is what creates it), so
    /// importing there would mean keying on something else and reconciling later.
    /// Returns the handles by value rather than a borrow: the caller needs `&mut self` again
    /// immediately (to record the copy), and a borrow out of the cache would keep this one
    /// alive across it.
    fn import(&mut self, buffer: &wl_buffer::WlBuffer) -> Result<ImportHandles> {
        let id = buffer.id();
        if !self.imports.contains_key(&id) {
            let dmabuf = get_dmabuf(buffer)
                .map_err(|e| anyhow::anyhow!("wl_buffer is not a dmabuf: {e:?}"))?;
            let size = dmabuf.size();
            let desc = Imported {
                width: size.w as u32,
                height: size.h as u32,
                // Plane 0 only. The vehicle advertises exactly one format/modifier pair, and a
                // multi-planar buffer could not have been created against it — so this is a
                // scope, not an unchecked assumption.
                stride: dmabuf.strides().next().context("dmabuf has no plane 0")?,
                offset: dmabuf.offsets().next().context("dmabuf has no plane 0")?,
                modifier: dmabuf.format().modifier.into(),
            };
            let handle = dmabuf.handles().next().context("dmabuf has no plane 0 fd")?;
            let img = self
                .vk
                .import_dmabuf(handle, &desc)
                .context("importing a client dmabuf")?;
            log::info!(
                "imported client dmabuf {}x{} stride={} modifier={:#x} ({} live)",
                desc.width,
                desc.height,
                desc.stride,
                desc.modifier,
                self.imports.len() + 1,
            );
            self.imports.insert(id.clone(), img);
            self.imported += 1;
        }
        let img = &self.imports[&id];
        Ok(ImportHandles {
            image: img.image,
            width: img.width,
            height: img.height,
        })
    }

    /// Drop a client's import. Called from `buffer_destroyed`, which fires both when a client
    /// destroys a buffer politely and when it dies holding one — so this is the release whose
    /// *absence* the teardown tests are looking for on the host side.
    fn evict(&mut self, buffer: &wl_buffer::WlBuffer) {
        if self.leak_imports {
            return;
        }
        if let Some(img) = self.imports.remove(&buffer.id()) {
            self.vk.destroy_imported(&img);
            self.evicted += 1;
            log::info!("evicted a client dmabuf import ({} live)", self.imports.len());
        }
    }

    /// Composite `contents` over the backdrop into a fresh scanout image and flip to it.
    fn present(&mut self, contents: &[Content]) -> Result<()> {
        let (w, h) = self.out.size();
        let img = self.vk.scanout_image(w, h)?;
        self.vk.clear(&img, BACKDROP)?;

        for c in contents {
            match c {
                Content::Shm(s) => {
                    // Clip to the output: a client is free to ask for a surface larger than the
                    // screen, and vkCmdCopyBufferToImage on an out-of-bounds region is undefined
                    // behaviour, not a polite error.
                    let cw = s.width.min(w);
                    let ch = s.height.min(h);
                    self.vk
                        .upload(&img, &s.pixels, cw, ch, s.stride, 0, 0)
                        .context("compositing a client shm surface")?;
                }
                Content::Dmabuf(buffer) => {
                    // Import errors are per-client, not fatal: a client that asks for something
                    // unimportable should lose its own frame, not take the compositor down and
                    // with it whatever measurement is in flight.
                    match self.import(buffer) {
                        Ok(src) => {
                            self.vk
                                .copy_image(&img, src.image, src.width, src.height, 0, 0)
                                .context("compositing a client dmabuf surface")?;
                        }
                        Err(e) => {
                            self.import_failures += 1;
                            log::warn!("dmabuf import failed: {e:#}");
                        }
                    }
                }
            }
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
        // Anything still cached is an import whose `wl_buffer` outlived the run — a client that
        // never destroyed it, or one killed while we held it. Releasing here keeps a clean exit
        // *clean*, so a host-side residual after `COMPOSITOR DONE` is limina's and not ours.
        // Loud rather than silent: on the clean-exit path this count should be zero, and a
        // nonzero one changes what the following measurement means.
        if self.leak_imports {
            // Deliberately NOT swept: the RED arm has to still be holding them when the
            // process dies, or the leak it is meant to demonstrate is cleaned up on the way
            // out and the measurement reads green.
            log::warn!(
                "--leak-imports: leaving {} import(s) held at exit ON PURPOSE",
                self.imports.len()
            );
            return;
        }
        if !self.imports.is_empty() {
            log::warn!(
                "{} client dmabuf import(s) still cached at exit — released now",
                self.imports.len()
            );
        }
        for (_, img) in self.imports.drain() {
            self.vk.destroy_imported(&img);
        }
    }
}

impl Comp {
    pub fn run(
        vk: Vk,
        out: Output,
        frames: Option<u64>,
        hold_buffers: bool,
        leak_imports: bool,
        hog_mib: Option<u64>,
    ) -> Result<()> {
        let mut event_loop: EventLoop<Comp> = EventLoop::try_new().context("calloop")?;
        let display: Display<Comp> = Display::new().context("wayland display")?;
        let dh = display.handle();

        let mut dmabuf_state = DmabufState::new();
        // Exactly one format/modifier pair — the one the scanout path can actually copy from.
        // Advertising more would let a client hand us something we would then fail to import,
        // turning a vehicle limitation into what looks like an import bug.
        let dmabuf_global = dmabuf_state.create_global::<Self>(
            &dh,
            vec![Format {
                code: Fourcc::Xrgb8888,
                modifier: Modifier::Linear,
            }],
        );

        let mut state = Comp {
            compositor_state: CompositorState::new::<Self>(&dh),
            xdg_shell_state: XdgShellState::new::<Self>(&dh),
            shm_state: ShmState::new::<Self>(&dh, vec![]),
            dmabuf_state,
            dmabuf_global,
            display_handle: dh,
            toplevels: Vec::new(),
            pending: Vec::new(),
            frame_callbacks: Vec::new(),
            hold_buffers,
            render: Render::new(vk, out, leak_imports),
            start: Instant::now(),
            running: true,
            hog_mib,
            hogged: false,
        };
        if hold_buffers {
            log::warn!("--hold-buffers: client buffers will NOT be released (M3b teardown mode)");
        }
        if leak_imports {
            log::warn!("--leak-imports: RED arm — imports are retained forever, on purpose");
        }
        if let Some(mib) = hog_mib {
            log::warn!(
                "--hog-mib {mib}: will trip the host GPU budget when an import is live AND \
                 {HOG_TRIGGER} exists"
            );
        }

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
            state.maybe_hog();
            if let Some(limit) = frames {
                if state.render.frames >= limit {
                    break;
                }
            }
        }
        // `imported=`/`evicted=` are the load-bearing fields, in the same spirit as `churn`'s
        // `created=`: a host-side retention number means nothing without evidence that the
        // guest took references and gave them back. Equal counts mean the vehicle is clean and
        // any residual is limina's.
        println!(
            "COMPOSITOR DONE frames={} imported={} evicted={} import_failures={}",
            state.render.frames,
            state.render.imported,
            state.render.evicted,
            state.render.import_failures,
        );
        Ok(())
    }

    /// Trip the host GPU-memory budget, once, as soon as this context is holding a live import.
    ///
    /// Nothing here can tell whether it worked: venus submits `vkAllocateMemory` asynchronously
    /// and throws the host's `VkResult` away, so the allocation "succeeds" guest-side whether the
    /// host admitted it or killed the context for asking. The worker log is the only oracle —
    /// `vkr_budget_refused` names the size and the context, and the FATAL follows it.
    fn maybe_hog(&mut self) {
        let Some(mib) = self.hog_mib else { return };
        if self.hogged || self.render.imports_live() == 0 {
            return;
        }
        // WHEN is the harness's call, not ours. Firing on "an import exists" alone raced the
        // census-tick client every time: `refs` connects a throwaway client to make the worker
        // allocate, that import satisfied the condition, and the context was already dead before
        // the measurement window opened — with the refusal off the top of the log slice, so the
        // run read as "no refusal, both arms identical". Wait to be told.
        if !std::path::Path::new(HOG_TRIGGER).exists() {
            return;
        }
        self.hogged = true;
        println!("HOG SUBMITTED mib={mib} imports_live={}", self.render.imports_live());
        match self.render.vk.hog(mib) {
            Ok(_) => log::warn!("--hog-mib {mib}: guest-side Ok (says nothing — see the host log)"),
            Err(e) => log::warn!("--hog-mib {mib}: guest-side refusal {e:?}"),
        }
    }

    /// Composite whatever clients have committed, flip, then answer their frame callbacks.
    ///
    /// Order matters: a callback says "the content you committed has been shown", so sending
    /// it before the flip would be a lie the client then paces itself against.
    fn render_and_notify(&mut self) {
        let contents = std::mem::take(&mut self.pending);
        if let Err(e) = self.render.present(&contents) {
            log::error!("present: {e:#}");
            // Once the hog has tripped the budget, this context is FATAL and every later command
            // fails — usually as `VK_ERROR_OUT_OF_HOST_MEMORY`, which the venus ring returns for
            // any "could not get a reply", memory or not. Exiting here would be the wrong
            // vehicle: the process's destructors would run, the guest would destroy its
            // VkInstance, and the host would take the ordinary `instance was gone` teardown —
            // never the state path 4 is about. The 2026-08-06 incident's context sat FATAL for
            // 38 s with its client alive; staying up is what reproduces that.
            if self.hogged {
                log::warn!("staying alive with a FATAL context (path 4) — kill me to tear down");
                return;
            }
            self.running = false;
            return;
        }

        // Release dmabufs only now. `present` fence-waits every copy it records, so by this
        // point the GPU has provably finished reading the client's memory — releasing before
        // that would hand the buffer back while a copy was still fetching from it, which is a
        // tear at best and, being asynchronous, would reproduce intermittently.
        //
        // Under `--hold-buffers` we skip it on purpose, to hold the committed-and-unreleased
        // state open for the teardown tests.
        if !self.hold_buffers {
            for c in &contents {
                if let Content::Dmabuf(buffer) = c {
                    buffer.release();
                }
            }
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
                // Which kind it is decides who owns the pixels from here, and that is the whole
                // difference M3 adds. `get_dmabuf` succeeding is the discriminator smithay
                // gives us; anything else is shm (or unreadable, which is a client error).
                if get_dmabuf(&buffer).is_ok() {
                    // Deferred to `render_and_notify`, after the GPU copy has retired: the
                    // pixels are still in the *client's* allocation and we are about to read
                    // them directly.
                    self.pending.push(Content::Dmabuf(buffer));
                } else {
                    match read_shm(&buffer) {
                        Ok(surface) => self.pending.push(Content::Shm(surface)),
                        Err(e) => log::warn!("client buffer is neither dmabuf nor shm: {e:#}"),
                    }
                    // Released immediately: the pixels are ours now, in our own allocation. A
                    // compositor that holds an shm buffer past this point is holding a
                    // reference the client cannot see — the exact shape this vehicle exists to
                    // detect elsewhere.
                    if !self.hold_buffers {
                        buffer.release();
                    }
                }
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
    /// **The eviction obligation.** Fires when a client destroys a buffer and when a client
    /// dies holding one, which are the same event from here — so this is where the vehicle
    /// provably lets go of its side, and a host-side reference that survives past it is
    /// limina's to explain.
    fn buffer_destroyed(&mut self, buffer: &wl_buffer::WlBuffer) {
        self.render.evict(buffer);
    }
}

impl DmabufHandler for Comp {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    /// A client has finished describing a dmabuf and wants a `wl_buffer` for it.
    ///
    /// The import itself is **not** done here: the `wl_buffer` that keys the cache does not
    /// exist until `successful()` creates it. What is checked here is the shape, so a buffer we
    /// could never composite is refused at creation — where the protocol has a way to say so —
    /// rather than at the first commit, where the only options are a dropped frame or a lie.
    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: smithay::backend::allocator::dmabuf::Dmabuf,
        notifier: ImportNotifier,
    ) {
        let format = dmabuf.format();
        if dmabuf.num_planes() != 1 || format.modifier != Modifier::Linear {
            log::warn!(
                "refusing a client dmabuf: {} plane(s), modifier {:?} — this vehicle handles \
                 single-plane LINEAR only",
                dmabuf.num_planes(),
                format.modifier,
            );
            // Dropping the notifier without `successful()` is how smithay reports the failure
            // to the client; answering it either way is not optional, as the client is blocked
            // on the reply.
            return;
        }
        let _ = notifier.successful::<Comp>();
    }
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
delegate_dmabuf!(Comp);
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
