// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The limina native window (decision D3: the UI lives in the supervisor process).
//!
//! The worker publishes each guest scanout as a shared IOSurface and reports it over a
//! control fd (`surface <id> <w> <h>` / `frame`). Here, a reader thread parses that and
//! updates shared state; an `NSTimer` on the **main thread** polls the state and updates
//! the window's `CALayer.contents` to the looked-up IOSurface. All AppKit access stays on
//! the main thread; only plain data (ids/counters) crosses the thread boundary.
#![allow(deprecated)] // objc2-io-surface 0.3 renamed some free fns to methods.
//!
//! Split along its seams (2026-07-01 review, Part I recommendation 5): `present` (surface
//! store/reader/frame-apply/shown-ack), `cursor` (guest-cursor shape + capture compositing),
//! `lifecycle` (worker connection + quit policy), `diag` (capture / present-copy probes),
//! plus the pre-existing `input` and `capture_tap`. This file keeps `run()` — the window,
//! the render timer, and the event monitor.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::NSObjectProtocol;
use objc2::{define_class, msg_send, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSAlertSecondButtonReturn, NSApplication,
    NSApplicationActivationPolicy, NSApplicationDelegate, NSApplicationTerminateReply,
    NSBackingStoreType, NSColor, NSEvent, NSEventMask, NSEventType, NSMenu, NSMenuItem, NSScreen,
    NSView, NSViewLayerContentsRedrawPolicy, NSWindow, NSWindowCollectionBehavior,
    NSWindowDelegate, NSWindowStyleMask,
};
use objc2_core_foundation::CFRetained;
use objc2_foundation::{
    NSPoint, NSRect, NSRunLoop, NSRunLoopCommonModes, NSSize, NSString, NSTimer,
};
use objc2_io_surface::{IOSurfaceLookup, IOSurfaceRef};
use objc2_quartz_core::{CALayer, CATransaction};

mod capture_tap;
mod cursor;
mod diag;
pub(crate) mod fit;
mod hostdisplay;
mod input;
mod lifecycle;
mod overlay;
mod present;

pub use lifecycle::{WorkerConn, WorkerIo};
pub use present::{
    empty_surface_map, mark_worker_exited, mark_worker_suspended, spawn_reader, surface_rendezvous,
    Shared, SurfaceMap,
};

// `input` builds the host pointer's default (blank) shape from the cursor module; re-exported
// here so its `super::blank_cursor()` call keeps working across the split.
pub(crate) use cursor::blank_cursor;

use cursor::{apply_cursor, update_capture_cursor};
use diag::{
    capture_ids_from_env, capture_iosurface, copy_surface, create_local_iosurface, sync_surface,
};
use lifecycle::{kill_worker_group, should_initiate_quit};
use present::{register_apply_hook, set_layer_surface};

use limina_displayctl::DisplayCommand;

use crate::vmlib::schema::DisplayResolution;
use crate::vmlib::state::WindowState;

/// Everything `run` needs beyond the live worker channels: display policy, remembered
/// window state, and the resize plumbing. Groups what used to be positional arguments.
pub struct WindowOptions {
    pub resize_socket: Option<PathBuf>,
    pub remap: limina_input::keymap::KeyRemap,
    /// Soft keyboard grab: while the window is key, the tap consumes keyboard input (system
    /// combos included) for the guest, mouse left free. `--no-soft-kbd-grab` opts out.
    pub soft_kbd_grab: bool,
    pub title: String,
    /// Display mode: host (drive the guest to the window's screen size, letterboxing the
    /// window), dynamic (guest follows the window — the original behavior), or fixed.
    pub mode: DisplayResolution,
    /// The resolution the worker booted at.
    pub initial_size: (u32, u32),
    /// First-appearance window content size (points), used when there is no remembered
    /// frame — half the display's area at the guest aspect (`fit::default_window_content`).
    pub default_content: (u32, u32),
    /// Remembered NSWindow frame to restore, if it still lands on a screen.
    pub restore_frame: Option<[f64; 4]>,
    /// Persist window state here (managed VMs: the bundle's state.toml); None = off.
    pub state_path: Option<PathBuf>,
    /// Shared with the reboot-relaunch monitor: every resolution push records itself here
    /// so a relaunched worker boots at the current size (see `session::pack_size`).
    pub desired_size: Arc<std::sync::atomic::AtomicU64>,
    /// What closing the window does (M9.4): suspend (default), shutdown, or ask. Already
    /// resolved by the session — Suspend only arrives when suspend is armed.
    pub on_window_close: crate::vmlib::schema::WindowCloseAction,
    /// Where the last-presented frame is saved when the worker suspends (the restore
    /// splash, next to the snapshot). None = flat-CLI VM, no splash.
    pub splash_save_path: Option<PathBuf>,
    /// A splash to show from window creation until the first presented frame — set when
    /// this boot restores from a snapshot and the splash file exists.
    pub restore_splash: Option<PathBuf>,
    /// The VM-menu context (Suspend gating, Show in Finder, Copy SSH Command).
    pub menu_ctx: MenuCtx,
    /// Drive the guest at the window's screen in device pixels rather than points, so a Retina
    /// panel renders natively (`[display] hidpi`, default on). See [`fit::Scale`].
    pub hidpi: bool,
    /// What fullscreen does with the camera housing on a notched built-in display
    /// (`[display] notch`, default `avoid`). See [`crate::vmlib::schema::NotchPolicy`].
    pub notch: crate::vmlib::schema::NotchPolicy,
    /// Points of push needed to move the pointer past a fullscreen guest's edge
    /// (`[display] edge-resistance`; 0 disables). See [`fit::EdgeResist`].
    pub edge_resistance: f64,
}

/// The point-to-guest-pixel scale for the screen a window is currently on. Recomputed per use
/// rather than cached: dragging between a Retina and a 1x display changes it, and AppKit reports
/// no screen at all mid-transition (there, the historical 1:1 is the safe answer).
fn scale_for(window: &NSWindow, hidpi: bool) -> fit::Scale {
    match window.screen() {
        Some(screen) => fit::Scale::new(screen.backingScaleFactor(), hidpi),
        None => fit::Scale::none(),
    }
}

/// Point geometry of the screen a windowed boot targets: `frame` is the full screen size
/// (the match-host guest resolution), `visible` the size minus menu bar/Dock (the
/// window-content clamp for the first-appearance default).
pub struct ScreenInfo {
    pub frame: (u32, u32),
    pub visible: (f64, f64),
    /// `NSScreen.backingScaleFactor` — 2.0 on Retina. Match-host multiplies `frame` by it to
    /// get the boot resolution under HiDPI; window sizing stays in points.
    pub backing: f64,
}

/// The screen a remembered window frame lands on (by its midpoint), or the main screen
/// when there is no frame / no match. `None` off the main thread or on a screen-less
/// host — callers fall back to configured sizes. This is how match-host mode derives the
/// initial guest resolution BEFORE any window exists.
///
/// `notch` decides whether `frame` withholds the camera-housing strip on a notched built-in
/// display; the boot resolution has to agree with what the running window will do, or host mode
/// would modeset the guest on the first tick.
pub fn screen_info_for_frame(
    frame: Option<[f64; 4]>,
    notch: crate::vmlib::schema::NotchPolicy,
) -> Option<ScreenInfo> {
    let mtm = MainThreadMarker::new()?;
    let by_midpoint = frame.and_then(|f| {
        let (mx, my) = (f[0] + f[2] / 2.0, f[1] + f[3] / 2.0);
        NSScreen::screens(mtm).into_iter().find(|s| {
            let sf = s.frame();
            mx >= sf.origin.x
                && mx < sf.origin.x + sf.size.width
                && my >= sf.origin.y
                && my < sf.origin.y + sf.size.height
        })
    });
    let screen = by_midpoint.or_else(|| NSScreen::mainScreen(mtm))?;
    let sz = screen.frame().size;
    let vis = screen.visibleFrame().size;
    let (sw, sh) = fit::usable_content(sz.width, sz.height, notch_inset_for(&screen, notch));
    Some(ScreenInfo {
        frame: (sw.round() as u32, sh.round() as u32),
        visible: (vis.width, vis.height),
        backing: screen.backingScaleFactor(),
    })
}

/// The housing height to withhold from the guest on this screen: the screen's real notch under
/// the `avoid` policy, nothing under `extend` (and nothing on a screen with no housing).
fn notch_inset_for(screen: &NSScreen, notch: crate::vmlib::schema::NotchPolicy) -> f64 {
    match notch {
        // Not the housing height but the height AppKit's own fullscreen inset costs, which is
        // the thing the guest has to match under `avoid` — they differ by a point. See
        // [`hostdisplay::fullscreen_inset`].
        crate::vmlib::schema::NotchPolicy::Avoid => hostdisplay::fullscreen_inset(screen),
        crate::vmlib::schema::NotchPolicy::Extend => 0.0,
    }
}

/// Does this window frame (screen points) intersect any current screen? Guards restoring
/// a frame remembered on a since-unplugged display (which would open the window off-screen).
fn frame_on_some_screen(frame: NSRect, mtm: MainThreadMarker) -> bool {
    NSScreen::screens(mtm).into_iter().any(|s| {
        let sf = s.frame();
        frame.origin.x < sf.origin.x + sf.size.width
            && frame.origin.x + frame.size.width > sf.origin.x
            && frame.origin.y < sf.origin.y + sf.size.height
            && frame.origin.y + frame.size.height > sf.origin.y
    })
}

/// Constrain interactive window resize to a fixed aspect ratio, so the content view can't
/// be dragged to a shape the guest never fills (which would only grow the letterbox bars).
/// Host mode locks to the display's aspect (re-applied on display migration); fixed mode to
/// the configured WxH; dynamic stays unconstrained — there the guest follows the window, so
/// a free resize is the point. `setContentAspectRatio` only bounds *user* resizing (and the
/// zoom button); it never resizes the window itself, so the boot frame and any restored frame
/// are left as-is and the letterbox still absorbs any residual mismatch until the next drag.
/// A degenerate ratio is ignored (leaves the window free rather than wedging resize).
fn apply_aspect_lock(window: &NSWindow, ratio: (u32, u32)) {
    if ratio.0 > 0 && ratio.1 > 0 {
        window.setContentAspectRatio(NSSize::new(f64::from(ratio.0), f64::from(ratio.1)));
    }
}

/// Apply a fit rect as the scanout layer's frame, with implicit animation off (same
/// reason as `set_layer_surface`: CA's default action would tween the letterbox).
fn set_layer_frame(layer: &CALayer, r: fit::FitRect) {
    CATransaction::begin();
    CATransaction::setDisableActions(true);
    layer.setFrame(NSRect::new(NSPoint::new(r.x, r.y), NSSize::new(r.w, r.h)));
    CATransaction::commit();
}

