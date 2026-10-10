// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The "Debug" menu: the log filter and the levers of the running VM (traces, and the harness
//! access levers `input-inject` and `debug-port`), the same switches `limina debug` drives over
//! the debug socket (`crate::debug_ctl`).
//!
//! Every change lasts until the VM exits. The menu carries presets rather than a text field: the
//! common asks are a handful of filters, and anything finer is what the CLI is for — the last
//! row copies a ready command for this VM.

use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSMenu, NSMenuItem};
use objc2_foundation::NSString;

use limina_debug::wire::{Request, Scope};

use super::VmMenuActions;
use crate::debug_ctl;

/// The log presets, applied to both processes. `default` is whatever the VM started with.
pub(super) const LOG_PRESETS: &[(&str, &str)] = &[
    ("As Started", "default"),
    ("Limina at Info", "warn,limina=info,limina_vmm=info"),
    ("Limina at Debug", "warn,limina=debug,limina_vmm=debug"),
    (
        "Limina and Devices at Info",
        "warn,limina=info,limina_vmm=info,krun::vmm=info,krun_devices=info",
    ),
    ("Everything at Debug", "debug"),
];

/// The menu's title, which is also how the shared menu delegate recognises it.
pub(super) const TITLE: &str = "Debug";

pub(super) fn build(mtm: MainThreadMarker, actions: &VmMenuActions) -> Retained<NSMenu> {
    let menu = NSMenu::new(mtm);
    menu.setTitle(&NSString::from_str(TITLE));
    // Built on open: `limina debug` can change any of it from outside.
    menu.setDelegate(Some(objc2::runtime::ProtocolObject::from_ref(actions)));
    populate(&menu, mtm, actions);
    menu
}

fn header(mtm: MainThreadMarker, title: &str) -> Retained<NSMenuItem> {
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            None,
            &NSString::from_str(""),
        )
    };
    item.setEnabled(false);
    item
}

fn row(
    mtm: MainThreadMarker,
    actions: &VmMenuActions,
    title: &str,
    action: objc2::runtime::Sel,
    tag: isize,
    on: bool,
) -> Retained<NSMenuItem> {
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            Some(action),
            &NSString::from_str(""),
        )
    };
    item.setTag(tag);
    item.setState(if on {
        objc2_app_kit::NSControlStateValueOn
    } else {
        objc2_app_kit::NSControlStateValueOff
    });
    unsafe { item.setTarget(Some(actions)) };
    item
}

pub(super) fn populate(menu: &NSMenu, mtm: MainThreadMarker, actions: &VmMenuActions) {
    menu.removeAllItems();
    menu.addItem(&header(mtm, "Log Filter (this run)"));
    let current = limina_debug::logger::filter_spec();
    let startup = limina_debug::logger::startup_spec();
    let mut matched = false;
    for (i, (title, spec)) in LOG_PRESETS.iter().enumerate() {
        let spec = if *spec == "default" {
            startup.as_str()
        } else {
            spec
        };
        // The first preset that matches wins, so "As Started" is checked when the VM was
        // started with one of the others.
        let on = !matched && spec == current;
        matched |= on;
        menu.addItem(&row(
            mtm,
            actions,
            title,
            objc2::sel!(setDebugLogPreset:),
            i as isize,
            on,
        ));
    }
    if !matched {
        // Set from the CLI: show it, so the menu never claims a filter that is not in force.
        let custom = header(mtm, &format!("Custom: {current}"));
        custom.setState(objc2_app_kit::NSControlStateValueOn);
        menu.addItem(&custom);
    }

    // Traces first, then the harness access levers; the tag is the index into `LEVERS` either way.
    for (title, access) in [
        ("Traces (this run)", false),
        ("Harness Access (this run)", true),
    ] {
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        menu.addItem(&header(mtm, title));
        add_levers(menu, mtm, actions, access);
    }

    menu.addItem(&NSMenuItem::separatorItem(mtm));
    let copy = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str("Copy Debug Command"),
            Some(objc2::sel!(copyDebugCommand:)),
            &NSString::from_str(""),
        )
    };
    copy.setToolTip(Some(&NSString::from_str(
        "A `limina debug` command line for this VM, for filters the presets do not cover",
    )));
    unsafe { copy.setTarget(Some(actions)) };
    menu.addItem(&copy);
}

fn add_levers(menu: &NSMenu, mtm: MainThreadMarker, actions: &VmMenuActions, access: bool) {
    for (i, l) in debug_ctl::LEVERS.iter().enumerate() {
        if debug_ctl::is_access(l) != access {
            continue;
        }
        let item = row(
            mtm,
            actions,
            l.name(),
            objc2::sel!(toggleDebugLever:),
            i as isize,
            l.on(),
        );
        item.setToolTip(Some(&NSString::from_str(&format!(
            "{} ({})",
            l.about(),
            l.env()
        ))));
        menu.addItem(&item);
    }
}

/// Apply a preset to both processes. Off the main thread: the worker's half waits on its link.
pub(super) fn apply_preset(index: isize) {
    let Some((title, spec)) = usize::try_from(index).ok().and_then(|i| LOG_PRESETS.get(i)) else {
        return;
    };
    log::warn!("menu: debug log filter → {title} ({spec})");
    let req = Request::Log {
        scope: Scope::All,
        spec: (*spec).to_string(),
    };
    run_off_main(req);
}

/// Flip a lever. Levers are atomics, so this needs no thread hop.
pub(super) fn toggle_lever(index: isize) {
    let Some(l) = usize::try_from(index)
        .ok()
        .and_then(|i| debug_ctl::LEVERS.get(i))
    else {
        return;
    };
    let req = Request::Lever {
        name: l.name().to_string(),
        on: !l.on(),
    };
    if let Err(e) = debug_ctl::handle(&req) {
        log::warn!("menu: {}: {e}", req.to_line());
    }
}

/// The command line `limina debug` needs for this VM, by supervisor pid.
pub(super) fn command_line() -> String {
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "limina".into());
    format!("{exe} debug {} status", std::process::id())
}

fn run_off_main(req: Request) {
    let spawned = std::thread::Builder::new()
        .name("debug-menu".into())
        .spawn(move || match debug_ctl::handle(&req) {
            Ok(lines) => {
                for l in lines {
                    log::warn!("menu: {l}");
                }
            }
            Err(e) => log::warn!("menu: {}: {e}", req.to_line()),
        });
    if let Err(e) = spawned {
        log::warn!("menu: could not apply the debug change: {e}");
    }
}
