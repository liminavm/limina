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

use std::ptr::NonNull;

use block2::RcBlock;
use objc2::runtime::ProtocolObject;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSMenu, NSMenuItem, NSWindow,
    NSWindowStyleMask,
};
use objc2_foundation::{
    NSPoint, NSRect, NSRunLoop, NSRunLoopCommonModes, NSSize, NSString, NSTimer,
};

use controller::CenterController;

/// Open the control center and run the AppKit loop. Never returns; quitting the
/// center exits the process (running VM children are NOT ours — they survive).
pub fn run() -> ! {
    let mtm = MainThreadMarker::new().expect("the control center must run on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);

    // A minimal main menu so Cmd-Q works in plist-less dev runs (`cargo run`).
    let menubar = NSMenu::new(mtm);
    let app_item = NSMenuItem::new(mtm);
    menubar.addItem(&app_item);
    let app_menu = NSMenu::new(mtm);
    let quit = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str("Quit limina"),
            Some(objc2::sel!(terminate:)),
            &NSString::from_str("q"),
        )
    };
    app_menu.addItem(&quit);
    app_item.setSubmenu(Some(&app_menu));
    app.setMainMenu(Some(&menubar));

    let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(560.0, 400.0));
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
    window.setTitle(&NSString::from_str("limina — Virtual Machines"));
    window.center();

    let controller = CenterController::new(mtm, &window);
    app.setDelegate(Some(ProtocolObject::from_ref(&*controller)));
    controller.refresh(true);
    window.makeKeyAndOrderFront(None);

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