/// Snapshot the window's frame + content size for state.toml. `None` while fullscreen (the
/// *windowed* frame is what we remember) or before the window has a real size. The `extend`
/// overlay needs no case of its own: it exists only while the carrier is natively fullscreen,
/// and the carrier is what is asked here.
fn window_state_snapshot(window: &NSWindow) -> Option<WindowState> {
    if window.styleMask().contains(NSWindowStyleMask::FullScreen) {
        return None;
    }
    let f = window.frame();
    let c = window.contentView()?.frame().size;
    let (cw, ch) = (c.width.round() as u32, c.height.round() as u32);
    if cw < 64 || ch < 64 {
        return None;
    }
    Some(WindowState {
        frame: [f.origin.x, f.origin.y, f.size.width, f.size.height],
        content: (cw, ch),
    })
}

/// Best-effort synchronous state save for the exit paths (the periodic saver is async,
/// so a close right after a move could otherwise lose the final position).
fn save_state_final(path: Option<&Path>, window: &NSWindow) {
    let (Some(path), Some(snap)) = (path, window_state_snapshot(window)) else {
        return;
    };
    // Merge, never whole-save: on a windowed suspend the monitor thread has just written
    // [suspended]; a full save here would consume it and the VM would cold-boot (M9.4-1a).
    if let Err(e) = crate::vmlib::state::set_window(path, Some(snap)) {
        log::warn!("window state save failed: {e}");
    }
}

/// The `on_window_close = ask` dialog: Suspend / Shut Down / Cancel (→ `None`). Modal on the
/// main thread — the render timer pauses for the answer, which is fine (the guest keeps
/// running; frames resume with the next tick).
fn ask_close_action(
    mtm: MainThreadMarker,
    vm_name: &str,
) -> Option<crate::vmlib::schema::WindowCloseAction> {
    use crate::vmlib::schema::WindowCloseAction;
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str(&format!("Close “{vm_name}”?")));
    alert.setInformativeText(&NSString::from_str(
        "Suspend parks the VM and resumes it the next time it starts; \
         Shut Down powers the guest off.",
    ));
    alert.addButtonWithTitle(&NSString::from_str("Suspend"));
    alert.addButtonWithTitle(&NSString::from_str("Shut Down"));
    alert.addButtonWithTitle(&NSString::from_str("Cancel"));
    let r = alert.runModal();
    if r == NSAlertFirstButtonReturn {
        Some(WindowCloseAction::Suspend)
    } else if r == NSAlertSecondButtonReturn {
        Some(WindowCloseAction::Shutdown)
    } else {
        None
    }
}

/// Push a window-resize to the worker over its display-control socket.
fn send_resize(path: &Path, width: u32, height: u32) {
    send_display_command(path, DisplayCommand::Resize { width, height });
}

/// Push a display command to the worker over its display-control socket (off the AppKit main
/// thread — a brief connect/write must never beachball the UI). Best-effort: a failure just
/// means this gesture's update is dropped; the next one retries.
fn send_display_command(path: &Path, command: DisplayCommand) {
    let path = path.to_path_buf();
    let line = command.to_wire();
    std::thread::spawn(move || {
        use std::io::Write;
        match std::os::unix::net::UnixStream::connect(&path) {
            Ok(mut stream) => {
                if let Err(e) = writeln!(stream, "{line}") {
                    log::warn!("display-control: send {line:?} failed: {e}");
                } else {
                    log::info!("display-control: pushed {line:?} to the guest");
                }
            }
            Err(e) => log::warn!("display-control: connect {path:?} failed: {e}"),
        }
    });
}

/// What the VM menu's items need to know about THIS VM (M9.4). Set once by `run` before
/// the menu is built; read by the action methods and the Dock-menu builder. Main-thread
/// only, like all menu state.
#[derive(Clone, Default)]
pub(crate) struct MenuCtx {
    /// Suspend is armed (managed VM: snapshot + state paths both set) — gates the
    /// Suspend item.
    pub(crate) suspend_armed: bool,
    /// The VM's .liminavm bundle directory — gates Show in Finder.
    pub(crate) bundle_dir: Option<PathBuf>,
    /// The ready-to-paste SSH command (NAT gateway forward) — gates Copy SSH Command.
    pub(crate) ssh_cmd: Option<String>,
}

thread_local! {
    static MENU_CTX: RefCell<MenuCtx> = RefCell::new(MenuCtx::default());
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no Drop; the action
    // signatures below match AppKit's target/action convention.
    #[unsafe(super = objc2_foundation::NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "LiminaVmMenuActions"]
    pub struct VmMenuActions;

    unsafe impl NSObjectProtocol for VmMenuActions {}

    unsafe impl NSApplicationDelegate for VmMenuActions {
        // A quit Apple event (osascript "quit", logout) must not exit the
        // supervisor abruptly — that orphans the worker (live-reproduced
        // 2026-07-02). Cancel the terminate and route into the same graceful
        // stop the Ctrl-C/window-close path uses; the render timer drives the
        // shutdown ladder and exits the process when the guest is down.
        #[unsafe(method(applicationShouldTerminate:))]
        fn application_should_terminate(
            &self,
            _app: &NSApplication,
        ) -> NSApplicationTerminateReply {
            crate::supervisor::request_stop();
            NSApplicationTerminateReply::TerminateCancel
        }

        // The Dock icon's context menu carries the same VM verbs as the menu bar.
        #[unsafe(method_id(applicationDockMenu:))]
        fn application_dock_menu(&self, _app: &NSApplication) -> Option<Retained<NSMenu>> {
            Some(build_vm_menu(self.mtm(), self))
        }
    }

    impl VmMenuActions {
        // "Control Center…" / "Settings…": show the running center or spawn a fresh one —
        // the way back to the center (where VM configuration lives), Parallels-style.
        #[unsafe(method(showControlCenter:))]
        fn show_control_center(&self, _sender: &NSMenuItem) {
            if let Err(e) = crate::center::show_or_spawn() {
                log::warn!("opening the control center: {e:#}");
            }
        }

        // Suspend: same path as close-to-suspend / `limina suspend` — the monitor relays
        // the bracket, the overlay dims the live frame, the exit persists [suspended].
        #[unsafe(method(suspendVm:))]
        fn suspend_vm(&self, _sender: &NSMenuItem) {
            log::info!("menu: Suspend");
            crate::supervisor::request_suspend();
        }

        // Shut Down: the graceful power-off ladder (agent shutdown → power button →
        // SIGKILL after grace) — identical to Ctrl-C / `limina stop`.
        #[unsafe(method(shutDownVm:))]
        fn shut_down_vm(&self, _sender: &NSMenuItem) {
            log::info!("menu: Shut Down");
            crate::supervisor::request_stop();
        }

        // Force Stop: SIGKILL now, no grace — the hung-guest escape hatch.
        #[unsafe(method(forceStopVm:))]
        fn force_stop_vm(&self, _sender: &NSMenuItem) {
            log::warn!("menu: Force Stop (no grace)");
            crate::supervisor::request_force_stop();
        }

        // Show in Finder: reveal the .liminavm bundle.
        #[unsafe(method(revealVm:))]
        fn reveal_vm(&self, _sender: &NSMenuItem) {
            let Some(dir) = MENU_CTX.with(|c| c.borrow().bundle_dir.clone()) else {
                return;
            };
            let url = objc2_foundation::NSURL::fileURLWithPath(&NSString::from_str(
                &dir.to_string_lossy(),
            ));
            objc2_app_kit::NSWorkspace::sharedWorkspace().activateFileViewerSelectingURLs(
                &objc2_foundation::NSArray::from_retained_slice(&[url]),
            );
        }

        // Copy SSH Command: the NAT gateway's inbound forward, ready to paste.
        #[unsafe(method(copySshVm:))]
        fn copy_ssh_vm(&self, _sender: &NSMenuItem) {
            let Some(cmd) = MENU_CTX.with(|c| c.borrow().ssh_cmd.clone()) else {
                return;
            };
            unsafe {
                let pb = objc2_app_kit::NSPasteboard::generalPasteboard();
                pb.clearContents();
                pb.setString_forType(
                    &NSString::from_str(&cmd),
                    objc2_app_kit::NSPasteboardTypeString,
                );
            }
        }
    }
);

/// Build the "Virtual Machine" verbs menu (shared between the menu bar and the Dock menu).
/// Items whose prerequisite is absent (no bundle, no SSH forward, suspend unarmed) are
/// simply not added — the menu never shows dead verbs.
fn build_vm_menu(mtm: MainThreadMarker, actions: &VmMenuActions) -> Retained<NSMenu> {
    let ctx = MENU_CTX.with(|c| c.borrow().clone());
    let menu = NSMenu::new(mtm);
    menu.setTitle(&NSString::from_str("Virtual Machine"));
    let add = |title: &str, sel: objc2::runtime::Sel, key: &str| {
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(title),
                Some(sel),
                &NSString::from_str(key),
            )
        };
        unsafe { item.setTarget(Some(actions)) };
        menu.addItem(&item);
    };
    if ctx.suspend_armed {
        add("Suspend", objc2::sel!(suspendVm:), "");
    }
    add("Shut Down", objc2::sel!(shutDownVm:), "");
    add("Force Stop", objc2::sel!(forceStopVm:), "");
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    add("Settings…", objc2::sel!(showControlCenter:), ",");
    if ctx.bundle_dir.is_some() {
        add("Show in Finder", objc2::sel!(revealVm:), "");
    }
    if ctx.ssh_cmd.is_some() {
        add("Copy SSH Command", objc2::sel!(copySshVm:), "");
    }
    menu
}

thread_local! {
    /// Set by [`WindowCloseInterceptor`] when the user asked to close the window (red
    /// button / Cmd-W); consumed by the render timer, which routes it through the
    /// `on_window_close` policy. Main-thread only, like all window state.
    static CLOSE_REQUESTED: Cell<bool> = const { Cell::new(false) };
}

define_class!(
    // SAFETY: NSWindow tolerates subclassing; no Drop; no ivars. The only overrides are the
    // two key-eligibility predicates, and they return the value a titled window would return
    // anyway — so this class behaves exactly like NSWindow until the window goes borderless.
    #[unsafe(super = NSWindow)]
    #[thread_kind = MainThreadOnly]
    #[name = "LiminaWindow"]
    pub(crate) struct LiminaWindow;

    unsafe impl NSObjectProtocol for LiminaWindow {}

    impl LiminaWindow {
        /// A **borderless** `NSWindow` refuses key status, and the `extend` overlay
        /// ([`ExtendOverlay`]) must be borderless — it is the only style the compositor
        /// will let draw beside the camera housing (titled + `fullSizeContentView` is masked;
        /// measured in `spikes/notch-fullscreen/` round 2). A VM window that cannot take
        /// keyboard focus is useless, so override the refusal.
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key_window(&self) -> bool {
            true
        }

        #[unsafe(method(canBecomeMainWindow))]
        fn can_become_main_window(&self) -> bool {
            true
        }
    }
);

