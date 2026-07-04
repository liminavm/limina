// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The VM control center — the window that opens when limina runs with no
//! arguments (double-clicking limina.app) or via `limina center`.
//!
//! Architecture: the center is a launcher, not a host. Each running VM is its own
//! child `limina start <bundle>` supervisor process (the per-process gvproxy/control
//! globals, the one-NSWindow-per-process design and the `process::exit` teardown all
//! assume one VM per process). Children are spawned detached and **survive the
//! center quitting**; state is re-derived from each bundle's `run/lock` flock +
//! pidfile, so a relaunched center — or one looking at VMs started from a terminal —
//! controls them all the same way.
//!
//! The UI is deliberately simple AppKit: a vertical stack of VM rows (status dot,
//! name, summary, per-row action buttons) rebuilt only when the model snapshot
//! changes, refreshed by a 1 s timer. No table-view data sources, no daemon.

mod controller;
mod model;
mod spawn;

use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;

use block2::RcBlock;
use objc2::runtime::ProtocolObject;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSMenu, NSMenuItem, NSWindow,
    NSWindowStyleMask,
};
use objc2_foundation::{
    NSDictionary, NSDistributedNotificationCenter, NSNumber, NSPoint, NSRect, NSRunLoop,
    NSRunLoopCommonModes, NSSize, NSString, NSTimer, NSUserDefaults,
};

use controller::CenterController;

/// Distributed-notification name a second center posts to ask the running one to
/// show its window (the single-instance "show yourself" channel).
pub const SHOW_CENTER_NOTIFICATION: &str = "eti.noronha.limina.show-center";

/// The center's single-instance flock sentinel, next to the VM library.
fn center_lock_path() -> PathBuf {
    let lib = crate::vmlib::bundle::library_dir();
    lib.parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| lib.clone())
        .join("center.lock")
}

/// Take the center's exclusive flock. `None` = another center already holds it.
fn acquire_center_lock() -> Option<std::fs::File> {
    let path = center_lock_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .ok()?;
    let r = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    (r == 0).then_some(f)
}

/// Is a center running (holding the exclusive flock)? A shared probe fails only then.
fn center_is_running() -> bool {
    let Ok(f) = std::fs::File::open(center_lock_path()) else {
        return false;
    };
    let r = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) };
    r != 0 // lock released (LOCK_UN implicit) when f drops
}

fn post_show_notification() {
    let dnc = NSDistributedNotificationCenter::defaultCenter();
    unsafe {
        dnc.postNotificationName_object_userInfo_deliverImmediately(
            &NSString::from_str(SHOW_CENTER_NOTIFICATION),
            None,
            None,
            true,
        );
    }
}

/// Bring the control center forward from anywhere (the VM window's
/// "Control Center…" menu item): ask a running center to show itself, or spawn a
/// fresh detached `limina center` when none is running.
pub fn show_or_spawn() -> anyhow::Result<()> {
    if center_is_running() {
        post_show_notification();
        return Ok(());
    }
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe()?;
    let child = std::process::Command::new(exe)
        .arg("center")
        .process_group(0)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    // Reap without blocking; the center is not our child in any meaningful sense.
    std::thread::spawn(move || {
        let mut child = child;
        let _ = child.wait();
    });
    Ok(())
}

/// Open the control center and run the AppKit loop. Never returns; quitting the
/// center exits the process (running VM children are NOT ours — they survive).
pub fn run() -> ! {
    // Single instance: hold an exclusive flock for the center's lifetime. A second
    // `limina center` (double-clicked app, a VM window's "Control Center…" item)
    // just asks the running one to show itself and exits.
    match acquire_center_lock() {
        Some(lock) => std::mem::forget(lock), // held until process exit
        None => {
            post_show_notification();
            std::process::exit(0);
        }
    }

    let mtm = MainThreadMarker::new().expect("the control center must run on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);

    // The Configure sheet's "?" hovers use tooltips; the system's ~1.5 s initial
    // delay makes them feel dead. Register (not set: transient, this app only) a
    // short delay so help appears essentially on hover.
    unsafe {
        let defaults = NSUserDefaults::standardUserDefaults();
        let delay = NSNumber::new_i32(100);
        let dict = NSDictionary::from_slices(
            &[&*NSString::from_str("NSInitialToolTipDelay")],
            &[delay.as_ref() as &objc2::runtime::AnyObject],
        );
        defaults.registerDefaults(&dict);
    }

    // A minimal main menu so Cmd-Q / Cmd-W work in plist-less dev runs (`cargo run`).
    let menubar = NSMenu::new(mtm);
    let app_item = NSMenuItem::new(mtm);
    menubar.addItem(&app_item);
    let app_menu = NSMenu::new(mtm);
    // Close hides the window; the center keeps running (Dock icon brings it back).
    let close = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str("Close Window"),
            Some(objc2::sel!(performClose:)),
            &NSString::from_str("w"),
        )
    };
    app_menu.addItem(&close);
    let quit = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str("Quit Limina"),
            Some(objc2::sel!(terminate:)),
            &NSString::from_str("q"),
        )
    };
    app_menu.addItem(&quit);
    app_item.setSubmenu(Some(&app_menu));
    app.setMainMenu(Some(&menubar));

    // Size the first-launch window to the library (≈116 pt per row + chrome) so a
    // small library doesn't open into mostly empty space; afterwards the frame
    // autosave restores whatever size/position the user chose.
    let vms = crate::vmlib::bundle::list().map(|v| v.len()).unwrap_or(0);
    let height = (90.0 + 116.0 * vms.max(1) as f64).clamp(240.0, 640.0);
    let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(560.0, height));
    let style = NSWindowStyleMask::Titled
        | NSWindowStyleMask::Closable
        | NSWindowStyleMask::Miniaturizable
        | NSWindowStyleMask::Resizable;
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            rect,
            style,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    // The window outlives every scope here (the controller retains it); created
    // outside a window controller, so opt out of release-when-closed.
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&NSString::from_str("Limina"));
    window.center();
    window.setFrameAutosaveName(&NSString::from_str("LiminaCenterWindow"));

    let controller = CenterController::new(mtm, &window);
    app.setDelegate(Some(ProtocolObject::from_ref(&*controller)));
    controller.refresh(true);
    window.makeKeyAndOrderFront(None);

    // "Show yourself" channel: a second `limina center` (or a VM window's
    // "Control Center…" menu item) posts this distributed notification instead of
    // starting another center.
    let dnc = NSDistributedNotificationCenter::defaultCenter();
    unsafe {
        dnc.addObserver_selector_name_object(
            &controller,
            objc2::sel!(showCenterRequested:),
            Some(&NSString::from_str(SHOW_CENTER_NOTIFICATION)),
            None,
        );
    }

    // 1 s status refresh (flock probes + vm.toml mtimes are cheap). Common modes so
    // status keeps updating while the user drags/resizes the window.
    let timer_controller = controller.clone();
    let block = RcBlock::new(move |_timer: NonNull<NSTimer>| {
        timer_controller.refresh(false);
    });
    let timer = unsafe { NSTimer::timerWithTimeInterval_repeats_block(1.0, true, &block) };
    unsafe {
        NSRunLoop::currentRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes);
    }

    // Activate so the window comes to the front when launched from a terminal.
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    app.run();
    std::process::exit(0);
}