/// Window level for the `extend` overlay: above the menu bar, so the chrome cannot reveal over
/// the guest. `NSMainMenuWindowLevel` is 24.
const OVERLAY_LEVEL: isize = 25;

/// The `notch = extend` overlay: native fullscreen as a **carrier** for the Space, with the guest
/// drawn by a borderless window floating on top of it.
///
/// Neither half works alone. A Space cannot draw beside the camera housing — Apple documents the
/// inset as unconditional ("the system automatically positions the window's contents within the
/// safe area") — while a borderless window is not a Space, so using one on its own costs Mission
/// Control, the swipe and the fullscreen animation. Floating a `.fullScreenAuxiliary` window over
/// the carrier's Space gives both, and because the overlay sits above menu-bar level the chrome
/// cannot appear over the guest at all. All three measured in `spikes/notch-fullscreen/` round 5.
///
/// The trick that keeps this small: the overlay gets no view of its own. The **same** `NSView` —
/// scanout layer, cursor sublayer and every input binding already attached — is re-parented into
/// it and back out. Nothing that holds the view needs to know.
#[derive(Default)]
pub(crate) struct ExtendOverlay {
    window: RefCell<Option<Retained<NSWindow>>>,
    /// Read by the capture tap, which has no access to the `Rc` graph. One fact, one owner.
    active: Arc<std::sync::atomic::AtomicBool>,
}

impl ExtendOverlay {
    fn is_active(&self) -> bool {
        self.active.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn flag(&self) -> Arc<std::sync::atomic::AtomicBool> {
        self.active.clone()
    }

    /// The view the guest is drawn in and receives input through: the overlay's while it is up,
    /// the carrier's otherwise — the same object either way.
    fn active_view(&self, carrier: &NSWindow) -> Option<Retained<NSView>> {
        match self.window.borrow().as_ref() {
            Some(w) => w.contentView(),
            None => carrier.contentView(),
        }
    }

    fn show(&self, carrier: &NSWindow, mtm: MainThreadMarker) {
        if self.is_active() {
            return;
        }
        let (Some(screen), Some(view)) = (carrier.screen(), carrier.contentView()) else {
            return;
        };
        let overlay: Retained<NSWindow> = unsafe {
            let w: Retained<LiminaWindow> = msg_send![
                LiminaWindow::alloc(mtm),
                initWithContentRect: screen.frame(),
                styleMask: NSWindowStyleMask::Borderless,
                backing: NSBackingStoreType::Buffered,
                defer: false,
            ];
            Retained::cast_unchecked(w)
        };
        // Borderless is the only style the compositor lets draw beside the housing;
        // FullScreenAuxiliary is what lets the window join the carrier's Space rather than
        // yanking the user out of it.
        overlay.setCollectionBehavior(NSWindowCollectionBehavior::FullScreenAuxiliary);
        // MUST be false. `isReleasedWhenClosed` defaults to TRUE for a programmatically created
        // NSWindow, so `close()` releases it out from under the `Retained` we are holding — an
        // over-release that segfaults in the next autorelease-pool drain, i.e. inside
        // `NSApplication::run`, nowhere near the code that caused it. Cost a crash on the first
        // Cmd-Tab out of the overlay (2026-08-01).
        // SAFETY: plain property setter on a window we own and have not yet shown.
        unsafe { overlay.setReleasedWhenClosed(false) };
        overlay.setLevel(OVERLAY_LEVEL);
        overlay.setOpaque(true);
        overlay.setBackgroundColor(Some(NSColor::blackColor().as_ref()));
        overlay.setContentView(Some(&view));
        // Never leave the carrier with a nil content view. It is never seen — the overlay covers
        // it — but AppKit is happier with something there.
        carrier.setContentView(Some(&NSView::new(mtm)));
        overlay.setFrame_display(screen.frame(), true);
        overlay.makeKeyAndOrderFront(None);
        self.active
            .store(true, std::sync::atomic::Ordering::Relaxed);
        *self.window.borrow_mut() = Some(overlay);
    }

    fn hide(&self, carrier: &NSWindow) {
        let Some(overlay) = self.window.borrow_mut().take() else {
            return;
        };
        self.active
            .store(false, std::sync::atomic::Ordering::Relaxed);
        if let Some(view) = overlay.contentView() {
            carrier.setContentView(Some(&view));
        }
        overlay.orderOut(None);
        overlay.close();
        carrier.makeKeyAndOrderFront(None);
    }

    /// Keep the overlay in step with the carrier, from the tick that already runs — polled rather
    /// than delegate-driven, like every other window-state read here.
    ///
    /// Up only while all of these hold:
    /// - the policy is `extend`;
    /// - the carrier is natively fullscreen (the overlay needs a Space to float over);
    /// - the screen actually **has** a camera housing — on an external display native fullscreen
    ///   already covers everything, so an overlay would be risk for no pixels;
    /// - **the app is active.** An overlay above menu-bar level would otherwise float over
    ///   whatever the user switched to, so Cmd-Tabbing away has to put it down. Dropping it
    ///   returns the view to the carrier, so the Space still shows the guest (inset below the
    ///   housing) — the right look for a background app anyway.
    /// - **the user is not asking for the chrome.** Nothing can reveal over the overlay, which is
    ///   the point of it, but the menu bar and the window's controls still have to be reachable
    ///   for the VM's own menu actions. A deliberate shove at the top edge (the edge-resistance
    ///   breakthrough, uncaptured only) sets `reveal_chrome` and puts the overlay down until the
    ///   pointer returns to the guest.
    fn reconcile(
        &self,
        carrier: &NSWindow,
        notch: crate::vmlib::schema::NotchPolicy,
        app: &NSApplication,
        reveal_chrome: bool,
    ) {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let screen = carrier.screen();
        let want = notch == crate::vmlib::schema::NotchPolicy::Extend
            && !reveal_chrome
            && carrier.styleMask().contains(NSWindowStyleMask::FullScreen)
            && app.isActive()
            && screen
                .as_ref()
                .is_some_and(|s| hostdisplay::notch_inset(s) > 0.0);
        if want == self.is_active() {
            // No switch, but the overlay has no AppKit machinery keeping it on the screen: a
            // display reconfigured under it would leave it the wrong size.
            if let (Some(overlay), Some(screen)) = (self.window.borrow().as_ref(), screen) {
                if overlay.frame() != screen.frame() {
                    overlay.setFrame_display(screen.frame(), true);
                }
            }
            return;
        }
        if want {
            self.show(carrier, mtm);
        } else {
            self.hide(carrier);
        }
    }
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no Drop; only
    // `windowShouldClose:` is implemented, so every other delegate behavior keeps its
    // AppKit default (the window otherwise stays on its deliberate no-delegate diet).
    #[unsafe(super = objc2_foundation::NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "LiminaWindowCloseInterceptor"]
    struct WindowCloseInterceptor;

    unsafe impl NSObjectProtocol for WindowCloseInterceptor {}

    unsafe impl NSWindowDelegate for WindowCloseInterceptor {
        // Intercept the close (M9.4, user-decided over hide-and-reopen): the window must
        // STAY VISIBLE while the close policy runs — a suspend shows the dim + spinner in
        // place for the save's ~10-20s, an Ask dialog's Cancel is a true no-op, and a
        // shutdown closes the window programmatically. Returning NO here is what keeps
        // the red button from hiding the window before any of that can be seen.
        #[unsafe(method(windowShouldClose:))]
        fn window_should_close(&self, _sender: &NSWindow) -> bool {
            CLOSE_REQUESTED.with(|c| c.set(true));
            false
        }
    }
);

/// The VM window's minimal main menu: Control Center… (Cmd-Shift-C) and Close
/// Window (Cmd-W — routes to `performClose:`, i.e. the normal VM-shutdown path).
/// The actions object also becomes the app delegate so quit Apple events go
/// through the graceful stop instead of exiting the supervisor under the worker.
fn install_main_menu(mtm: MainThreadMarker, app: &NSApplication) {
    let actions: Retained<VmMenuActions> = unsafe { msg_send![VmMenuActions::alloc(mtm), init] };
    app.setDelegate(Some(objc2::runtime::ProtocolObject::from_ref(&*actions)));
    let menubar = NSMenu::new(mtm);
    let app_item = NSMenuItem::new(mtm);
    menubar.addItem(&app_item);
    let app_menu = NSMenu::new(mtm);
    let cc = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str("Control Center…"),
            Some(objc2::sel!(showControlCenter:)),
            &NSString::from_str("C"), // uppercase ⇒ Cmd-Shift-C
        )
    };
    unsafe { cc.setTarget(Some(&*actions)) };
    app_menu.addItem(&cc);
    let close = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str("Close Window"),
            Some(objc2::sel!(performClose:)),
            &NSString::from_str("w"),
        )
    };
    app_menu.addItem(&close);
    app_item.setSubmenu(Some(&app_menu));
    // The VM verbs menu (M9.4): every lifecycle action reachable from the menu bar
    // (and the same set from the Dock icon via applicationDockMenu:).
    let vm_item = NSMenuItem::new(mtm);
    menubar.addItem(&vm_item);
    vm_item.setSubmenu(Some(&build_vm_menu(mtm, &actions)));
    app.setMainMenu(Some(&menubar));
    // NSMenuItem targets are weak; the actions object must live as long as the menu.
    std::mem::forget(actions);
}

/// Run the AppKit window on the main thread. The render timer polls `shared`, updates the
/// window contents, and — when the worker exits, the window is closed, or Ctrl-C is hit —
/// kills the worker's process group (`worker_pid`) and exits the process. (We exit from
/// the timer rather than `NSApplication::stop`, which doesn't return without a UI event.)
pub fn run(
    shared: Arc<Mutex<Shared>>,
    mtm: MainThreadMarker,
    conn: Arc<WorkerConn>,
    control: Option<crate::control::ControlPlane>,
    surface_map: SurfaceMap,
    opts: WindowOptions,
) -> ! {
    let WindowOptions {
        resize_socket,
        remap,
        soft_kbd_grab,
        title,
        mode,
        initial_size,
        default_content,
        restore_frame,
        state_path,
        desired_size,
        on_window_close,
        splash_save_path,
        restore_splash,
        menu_ctx,
        hidpi,
        notch: cfg_notch,
        edge_resistance,
    } = opts;

    // The VM menu reads this when install_main_menu (below) builds it — set it first.
    MENU_CTX.with(|c| *c.borrow_mut() = menu_ctx);

    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    install_main_menu(mtm, &app);

    // First-appearance content size: half the display's area at the guest aspect (a
    // remembered frame overrides this below). In dynamic mode the guest boots at exactly
    // this size, so the first presented frame is 1:1 from tick zero.
    let rect = NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(
            f64::from(default_content.0.max(64)),
            f64::from(default_content.1.max(64)),
        ),
    );
    let style = NSWindowStyleMask::Titled
        | NSWindowStyleMask::Closable
        | NSWindowStyleMask::Miniaturizable
        | NSWindowStyleMask::Resizable;
    // `LiminaWindow`, not a plain NSWindow: the `extend` overlay is borderless and carries the
    // guest, and only a subclass stays key-eligible there. Identical to NSWindow otherwise.
    let window: Retained<NSWindow> = unsafe {
        let w: Retained<LiminaWindow> = msg_send![
            LiminaWindow::alloc(mtm),
            initWithContentRect: rect,
            styleMask: style,
            backing: NSBackingStoreType::Buffered,
            defer: false,
        ];
        Retained::cast_unchecked(w)
    };
    window.setTitle(&NSString::from_str(&title));
    // Allow native (Spaces) full screen: the green title-bar button becomes Enter Full Screen
    // and `toggleFullScreen:` (our Cmd-Ctrl-F host shortcut, below) works. Going fullscreen
    // resizes the window, which the existing resize path reflows into the guest resolution.
    //
    // Under `notch = extend` the green button is deliberately left on the NATIVE path even
    // though Cmd-Ctrl-F does panel fullscreen: native fullscreen still works, it just cannot
    // use the housing strip. Two doors to two different fullscreens is worse than one door to
    // the good one, so this is a wart to revisit — noted in docs/design/display-modes.md.
    window.setCollectionBehavior(NSWindowCollectionBehavior::FullScreenPrimary);
    let view = window.contentView().expect("content view");
    // Layer-HOSTING (not layer-backed): we own the layer, so AppKit never draws over the
    // IOSurface we set as its contents. Order matters: setLayer before setWantsLayer.
    let layer = CALayer::new();
    // The guest scanout is XRGB (opaque; the X/alpha channel is "don't care" and Venus/zink
    // leaves it 0). Our IOSurface is BGRA, so without this Core Animation alpha-blends the
    // surface and a 0 alpha makes the whole desktop composite transparent (reads as black/
    // white depending on the backdrop). Mark the scanout layer opaque so CA ignores alpha.
    layer.setOpaque(true);
    // Pointer-capture cursor overlay: a sublayer we composite the guest cursor into at its
    // guest-reported position while captured (the host NSCursor is hidden then, so without this
    // the cursor would vanish). Hidden by default; positioned/shown by `update_capture_cursor`.
    let cursor_layer = CALayer::new();
    cursor_layer.setHidden(true);
    layer.addSublayer(&cursor_layer);
    view.setLayer(Some(&layer));
    view.setWantsLayer(true);
    // We OWN the layer's contents (the guest IOSurface) — AppKit must never invalidate or redraw
    // them. Without `Never`, the default policy lets AppKit manage the layer across a live window
    // resize, and at the end of the drag it leaves the layer blank (the IOSurface contents are
    // dropped) until a large enough frame change reallocates the backing. `Never` tells AppKit to
    // keep its hands off so our present is the sole authority on what the layer shows.
    view.setLayerContentsRedrawPolicy(NSViewLayerContentsRedrawPolicy::Never);
    // The letterbox bars ARE the window background: in host/fixed modes the scanout layer
    // aspect-fits inside the content view and the uncovered margin shows the window
    // background — black so the bars read as bars. (In dynamic mode the layer fills the
    // window, so this is invisible.)
    window.setBackgroundColor(Some(&NSColor::blackColor()));
    // The runtime resize path never asks the guest for less than 64 pt; don't let the
    // window shrink below what the guest can be driven to.
    window.setContentMinSize(NSSize::new(64.0, 64.0));
    // Restore the remembered frame when it still lands on a live screen (a frame from a
    // since-unplugged display would open the window off-screen); otherwise center.
    let restored = restore_frame
        .map(|f| NSRect::new(NSPoint::new(f[0], f[1]), NSSize::new(f[2], f[3])))
        .filter(|r| frame_on_some_screen(*r, mtm));
    match restored {
        Some(r) => window.setFrame_display(r, false),
        None => window.center(),
    }
    // Lock interactive resize to the guest's aspect so the window can't be dragged into a
    // shape that only grows the letterbox. Host mode uses the display's aspect (`initial_size`
    // is the screen the window boots on; re-applied below on display migration); fixed mode
    // the configured WxH. Dynamic stays free — the guest follows the window there.
    match mode {
        DisplayResolution::Host => apply_aspect_lock(&window, initial_size),
        DisplayResolution::Fixed(w, h) => apply_aspect_lock(&window, (w, h)),
        DisplayResolution::Dynamic => {}
    }
    // Required for hover (non-dragging) motion to be delivered as MouseMoved events.
    window.setAcceptsMouseMovedEvents(true);
    window.makeKeyAndOrderFront(None);
    app.activate();

    // Per-timer state (main thread only).
    let last_gen = Cell::new(0u64);
    let geom = Cell::new((0u32, 0u32));
    // Diagnostic: dump the presented IOSurface to a PNG (no screen-record perm). LIMINA_WINDOW_CAPTURE.
    let capture_path = std::env::var("LIMINA_WINDOW_CAPTURE").ok();
    // Diagnostic: ids to ALSO dump by global lookup each tick (LIMINA_CAPTURE_IDS — see
    // diag::capture_ids_from_env for the format and why).
    let capture_ids: Vec<u32> = capture_ids_from_env();
    let applies = Cell::new(0u64);
    // Dynamic mode's runtime window-resize → guest. The 60 Hz timer debounces the window's
    // content size and, once a drag settles, pushes the new size to the worker over
    // `resize_socket` (which forwards it to the live virtio-gpu → the guest re-modesets).
    // `geom` (the guest's current resolution) is the feedback guard: a window that already
    // matches it — including the guest-driven setContentSize echo — sends nothing. See
    // docs/design/runtime-display-resize.md.
    let resize_sent: Cell<(u32, u32)> = Cell::new((0, 0));
    // Host mode's screen tracker: the screen size the guest was last driven to, plus the
    // identity key of the display it came from. Seeded with the boot size (derived from the
    // same screen) and a zero key, so the first poll pushes the full identity once and every
    // later poll is a no-op until something actually changes. Tracking the identity — not just
    // the size — is what catches a move between two same-sized displays, which the size-only
    // check missed entirely.
    let screen_sent: Cell<((u32, u32), u64)> = Cell::new((initial_size, 0));
    // The display identity the guest was last told about, tracked for ALL modes (host folds
    // its push into the size push; dynamic and fixed send identity on its own). Seeded to 0,
    // which no real display hashes to, so the first poll after the guest presents a frame
    // hands over the identity of whichever display it booted on.
    let identity_sent: Cell<u64> = Cell::new(0);
    // The scanout layer's current placement inside the content view, recomputed every tick
    // (dynamic: the full view — CA stretches the stale surface during a live drag exactly as
    // before; host/fixed: the guest resolution aspect-fit onto the black background). Shared
    // with the input path so the pointer transform can never disagree with the pixels.
    let fit_cell: std::rc::Rc<Cell<fit::FitRect>> = std::rc::Rc::new(Cell::new(
        fit::FitRect::full(f64::from(initial_size.0), f64::from(initial_size.1)),
    ));
    // Window-state persistence (state.toml): the settle-debounced candidate + what's on disk.
    let pending_state: Cell<Option<WindowState>> = Cell::new(None);
    let stable_ticks: Cell<u32> = Cell::new(0);
    let saved_state: Cell<Option<WindowState>> = Cell::new(None);
    // Diagnostic (LIMINA_PRESENT_COPY=1): never hand the GUEST's scanout surface to Core
    // Animation — copy it into a private 3-deep ring and show the copy. The zero-copy venus
    // path shares mutter's own double-buffered swapchain with the window server; with no
    // flip-completion feedback the guest reuses/repaints a buffer while CA may still be
    // compositing it (IOSurfaceLock is advisory), so mid-repaint states reach glass — seen
    // as the damaged region (a busy window) blinking out while the rest stays intact. The
    // copy decouples CA from the guest's write cycle entirely; if the flicker vanishes with
    // this on, that race is convicted (the real fix is flip-completion pacing, roadmap #8).
    // Env arms it for the whole run; the marker file toggles it LIVE (touch/rm
    // /tmp/limina-present-copy) so an intermittent flicker can be A/B'd within one session —
    // flicker present → touch → gone → rm → returns is the within-session conviction.
    let present_copy_env = std::env::var_os("LIMINA_PRESENT_COPY").is_some();
    // Lock-only variant (LIMINA_PRESENT_LOCK / touch /tmp/limina-present-lock): keep zero-copy,
    // but IOSurfaceLock+Unlock the guest surface before handing it to CA.
    // A/B VERDICT (2026-06-11): FAILED — visibly worse than no mitigation at all (several
    // anomalies within seconds vs ~5 bursts/hour untreated). Kept as a documented negative
    // result: the copy's load-bearing property is IMMUTABILITY, not the GPU-write sync.
    // (a) At present time the repaint may not be submitted to Metal yet (venus ring decode
    // is async), so the lock has nothing to wait on exactly when it matters; (b) even a
    // complete-at-lock frame is repainted by the guest ~33ms later while CA still samples
    // it (the SURFACE_RING reuse race). Do not enable; use LIMINA_PRESENT_COPY. COPY wins if
    // both are set.
    let present_lock_env = std::env::var_os("LIMINA_PRESENT_LOCK").is_some();
    // The live /tmp marker toggles are re-stat'ed at most every 500 ms, NOT per frame:
    // a synchronous /tmp stat on the main-thread frame apply is a present-path stall
    // source of exactly the hard-to-attribute kind (same class as libkrun 0113; the
    // worker's fence-present toggle got the same treatment).
    let marker_poll_at: Cell<std::time::Instant> = Cell::new(std::time::Instant::now());
    let copy_marker = Cell::new(std::fs::metadata("/tmp/limina-present-copy").is_ok());
    let lock_marker = Cell::new(std::fs::metadata("/tmp/limina-present-lock").is_ok());
    let copy_ring: RefCell<Vec<CFRetained<IOSurfaceRef>>> = RefCell::new(Vec::new());
    let copy_geom = Cell::new((0u32, 0u32));
    let copy_idx = Cell::new(0usize);
    // Cache looked-up surfaces by id (the worker reuses a small fixed set, its double buffer).
    let cache: RefCell<std::collections::HashMap<u32, CFRetained<IOSurfaceRef>>> =
        RefCell::new(std::collections::HashMap::new());
    // The surface last handed to Core Animation (copy-ring or guest, whichever actually became
    // layer contents). Each frame's shown-ack carries the surface it REPLACED so the ack sender
    // can hold the ack until WindowServer stops sampling it (#24 off-glass gating; see the ack
    // thread above).
    let last_ca: RefCell<Option<CFRetained<IOSurfaceRef>>> = RefCell::new(None);
    // Guest-cursor per-timer state: the last applied cursor gen and the (IOSurface id,
    // content scale) of the shape the host pointer currently wears (so we rebuild only on
    // an actual shape or window-scale change).
    let last_cursor_gen = Cell::new(0u64);
    let built_cursor: Cell<Option<(u32, u32)>> = Cell::new(None);
    // The host pointer's guest-shape adoption, shared with the input monitor (which
    // tracks the pointer crossing the view boundary and asserts/clears the shape).
    let host_cursor = input::HostCursor::new();
    // Pointer-capture flag, shared between the input monitor (which toggles it on Cmd-Ctrl-G),
    // the render timer (which composites the guest cursor while it's set), and the capture tap.
    let captured = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // The captured-mode virtual cursor (view points): seeded by uncaptured motion with the
    // pointer's last position over the content, stepped by the macOS-accelerated deltas while
    // captured, and driven through the SAME absolute-tablet mapping as uncaptured motion — so
    // captured movement feels exactly like the host cursor. Shared between the input translator
    // and the capture tap (both main thread).
    let capture_pos: std::rc::Rc<Cell<Option<(f64, f64)>>> = std::rc::Rc::new(Cell::new(None));
    // The `notch = extend` overlay (see [`ExtendOverlay`]), reconciled from the render tick. The
    // capture tap reads its flag: a guest hosted in the overlay is fullscreen as far as edge
    // resistance is concerned, even though the overlay carries no fullscreen style bit.
    let overlay = std::rc::Rc::new(ExtendOverlay::default());
    let overlay_flag = overlay.flag();
    // Set by the capture tap when the user deliberately shoves at the top edge: the gesture that
    // asks for the menu bar and the window's own controls back. See `ExtendOverlay::reconcile`.
    let reveal_chrome = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Reliable capture container: a session-level CGEventTap that *consumes* mouse events while
    // captured (so clicks/motion can't escape to host windows) and integrates them into the
    // virtual cursor driving the absolute tablet. Needs Accessibility permission; if absent,
    // capture falls back to the local monitor's warp path (see input.rs), the toggle path
    // retries the install (a mid-run grant heals without a restart), and the first tap-less
    // capture raises the system prompt. Installed on the main thread, before `app.run()`.
    // The input translator, created before the tap so both share it: the render timer flushes
    // held keys on focus loss, the local event monitor drives it per event, and the tap
    // delegates capture toggles + the ungrab chord to it (one source of truth for the
    // capture-boundary modifier bookkeeping).
    let input_state = std::rc::Rc::new(input::InputState::new(
        conn.clone(),
        host_cursor.clone(),
        remap,
        captured.clone(),
        fit_cell.clone(),
        capture_pos.clone(),
        overlay_flag.clone(),
        reveal_chrome.clone(),
    ));
    let _capture_tap = capture_tap::install(
        conn.clone(),
        captured.clone(),
        input_state.clone(),
        soft_kbd_grab,
        fit_cell.clone(),
        capture_pos.clone(),
        view.clone(),
        edge_resistance,
        overlay_flag,
        reveal_chrome.clone(),
    );

    // Shown-ack channel (#8 leg 2): after Core Animation latches a frame, tell the worker
    // "shown <id>" so it can complete the guest's held flush fence. The blocking send is done on
    // a DEDICATED thread, never the AppKit main thread: the ack fd shares a blocking open-file
    // description with the reader (so it can't be made non-blocking), and MSG_DONTWAIT is NOT
    // honored for AF_UNIX stream sockets on macOS — so a worker that briefly stops draining acks
    // (notably the early-boot window just after a reboot relaunch) would otherwise block the main
    // thread and beachball the whole UI. The completion block only `try_send`s the id (bounded,
    // best-effort — drop if full); this thread sends it to whichever worker is current (conn is
    // swapped on relaunch). A dropped ack is covered by the worker's fallback deadline.
    let (ack_tx, ack_rx) = std::sync::mpsc::sync_channel::<present::AckMsg>(64);
    {
        let conn = conn.clone();
        // #24 off-glass gating kill switch: LIMINA_ACK_ONGLASS=0 reverts to latch-only acks
        // (the pre-fix behavior); `touch /tmp/limina-ack-latch` does the same LIVE for
        // within-session A/B (rm re-arms). The marker is re-stat'ed at most every 500 ms —
        // never per ack (no sync I/O on the frame-pacing path; same treatment as the
        // present-copy markers and libkrun 0113).
        let onglass_env = std::env::var("LIMINA_ACK_ONGLASS").map_or(true, |v| v != "0");
        // §29/§30 ack SPLIT: one supervisor message conflated two host events — "the new
        // frame is presented" and "the replaced buffer is off glass". They differ by
        // WindowServer's over-hold tail (useprobe2: clear-of-prev p50 16.2 ms after commit
        // = the swap vblank, but p90 +7 ms / max +24 ms PAST the swap), and the guest's
        // flip fence — its present timestamp AND its buffer-release gate — rode the late
        // edge, which is the §29 "queued early, presented a cycle late" miss class.
        // Split: "shown <id>" (completes the guest flush fence) is sent at the off-glass
        // gate CAPPED at ~one refresh + slack past the latch — identical to the proven
        // tear-safe timing whenever the clear is punctual, and in tail cases the cap fires
        // post-swap (the replaced surface is off glass, merely over-held). "free <id>" is
        // sent at the actually-observed clear (informational to the worker today; the
        // release-truth signal on the wire). LIMINA_PRESENT_FENCE=free restores the
        // old single-edge behavior (fence at full clear, 50 ms cap).
        let fence_capped = std::env::var("LIMINA_PRESENT_FENCE").map_or(true, |v| v != "free");
        let cap_ms = std::env::var("LIMINA_PRESENT_FENCE_CAP_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(20);
        std::thread::spawn(move || {
            let mut marker_at = std::time::Instant::now();
            let mut latch_marker = std::fs::metadata("/tmp/limina-ack-latch").is_ok();
            let send_line = |line: String| {
                // Snapshot the current worker's endpoints and hold the Arc across the send:
                // the ack fd can't be closed (nor its number reused) mid-send even if a
                // relaunch retires this worker concurrently. Blocking is fine here — a
                // wedged/booting worker only stalls this thread.
                let io = conn.io();
                unsafe {
                    libc::send(
                        io.ack_fd(),
                        line.as_ptr() as *const libc::c_void,
                        line.len(),
                        0,
                    );
                }
            };
            while let Ok((id, prev)) = ack_rx.recv() {
                // #24: the completion block (latch) fires ~one refresh BEFORE WindowServer
                // stops sampling the surface this frame replaced (measured p50 17 ms / max
                // 33 ms, spikes/present-pacing/). Acking at latch hands the guest its
                // flip-completion while the old buffer is still being read — it repaints it
                // and the mid-repaint state reaches glass (the zero-copy tear). Hold the ack
                // until the replaced surface leaves window-server use; the caps bound a
                // stuck count (occlusion oddities), and the worker's 150 ms fallback stands
                // behind that.
                if marker_at.elapsed() >= std::time::Duration::from_millis(500) {
                    marker_at = std::time::Instant::now();
                    latch_marker = std::fs::metadata("/tmp/limina-ack-latch").is_ok();
                }
                let gate = onglass_env && !latch_marker;
                let mut freed = true;
                if gate {
                    if let Some(prev) = &prev {
                        let t0 = std::time::Instant::now();
                        let gated = prev.is_in_use();
                        let shown_cap = std::time::Duration::from_millis(if fence_capped {
                            cap_ms
                        } else {
                            50
                        });
                        while prev.is_in_use() && t0.elapsed() < shown_cap {
                            std::thread::sleep(std::time::Duration::from_micros(500));
                        }
                        freed = !prev.is_in_use();
                        if gated {
                            // Engagement oracle (the 0114 lesson): the FIRST gated ack logs
                            // at INFO — one line per run confirms the mode without any
                            // per-frame firehose; the periodic wait sample stays at trace.
                            static GATED: std::sync::atomic::AtomicU64 =
                                std::sync::atomic::AtomicU64::new(0);
                            let n = GATED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            if n == 0 {
                                log::info!(
                                    "window: off-glass ack gating ENGAGED (first gated ack waited {:?}, fence cap {:?})",
                                    t0.elapsed(),
                                    shown_cap
                                );
                            } else if n % 512 == 0 {
                                log::trace!(
                                    "window: off-glass ack gate n={n}, last wait {:?}",
                                    t0.elapsed()
                                );
                            }
                        }
                    }
                }
                send_line(format!("shown {id}\n"));
                // Release truth: keep polling the replaced surface past the fence cap (up
                // to the old 50 ms total) and report the observed clear as "free <id>".
                // The worker ignores unknown verbs today; this is the wire's release
                // signal for any future reuse-safety bookkeeping, and it keeps the
                // over-hold tail observable (trace) without moving the fence.
                if !freed {
                    if let Some(prev) = &prev {
                        let t0 = std::time::Instant::now();
                        while prev.is_in_use()
                            && t0.elapsed() < std::time::Duration::from_millis(50)
                        {
                            std::thread::sleep(std::time::Duration::from_micros(500));
                        }
                        freed = !prev.is_in_use();
                        if freed {
                            log::trace!(
                                "window: over-hold tail on frame {id}: replaced surface cleared {:?} after the fence cap",
                                t0.elapsed()
                            );
                        }
                    }
                }
                send_line(format!("free {id}\n"));
            }
        });
    }

    // The frame-apply path, shared by the 60 Hz timer (fallback/liveness) and the
    // dispatch wake-up from the reader thread (event-driven, leg 1 of the latency
    // collapse): applying the moment a frame arrives instead of at the next tick.
    let apply: std::rc::Rc<dyn Fn()> = std::rc::Rc::new({
        let shared = shared.clone();
        let window = window.clone();
        let layer = layer.clone();
        let ack_tx = ack_tx.clone();
        let surface_map = surface_map.clone();
        let fit_cell = fit_cell.clone();
        let desired_size = desired_size.clone();
        let apply_overlay = overlay.clone();
        let apply_reveal = reveal_chrome.clone();
        move || {
            // Track the scanout layer to the window every tick — INCLUDING mid live-resize
            // (the timer fires in common modes, so this runs during the drag). Dynamic mode
            // fills the window (a layer-HOSTING view doesn't auto-size its layer; CA scales
            // the current surface to the new frame, so the desktop stretches smoothly during
            // a drag and snaps crisp once the guest re-modesets). Host/fixed aspect-fit the
            // guest resolution into the view — the letterbox — on the black window
            // background; the guest never re-modesets for a window resize in those modes.
            // Bring the `extend` overlay up or down before measuring anything: it decides
            // which view the guest lives in, and therefore what the fit is computed against.
            if !window.styleMask().contains(NSWindowStyleMask::FullScreen) {
                // Out of fullscreen the chrome is there for the taking, so the ask is moot —
                // and clearing it here means entering fullscreen always starts from the
                // overlay, whatever happened in the last session of it.
                apply_reveal.store(false, std::sync::atomic::Ordering::Relaxed);
            }
            apply_overlay.reconcile(
                &window,
                cfg_notch,
                &NSApplication::sharedApplication(mtm),
                apply_reveal.load(std::sync::atomic::Ordering::Relaxed),
            );
            if let Some(v) = apply_overlay.active_view(&window) {
                let sz = v.frame().size;
                // Native fullscreen is the only state that reveals what AppKit's own housing
                // inset actually costs; record it so `avoid` can size the guest to the point.
                // Skipped while the overlay is up — it is taller than the carrier by exactly the
                // inset, so measuring there would learn zero and un-inset the `avoid` guest.
                if window.styleMask().contains(NSWindowStyleMask::FullScreen)
                    && !apply_overlay.is_active()
                {
                    if let Some(s) = window.screen() {
                        hostdisplay::learn_fullscreen_inset(&s, s.frame().size.height - sz.height);
                    }
                }
                // No housing arithmetic here any more. Under `avoid` AppKit insets the native
                // fullscreen window below the housing itself (we no longer ship the
                // compatibility plist key, which only ever bought a *frame* covering the strip
                // while the compositor masked it anyway); under `extend` the panel-fullscreen
                // window is exactly the panel and every point of it is ours. Either way the
                // content view we are handed is already the usable area, and subtracting the
                // housing again would letterbox the guest by that much a second time.
                let (sz_w, sz_h) = (sz.width, sz.height);
                if sz_w > 0.0 && sz_h > 0.0 {
                    let g = geom.get();
                    let target = if mode == DisplayResolution::Dynamic {
                        fit::FitRect::full(sz_w, sz_h)
                    } else {
                        fit::aspect_fit(g.0, g.1, sz_w, sz_h)
                    };
                    if target != fit_cell.get() {
                        // Letterboxing in FULLSCREEN means the guest's mode and the panel
                        // disagree, and the black bars alone don't say which side is wrong.
                        // Log both numbers: bars on the SIDES mean the guest is on a mode of the
                        // wrong *aspect* (it settled on a DMT entry rather than the preferred
                        // timing), bars top and bottom mean the right aspect at a stale *size*,
                        // and a short view with a matching guest means the housing strip never
                        // reached us. Diagnosing this on dogfood otherwise costs a round of ssh
                        // archaeology (2026-08-01).
                        if (window.styleMask().contains(NSWindowStyleMask::FullScreen)
                            || apply_overlay.is_active())
                            && (target.w < sz_w - 1.0 || target.h < sz_h - 1.0)
                        {
                            log::debug!(
                                "fullscreen letterbox: guest {}x{} into {:.0}x{:.0} pt usable \
                                 (view {:.0}x{:.0}, overlay {}) -> {:.0}x{:.0} \
                                 at +{:.0}+{:.0}",
                                g.0,
                                g.1,
                                sz_w,
                                sz_h,
                                sz.width,
                                sz.height,
                                apply_overlay.is_active(),
                                target.w,
                                target.h,
                                target.x,
                                target.y,
                            );
                        }
                        fit_cell.set(target);
                        set_layer_frame(&layer, target);
                    }
                }
            }

            // Resolution pushes to the guest, by display mode.
            if let Some(sock) = &resize_socket {
                // Which physical display the window sits on, and therefore the identity,
                // density and refresh the guest should see. This is deliberately computed for
                // EVERY mode: those are properties of the *host display*, and only the policy
                // that decides the guest's *resolution* differs between modes. Host mode folds
                // the identity into its size push so a migration costs one modeset; dynamic and
                // fixed have no size push to fold it into, so they send it on its own below.
                // `screen()` is None mid-transition, which simply skips the tick.
                let host = window.screen().map(|s| {
                    let inset = notch_inset_for(&s, cfg_notch);
                    hostdisplay::describe(&s, scale_for(&window, hidpi), inset)
                });
                let migrated = host
                    .as_ref()
                    .is_some_and(|h| h.identity_key() != identity_sent.get());

                match mode {
                    // Dynamic: push the window's content size ONCE the resize gesture ENDS —
                    // never during the drag. `inLiveResize()` is true for the whole drag;
                    // firing while it's true would re-modeset the guest dozens of times
                    // mid-gesture (surface churn + cache clears → the window blanks). Only
                    // active once the guest has presented a frame (so `geom` is a real
                    // baseline, not 0×0), and skipped when the window already matches the
                    // guest (the feedback guard against the guest-driven setContentSize echo).
                    DisplayResolution::Dynamic => {
                        let base = geom.get();
                        let view = window.contentView();
                        let in_live = view.as_ref().map(|v| v.inLiveResize()).unwrap_or(false);
                        let size = view
                            .map(|v| v.frame().size)
                            .unwrap_or(NSSize::new(0.0, 0.0));
                        // The window is measured in points; the guest is driven in pixels.
                        let want = scale_for(&window, hidpi).to_guest((size.width, size.height));
                        if base != (0, 0)
                            && !in_live
                            && want.0 >= 64
                            && want.1 >= 64
                            && want != base
                            && want != resize_sent.get()
                        {
                            resize_sent.set(want);
                            desired_size.store(
                                crate::session::pack_size(want.0, want.1),
                                std::sync::atomic::Ordering::Relaxed,
                            );
                            send_resize(sock, want.0, want.1);
                        }
                    }
                    // Host: drive the guest to the point size of the screen the window is
                    // on; re-push only when that changes (moved to another display, display
                    // reconfigured). Window drags never modeset the guest. Polled from the
                    // timer — the established pattern (no NSWindowDelegate), and `screen()`
                    // is None mid-transition, which simply skips the tick.
                    DisplayResolution::Host => {
                        if let (Some(screen), Some(host)) = (window.screen(), host.as_ref()) {
                            let want = host.size;
                            let key = host.identity_key();
                            if geom.get() != (0, 0)
                                && want.0 >= 64
                                && want.1 >= 64
                                && (want, key) != screen_sent.get()
                            {
                                screen_sent.set((want, key));
                                identity_sent.set(key);
                                // Migrated to a differently-shaped display: re-lock resize to
                                // the new screen's aspect so the constraint tracks the screen.
                                apply_aspect_lock(&window, want);
                                // ...and reshape the window itself to that aspect NOW. The guest
                                // is being driven to the new screen's shape (below); the window,
                                // which AppKit left at its old shape on the drag across displays,
                                // would otherwise letterbox until the user resized it. Preserve
                                // the on-screen area, clamp into the new screen's visible frame;
                                // the per-tick fit recompute re-fits the scanout next tick.
                                if let Some(v) = window.contentView() {
                                    let cur = v.frame().size;
                                    let vis = screen.visibleFrame().size;
                                    let (nw, nh) = fit::reshape_to_aspect(
                                        (cur.width, cur.height),
                                        want,
                                        (vis.width, vis.height),
                                    );
                                    window
                                        .setContentSize(NSSize::new(f64::from(nw), f64::from(nh)));
                                }
                                desired_size.store(
                                    crate::session::pack_size(want.0, want.1),
                                    std::sync::atomic::Ordering::Relaxed,
                                );
                                // On a migration, hand the guest the new display's whole
                                // identity — name, serial, refresh rate, density, VRR range —
                                // together with the size, so its compositor recognizes the
                                // monitor and applies that monitor's remembered configuration
                                // rather than treating it as the same panel resized. A plain
                                // size change (the host reconfigured this display) carries no
                                // EDID, leaving the identity exactly where it was.
                                let command = if migrated {
                                    hostdisplay::migration_command(host, true)
                                } else {
                                    DisplayCommand::Resize {
                                        width: want.0,
                                        height: want.1,
                                    }
                                };
                                send_display_command(sock, command);
                            }
                        }
                    }
                    // Fixed: the resolution is never pushed — the boot --display-size carries
                    // it, and a divergent guest (in-guest xrandr) just letterboxes differently.
                    // The display *identity* still is, below.
                    DisplayResolution::Fixed(..) => {}
                }

                // Identity-only push, for the modes whose size policy has nothing to fold it
                // into. Without this, a dynamic or fixed VM keeps the anonymous boot identity
                // and a flat 300 DPI on every display it is ever dragged to — so an ordinary
                // external monitor reads as Retina to the guest and it picks the wrong scale.
                // Gated on the guest having presented a frame, like every other push here.
                if migrated && !matches!(mode, DisplayResolution::Host) && geom.get() != (0, 0) {
                    if let Some(host) = host.as_ref() {
                        identity_sent.set(host.identity_key());
                        send_display_command(sock, hostdisplay::migration_command(host, false));
                    }
                }
            }

            let (gen, show_id, width, height) = {
                let s = shared.lock().unwrap();
                (s.gen, s.show_id, s.width, s.height)
            };
            if gen == last_gen.get() {
                return;
            }
            last_gen.set(gen);

            let Some(id) = show_id else { return };
            if geom.get() != (width, height) {
                geom.set((width, height));
                if mode == DisplayResolution::Dynamic {
                    // Guest-follow (dynamic only): the window tracks guest modesets, as
                    // originally shipped.
                    let (pw, ph) = scale_for(&window, hidpi).to_points((width, height));
                    window.setContentSize(NSSize::new(pw, ph));
                    let full = fit::FitRect::full(width as f64, height as f64);
                    fit_cell.set(full);
                    set_layer_frame(&layer, full);
                    // Keep the relaunch size current: a reboot then boots at whatever
                    // resolution the guest last ran (e.g. an in-guest xrandr choice).
                    desired_size.store(
                        crate::session::pack_size(width, height),
                        std::sync::atomic::Ordering::Relaxed,
                    );
                } else if let Some(v) = window.contentView() {
                    // Host/fixed: the window is host-owned — a guest modeset re-fits the
                    // letterbox NOW (not next tick) so this frame presents at the right rect.
                    let sz = v.frame().size;
                    let target = fit::aspect_fit(width, height, sz.width, sz.height);
                    if target != fit_cell.get() {
                        fit_cell.set(target);
                        set_layer_frame(&layer, target);
                    }
                }
                // A mode change means the worker allocated fresh surfaces; ids from the old mode
                // are gone (and could be reused for unrelated surfaces), so drop the cache.
                cache.borrow_mut().clear();
            }

            let mut cache = cache.borrow_mut();
            // Resolve the id to a surface once and keep our own retained reference. Prefer the
            // Mach-delivered store (the capability-scoped, non-global scanouts); fall back to a
            // global `IOSurfaceLookup` for the venus zero-copy path (still global) and the legacy
            // no-receiver mode. A failed resolve (the worker freed it during a rapid remodeset
            // before we caught up) is not fatal — skip this frame rather than panic the UI.
            use std::collections::hash_map::Entry;
            let surface = match cache.entry(id) {
                Entry::Occupied(e) => e.into_mut(),
                Entry::Vacant(e) => {
                    let resolved = surface_map
                        .lock()
                        .unwrap()
                        .get(id)
                        .or_else(|| IOSurfaceLookup(id));
                    match resolved {
                        Some(s) => e.insert(s),
                        None => {
                            log::warn!("window: surface {id} unresolved; skipping frame");
                            return;
                        }
                    }
                }
            };
            // Shown-ack channel (#8 leg 2): after Core Animation processes this frame's
            // transaction, hand the id to the dedicated ack-sender thread (a bounded, non-blocking
            // try_send) so it can tell the worker "shown <id>" at the real latch boundary — the
            // blocking socket write never touches the AppKit main thread. The ack identifies the
            // frame by the GUEST's surface id even in copy mode (the worker tracks holds by the id
            // it presented); the sender thread targets whichever worker is current after a relaunch.
            // The message also carries the surface this frame replaces as layer contents (the one
            // WindowServer may still be sampling) so the sender can hold the ack until it's truly
            // off glass (#24). A same-surface re-flush carries None — there's nothing replaced to
            // wait on (and the guest is single-buffering, which pacing can't protect).
            let ack_for = |shown: &CFRetained<IOSurfaceRef>| {
                let prev = last_ca
                    .borrow_mut()
                    .replace(shown.clone())
                    .filter(|p| !std::ptr::eq::<IOSurfaceRef>(&**p, &**shown));
                Some((ack_tx.clone(), (id, prev.map(present::SendSurface::new))))
            };
            // Distinct object each frame (the worker alternates ids) → CA re-reads.
            if marker_poll_at.get().elapsed() >= std::time::Duration::from_millis(500) {
                marker_poll_at.set(std::time::Instant::now());
                copy_marker.set(std::fs::metadata("/tmp/limina-present-copy").is_ok());
                lock_marker.set(std::fs::metadata("/tmp/limina-present-lock").is_ok());
            }
            let present_copy = present_copy_env || copy_marker.get();
            if present_copy {
                if copy_geom.get() != (width, height) {
                    copy_geom.set((width, height));
                    let mut ring = copy_ring.borrow_mut();
                    ring.clear();
                    for _ in 0..3 {
                        if let Some(s) = create_local_iosurface(width, height) {
                            ring.push(s);
                        }
                    }
                }
                let ring = copy_ring.borrow();
                if ring.len() == 3 {
                    let dst = &ring[copy_idx.get() % 3];
                    copy_idx.set(copy_idx.get().wrapping_add(1));
                    copy_surface(surface, dst);
                    set_layer_surface(&layer, dst, ack_for(dst));
                } else {
                    set_layer_surface(&layer, surface, ack_for(surface));
                }
            } else {
                let present_lock = present_lock_env || lock_marker.get();
                if present_lock {
                    sync_surface(surface);
                }
                set_layer_surface(&layer, surface, ack_for(surface));
            }

            // Diagnostic capture of the presented scanout. Periodic (overwrite) so a
            // long-running headless check ends with a recent frame, not just early boot.
            applies.set(applies.get() + 1);
            if applies.get() % 120 == 0 {
                if let Some(path) = &capture_path {
                    capture_iosurface(surface, id, path);
                }
            }
            // Targeted per-id sweep — look each requested global id up fresh (no cache) and
            // dump it, so we can read the venus blob surface directly even when it isn't the
            // presented one.
            if !capture_ids.is_empty() && applies.get() % 30 == 0 {
                if let Some(base) = &capture_path {
                    for &cid in &capture_ids {
                        if let Some(s) = IOSurfaceLookup(cid) {
                            capture_iosurface(&s, cid, &format!("{base}.id{cid}.png"));
                        } else {
                            log::info!("capture: IOSurfaceLookup({cid}) -> none (not alive)");
                        }
                    }
                }
            }
        }
    });
    register_apply_hook(apply.clone());

    // Quit escalation state: set when the user closed the window / hit Ctrl-C and we asked
    // the guest agent to power off; reaching the deadline falls back to SIGKILL.
    let quit_deadline: Cell<Option<std::time::Instant>> = Cell::new(None);

    // M9.4 close-policy state: the action chosen for THIS close episode (Ask answers once; a
    // close-suspend that times out demotes itself to Shutdown here), and when the
    // close-triggered suspend was requested (bounds the wait before falling back).
    let close_choice: Cell<Option<crate::vmlib::schema::WindowCloseAction>> = Cell::new(None);
    let suspend_close_at: Cell<Option<std::time::Instant>> = Cell::new(None);

    // M9.4: intercept window closes so the close policy runs with the window still up
    // (see WindowCloseInterceptor). The Retained binding must outlive the run loop —
    // delegates are weak references (`run` never returns, so this frame suffices).
    let close_interceptor: Retained<WindowCloseInterceptor> =
        unsafe { msg_send![WindowCloseInterceptor::alloc(mtm), init] };
    window.setDelegate(Some(objc2::runtime::ProtocolObject::from_ref(
        &*close_interceptor,
    )));

    // M9.4 felt-resume overlay: present from startup when restoring (splash until the first
    // presented frame); created by the timer when a suspend request is observed (dim scrim
    // over the live frame). One at a time.
    let window_opened_at = std::time::Instant::now();
    let timer_overlay: std::rc::Rc<RefCell<Option<overlay::Overlay>>> =
        std::rc::Rc::new(RefCell::new(None));
    if let Some(splash) = &restore_splash {
        timer_overlay
            .borrow_mut()
            .replace(overlay::Overlay::restore(&layer, &view, splash));
    }

    // Clone a window handle for the input monitor's fullscreen shortcut BEFORE the timer block
    // below moves `window` in.
    let shortcut_window = window.clone();

    let timer_cursor = host_cursor.clone();
    let timer_fit = fit_cell.clone();
    let timer_conn = conn.clone();
    let timer_captured = captured.clone();
    let timer_cursor_layer = cursor_layer.clone();
    let timer_surface_map = surface_map.clone();
    // For the quit-check below: distinguish a real window CLOSE from a mere miniaturize/app-hide
    // (all three make the window not-visible, but only a close should power the guest off).
    let timer_app = app.clone();
    let timer_state_path = state_path.clone();
    // (`input_state` itself was created further up, before the capture tap, which shares it.)
    let timer_input = input_state.clone();
    // Window key-focus state carried across ticks, so the timer can detect the key→not-key edge.
    // Seeded with the current state (the window was just made key), so the first tick is a no-op.
    let was_key = Cell::new(window.isKeyWindow());
    let block = RcBlock::new(move |_timer: NonNull<NSTimer>| {
        let (exited, worker_suspended, show_id, frames) = {
            let s = shared.lock().unwrap();
            (s.worker_exited, s.worker_suspended, s.show_id, s.frames)
        };

        // Worker gone (guest powered off, orderly or not): net any process-group
        // stragglers and exit. (`conn.pid()` is the *current* worker — relaunch keeps it fresh.)
        if exited {
            // A suspend teardown first saves the last-presented frame as the next restore's
            // splash (M9.4 felt-resume) — the IOSurface outlives the dead worker in our
            // mapping, so the grab still reads the final content.
            if worker_suspended {
                if let (Some(path), Some(id)) = (splash_save_path.as_deref(), show_id) {
                    let resolved = timer_surface_map
                        .lock()
                        .unwrap()
                        .get(id)
                        .or_else(|| IOSurfaceLookup(id));
                    match resolved {
                        Some(surface) => {
                            diag::capture_iosurface(&surface, id, &path.to_string_lossy())
                        }
                        None => log::warn!("splash save: surface {id} unresolved; skipping"),
                    }
                }
            }
            save_state_final(timer_state_path.as_deref(), &window);
            kill_worker_group(timer_conn.pid());
            crate::gateway::cleanup();
            crate::control::cleanup();
            std::process::exit(0);
        }

        // M9.4 overlay lifecycle: the restore splash comes down on the first presented frame;
        // the suspend dim appears when a suspend request is observed (close-triggered or
        // `limina suspend`) and comes down if the bracket is abandoned (timeout → the VM keeps
        // running). While up, re-fit to the content view every tick.
        {
            let mut ov = timer_overlay.borrow_mut();
            let suspending = crate::supervisor::suspend_requested();
            let take_down = match ov.as_ref() {
                Some(o) if o.until_first_frame => frames > 0,
                Some(_) => !suspending,
                None => false,
            };
            if take_down {
                if let Some(o) = ov.take() {
                    // A suspend flavor coming down = the bracket was ABANDONED (timeout;
                    // the VM keeps running). Forget the close episode too, so a later close
                    // starts a fresh suspend instead of resuming a spent one.
                    if !o.until_first_frame {
                        close_choice.set(None);
                        suspend_close_at.set(None);
                        CLOSE_REQUESTED.with(|c| c.set(false));
                    } else {
                        // The felt-resume endpoint (perf oracle): splash up → first
                        // presented guest frame.
                        log::info!(
                            "restore: first frame presented {:.1}s after the window opened",
                            window_opened_at.elapsed().as_secs_f32()
                        );
                    }
                    o.remove();
                }
            } else if let Some(o) = ov.as_ref() {
                if let Some(content) = window.contentView() {
                    o.fit(&content);
                }
            } else if suspending {
                if let Some(content) = window.contentView() {
                    if let Some(host_layer) = content.layer() {
                        ov.replace(overlay::Overlay::suspend(&host_layer, &content));
                    }
                }
            }
        }

        // Release keys held when the window loses key focus (e.g. the user hit Cmd-Tab): the local
        // event monitor stops delivering events the instant focus leaves, so the key-up — notably
        // the Command release — never arrives and the key would stick "down" in the guest. Polled
        // here on the key→not-key edge rather than via an NSWindowDelegate (the window's deliberate
        // no-delegate pattern); the timer keeps firing while the app is backgrounded, so it catches
        // the app-switch case too.
        let is_key = window.isKeyWindow();
        if was_key.get() && !is_key {
            timer_input.release_all_held();
        }
        was_key.set(is_key);

        // Remember the window placement: once the frame settles (~half a second stable,
        // not mid live-resize — the snapshot itself skips fullscreen), persist it on a
        // throwaway thread (the send_resize pattern; the write is atomic, so a torn run
        // at worst loses the last save). state.toml is disposable — best-effort throughout.
        if let Some(path) = &timer_state_path {
            let in_live = window
                .contentView()
                .map(|v| v.inLiveResize())
                .unwrap_or(false);
            if !in_live {
                if let Some(snap) = window_state_snapshot(&window) {
                    if pending_state.get() != Some(snap) {
                        pending_state.set(Some(snap));
                        stable_ticks.set(0);
                    } else if stable_ticks.get() < 30 {
                        stable_ticks.set(stable_ticks.get() + 1);
                        if stable_ticks.get() == 30 && saved_state.get() != Some(snap) {
                            saved_state.set(Some(snap));
                            let path = path.clone();
                            std::thread::spawn(move || {
                                // Merge (set_window), never whole-save — see save_state_final.
                                if let Err(e) = crate::vmlib::state::set_window(&path, Some(snap)) {
                                    log::warn!("window state save failed: {e}");
                                }
                            });
                        }
                    }
                }
            }
        }

        // The user closed the window (intercepted — the window is still visible) or hit
        // Ctrl-C / `limina stop`. Stop requests always power off; a window CLOSE consults
        // the `[display] on_window_close` policy (M9.4): suspend (default — the dim +
        // spinner run in place, then the process exits), shutdown (the power-off ladder,
        // window closed programmatically), or ask. A minimized window or a hidden app is
        // NOT a close (visibility is only a backstop signal here — the interceptor flag is
        // the real close trigger).
        if should_initiate_quit(
            crate::supervisor::stop_requested(),
            window.isVisible(),
            window.isMiniaturized(),
            timer_app.isHidden(),
        ) || CLOSE_REQUESTED.with(|c| c.get())
        {
            use crate::vmlib::schema::WindowCloseAction;
            let action = if crate::supervisor::stop_requested() {
                Some(WindowCloseAction::Shutdown)
            } else if let Some(a) = close_choice.get() {
                Some(a)
            } else {
                let picked = match on_window_close {
                    WindowCloseAction::Ask => ask_close_action(mtm, &title),
                    other => Some(other),
                };
                match picked {
                    Some(a) => {
                        close_choice.set(Some(a));
                        Some(a)
                    }
                    None => {
                        // Cancel: the close was intercepted, the window never went away —
                        // just forget it was asked.
                        CLOSE_REQUESTED.with(|c| c.set(false));
                        None
                    }
                }
            };
            match action {
                None => {}
                Some(WindowCloseAction::Suspend) => {
                    if suspend_close_at.get().is_none() {
                        log::info!("window close → suspending the VM (on_window_close)");
                        crate::supervisor::request_suspend();
                        suspend_close_at.set(Some(std::time::Instant::now()));
                        // The window stays up (close intercepted): the overlay block above
                        // shows the dim + spinner for the save's duration. Success exits
                        // via the `exited` branch; a bracket timeout clears the request
                        // and the overlay block resets this close episode.
                    }
                }
                // Ask cannot reach here (resolved above); route it like Shutdown for safety.
                Some(WindowCloseAction::Shutdown | WindowCloseAction::Ask) => {
                    // A close-triggered shutdown hides the window now, matching the native
                    // close feel (the intercept kept it up; `close()` bypasses the
                    // delegate). Stop-triggered shutdowns keep it visible while the guest
                    // powers off — the pre-existing Ctrl-C behavior.
                    if CLOSE_REQUESTED.with(|c| c.get()) && window.isVisible() {
                        window.close();
                    }
                    // A SECOND stop signal (limina stop --force, impatient double Ctrl-C)
                    // skips whatever grace remains — mirror of the headless monitor ladder.
                    let force_now = crate::supervisor::force_stop_requested()
                        || match quit_deadline.get() {
                            None => {
                                let orderly = control
                                    .as_ref()
                                    .map(|c| c.request_shutdown(crate::control::AGENT_GRACE))
                                    .unwrap_or(false);
                                if orderly {
                                    log::info!(
                                        "window closed → asked the guest agent to power off"
                                    );
                                    quit_deadline.set(Some(
                                        std::time::Instant::now() + crate::control::AGENT_GRACE,
                                    ));
                                    false
                                } else {
                                    true
                                }
                            }
                            Some(d) => std::time::Instant::now() >= d,
                        };
                    if force_now {
                        save_state_final(timer_state_path.as_deref(), &window);
                        kill_worker_group(timer_conn.pid());
                        crate::gateway::cleanup();
                        crate::control::cleanup();
                        std::process::exit(0);
                    }
                }
            }
        }

        // Guest cursor shape first — it has its own gen so a shape change (or hide)
        // applies even when the scanout hasn't produced a new frame. The shape is built at
        // the window's content scale (the fit rect over the guest resolution), so a resize
        // that rescales the desktop rescales the pointer with it.
        let (cur, guest_w) = {
            let s = shared.lock().unwrap();
            (
                (
                    s.cursor_gen,
                    s.cursor_visible,
                    s.cursor_id,
                    s.cursor_w,
                    s.cursor_h,
                    s.hot_x,
                    s.hot_y,
                ),
                s.width,
            )
        };
        let scale_key = cursor::cursor_scale_key(timer_fit.get().w, guest_w);
        let scale_moved = built_cursor.get().is_some_and(|(_, k)| k != scale_key);
        if cur.0 != last_cursor_gen.get() || scale_moved {
            last_cursor_gen.set(cur.0);
            apply_cursor(&timer_cursor, &built_cursor, &cur, &surface_map, scale_key);
        }
        // Pointer-capture cursor: while captured, composite the guest cursor at its reported
        // position (the host NSCursor is hidden then). Position moves every frame, so unlike the
        // shape this runs every tick, not gated on `cursor_gen`.
        update_capture_cursor(
            &timer_cursor_layer,
            &timer_captured,
            &shared,
            &timer_surface_map,
        );

        // Frame apply: normally event-driven (dispatch from the reader thread); this is
        // the fallback so a lost wake-up costs one tick, not a stuck frame.
        apply();
    });

    // ~60 Hz poll. Keep the timer alive for the app's lifetime. Schedule it in COMMON modes (not
    // just the default mode): `scheduledTimer...` adds it to NSDefaultRunLoopMode only, so it
    // FREEZES during a live window resize (the run loop runs in NSEventTrackingRunLoopMode while
    // the user drags). A frozen present timer leaves the layer stale → the window goes black mid/
    // post-drag and only recovers on the next forced repaint. Common modes keeps it firing through
    // the drag, so the present + resize-detection stay live the whole time.
    let timer = unsafe { NSTimer::timerWithTimeInterval_repeats_block(1.0 / 60.0, true, &block) };
    unsafe {
        NSRunLoop::currentRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes);
    }
    let _timer = timer;

    // Capture keyboard + mouse via a local event monitor and forward them to the worker as evdev
    // events. Swallowed key events return null; pass-through events return themselves. The
    // translator (`input_state`, an Rc) was created above so the render timer shares it (to flush
    // held keys on focus loss); the monitor moves its handle in and drives it per event.
    let monitor_view = view.clone();
    let input_block = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
        // SAFETY: the monitor hands us a valid, live event for the call's duration.
        let ev = unsafe { event.as_ref() };
        // Host shortcuts are intercepted BEFORE the guest sees them. We match on key-down; the
        // orphan key-up that leaks to the guest is dropped by the guest input core (a release of
        // an un-pressed key). Modifier flagsChanged still flow through, which is fine.
        if ev.r#type() == NSEventType::KeyDown {
            if let Some(sc) = input::match_host_shortcut(ev.keyCode(), ev.modifierFlags().0 as u64)
            {
                match sc {
                    input::HostShortcut::ToggleFullScreen => {
                        // One mechanism for both policies now: `extend` is delivered by an
                        // overlay floating over this very Space (see `ExtendOverlay`), not by a
                        // different kind of fullscreen. That also retires the wart where the
                        // green title-bar button and this shortcut did different things.
                        shortcut_window.toggleFullScreen(None);
                    }
                    input::HostShortcut::ToggleCapture => {
                        // An installed tap consumes this combo itself, so reaching the local
                        // monitor means the tap is MISSING (no Accessibility at startup). Retry —
                        // a grant given since then takes effect on a fresh create — and if it's
                        // still missing, raise the system prompt (once per run) rather than
                        // degrading silently to the warp path.
                        if capture_tap::retry_install() {
                            // Tap present (or just healed by the retry) — capture normally.
                            input_state.toggle_capture(&monitor_view);
                        } else if !capture_tap::prompt_accessibility_once() {
                            // The prompt was already shown earlier and Accessibility is still
                            // ungranted: honor the toggle in degraded (leaky warp) mode. On the
                            // FIRST tap-less toggle prompt_accessibility_once() returns true and we
                            // deliberately do NOT grab — the just-opened Accessibility dialog needs
                            // a clickable cursor, and a captured pointer is parked/consumed. The
                            // user grants, then presses Cmd-Ctrl-G again to actually capture.
                            input_state.toggle_capture(&monitor_view);
                        }
                    }
                }
                return std::ptr::null_mut(); // swallow — don't forward to the guest
            }
        }
        let swallow = input_state.handle(ev, &monitor_view);
        if swallow {
            std::ptr::null_mut()
        } else {
            event.as_ptr()
        }
    });
    let input_mask = NSEventMask::KeyDown
        | NSEventMask::KeyUp
        | NSEventMask::FlagsChanged
        | NSEventMask::MouseMoved
        | NSEventMask::LeftMouseDown
        | NSEventMask::LeftMouseUp
        | NSEventMask::LeftMouseDragged
        | NSEventMask::RightMouseDown
        | NSEventMask::RightMouseUp
        | NSEventMask::RightMouseDragged
        | NSEventMask::OtherMouseDown
        | NSEventMask::OtherMouseUp
        | NSEventMask::OtherMouseDragged
        | NSEventMask::ScrollWheel;
    // Keep the monitor alive for the app's lifetime (dropping it removes the monitor).
    let _monitor =
        unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(input_mask, &input_block) };

    app.run();
    // The run loop only returns if AppKit tears down unexpectedly; the timer is what
    // normally exits us. Either way, don't fall through.
    crate::gateway::cleanup();
    crate::control::cleanup();
    std::process::exit(0);
}
