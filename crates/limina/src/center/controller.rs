// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The control-center's one ObjC class: app delegate + row-button action target.
//!
//! The visible list is a vertical `NSStackView` of row views (status dot, name,
//! summary, per-row buttons) rebuilt from a [`model::snapshot`] only when it
//! changes. Buttons carry the row index in their `tag`. Long-running operations
//! (reset, import copies) run on background threads; they report errors into a
//! shared queue the next refresh drains into an alert, and mark their bundle
//! "busy" so the row shows a disabled placeholder meanwhile.

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use objc2::rc::Retained;
use objc2::runtime::Sel;
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSApplication, NSApplicationDelegate,
    NSAutoresizingMaskOptions, NSButton, NSColor, NSFont, NSLayoutAttribute, NSModalResponseOK,
    NSOpenPanel, NSStackView, NSStackViewGravity, NSTextField, NSUserInterfaceLayoutOrientation,
    NSView, NSWindow,
};
use objc2_foundation::{
    NSArray, NSEdgeInsets, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString,
};

use super::{model, model::VmRow, spawn};
use crate::vmlib;

pub struct CenterIvars {
    /// The last-applied snapshot (row order = view order = button tags).
    rows: RefCell<Vec<VmRow>>,
    /// The row list (rebuilt in place). Set once by `build_ui`.
    list: RefCell<Option<Retained<NSStackView>>>,
    /// "No virtual machines yet" placeholder, hidden while rows exist.
    empty_label: RefCell<Option<Retained<NSTextField>>>,
    /// Bundles with an in-flight background operation (reset/import): their rows
    /// show a disabled "Working…" placeholder instead of buttons.
    busy: Arc<Mutex<HashSet<PathBuf>>>,
    /// Errors from background threads, drained into an alert on the next refresh.
    errors: Arc<Mutex<Vec<String>>>,
    /// The busy set as of the last rebuild, so a busy-flip alone triggers one.
    last_busy: RefCell<HashSet<PathBuf>>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; CenterController does not
    // implement Drop; all protocol method signatures below match AppKit's.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "LiminaCenterController"]
    #[ivars = CenterIvars]
    pub struct CenterController;

    unsafe impl NSObjectProtocol for CenterController {}

    unsafe impl NSApplicationDelegate for CenterController {
        // The center is a launcher: closing its window quits the center process.
        // Running VMs are independent children and keep running.
        #[unsafe(method(applicationShouldTerminateAfterLastWindowClosed:))]
        fn should_terminate_after_last_window_closed(&self, _app: &NSApplication) -> bool {
            true
        }
    }

    impl CenterController {
        #[unsafe(method(startClicked:))]
        fn start_clicked(&self, sender: &NSButton) {
            if let Some(row) = self.row_for(sender) {
                if let Err(e) = spawn::start_vm(&row.bundle) {
                    self.alert("Could not start the VM", &format!("{e:#}"));
                }
                self.refresh(true);
            }
        }

        #[unsafe(method(stopClicked:))]
        fn stop_clicked(&self, sender: &NSButton) {
            if let Some(row) = self.row_for(sender) {
                if let Err(e) = spawn::stop_vm(&row.bundle, false) {
                    self.alert("Could not stop the VM", &format!("{e:#}"));
                }
                // The flock releases when the supervisor exits; the 1 s refresh
                // flips the row on its own.
            }
        }

        #[unsafe(method(resetClicked:))]
        fn reset_clicked(&self, sender: &NSButton) {
            let Some(row) = self.row_for(sender) else { return };
            let ivars = self.ivars();
            ivars.busy.lock().unwrap().insert(row.bundle.path.clone());
            let busy = ivars.busy.clone();
            let errors = ivars.errors.clone();
            let bundle = row.bundle.clone();
            // Blocking (stop + wait for the flock + start) — never on the main thread.
            std::thread::spawn(move || {
                if let Err(e) = spawn::reset_vm(&bundle) {
                    errors
                        .lock()
                        .unwrap()
                        .push(format!("Resetting {}: {e:#}", bundle.dir_name()));
                }
                busy.lock().unwrap().remove(&bundle.path);
            });
            self.refresh(true);
        }

        #[unsafe(method(configureClicked:))]
        fn configure_clicked(&self, sender: &NSButton) {
            if let Some(row) = self.row_for(sender) {
                self.run_configure_sheet(&row);
                self.refresh(true);
            }
        }

        #[unsafe(method(deleteClicked:))]
        fn delete_clicked(&self, sender: &NSButton) {
            let Some(row) = self.row_for(sender) else { return };
            let mtm = self.mtm();
            let alert = NSAlert::new(mtm);
            alert.setMessageText(&NSString::from_str(&format!("Delete “{}”?", row.name)));
            alert.setInformativeText(&NSString::from_str(
                "This deletes the VM bundle including the disks inside it. \
                 Disk images outside the bundle are not touched.",
            ));
            alert.addButtonWithTitle(&NSString::from_str("Delete"));
            alert.addButtonWithTitle(&NSString::from_str("Cancel"));
            if alert.runModal() == NSAlertFirstButtonReturn {
                if let Err(e) = spawn::delete_vm(&row.bundle) {
                    self.alert("Could not delete the VM", &format!("{e:#}"));
                }
                self.refresh(true);
            }
        }

        #[unsafe(method(importClicked:))]
        fn import_clicked(&self, _sender: &NSButton) {
            self.run_import_flow();
        }
    }
);

impl CenterController {
    pub fn new(mtm: MainThreadMarker, window: &NSWindow) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(CenterIvars {
            rows: RefCell::new(Vec::new()),
            list: RefCell::new(None),
            empty_label: RefCell::new(None),
            busy: Arc::new(Mutex::new(HashSet::new())),
            errors: Arc::new(Mutex::new(Vec::new())),
            last_busy: RefCell::new(HashSet::new()),
        });
        // SAFETY: NSObject's init signature.
        let this: Retained<Self> = unsafe { msg_send![super(this), init] };
        this.build_ui(mtm, window);
        this
    }

    /// Build the static chrome: root stack (header with Import…, the row list, the
    /// empty-state label) filling the window's content view.
    fn build_ui(&self, mtm: MainThreadMarker, window: &NSWindow) {
        let content = window.contentView().expect("window content view");

        let root = NSStackView::stackViewWithViews(&NSArray::new(), mtm);
        root.setOrientation(NSUserInterfaceLayoutOrientation::Vertical);
        // Width-alignment stretches every arranged view to the stack's width, so
        // the header's and rows' Trailing-gravity buttons hug the right edge.
        root.setAlignment(NSLayoutAttribute::Width);
        root.setSpacing(10.0);
        root.setEdgeInsets(NSEdgeInsets {
            top: 14.0,
            left: 16.0,
            bottom: 14.0,
            right: 16.0,
        });
        root.setFrame(content.bounds());
        root.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );

        let header = NSStackView::stackViewWithViews(&NSArray::new(), mtm);
        header.setOrientation(NSUserInterfaceLayoutOrientation::Horizontal);
        header.setAlignment(NSLayoutAttribute::CenterY);
        let title = NSTextField::labelWithString(&NSString::from_str("Virtual Machines"), mtm);
        title.setFont(Some(&NSFont::boldSystemFontOfSize(16.0)));
        header.addView_inGravity(&title, NSStackViewGravity::Leading);
        let import = unsafe {
            NSButton::buttonWithTitle_target_action(
                &NSString::from_str("Import…"),
                Some(self.as_ref()),
                Some(sel!(importClicked:)),
                mtm,
            )
        };
        header.addView_inGravity(&import, NSStackViewGravity::Trailing);
        root.addView_inGravity(&header, NSStackViewGravity::Top);

        let list = NSStackView::stackViewWithViews(&NSArray::new(), mtm);
        list.setOrientation(NSUserInterfaceLayoutOrientation::Vertical);
        list.setAlignment(NSLayoutAttribute::Width);
        list.setSpacing(6.0);
        root.addView_inGravity(&list, NSStackViewGravity::Top);

        let empty = NSTextField::labelWithString(
            &NSString::from_str(
                "No virtual machines yet.\nClick Import… to create one from a disk image.",
            ),
            mtm,
        );
        empty.setTextColor(Some(&NSColor::secondaryLabelColor()));
        root.addView_inGravity(&empty, NSStackViewGravity::Top);

        content.addSubview(&root);
        *self.ivars().list.borrow_mut() = Some(list);
        *self.ivars().empty_label.borrow_mut() = Some(empty);
    }

    /// Refresh the model and rebuild the rows if anything changed (or `force`).
    /// Also drains background-thread errors into an alert. Called by the 1 s timer
    /// and after every user action.
    pub fn refresh(&self, force: bool) {
        let drained: Vec<String> = std::mem::take(&mut *self.ivars().errors.lock().unwrap());
        if !drained.is_empty() {
            self.alert("Background operation failed", &drained.join("\n\n"));
        }

        let snap = model::snapshot();
        let busy_now = self.ivars().busy.lock().unwrap().clone();
        let changed = force
            || snap != *self.ivars().rows.borrow()
            || busy_now != *self.ivars().last_busy.borrow();
        if !changed {
            return;
        }
        self.rebuild_rows(&snap, &busy_now);
        *self.ivars().rows.borrow_mut() = snap;
        *self.ivars().last_busy.borrow_mut() = busy_now;
    }

    fn rebuild_rows(&self, rows: &[VmRow], busy: &HashSet<PathBuf>) {
        let mtm = self.mtm();
        let list_ref = self.ivars().list.borrow();
        let Some(list) = list_ref.as_ref() else {
            return;
        };

        for v in list.arrangedSubviews().iter() {
            list.removeArrangedSubview(&v);
            v.removeFromSuperview();
        }
        for (i, row) in rows.iter().enumerate() {
            let view = self.row_view(mtm, i, row, busy.contains(&row.bundle.path));
            list.addView_inGravity(&view, NSStackViewGravity::Top);
        }
        if let Some(empty) = self.ivars().empty_label.borrow().as_ref() {
            empty.setHidden(!rows.is_empty());
        }
    }

    /// One VM row: `● Name / summary …… [buttons]`.
    fn row_view(
        &self,
        mtm: MainThreadMarker,
        index: usize,
        row: &VmRow,
        busy: bool,
    ) -> Retained<NSView> {
        let outer = NSStackView::stackViewWithViews(&NSArray::new(), mtm);
        outer.setOrientation(NSUserInterfaceLayoutOrientation::Horizontal);
        outer.setAlignment(NSLayoutAttribute::CenterY);
        outer.setSpacing(8.0);

        let dot = NSTextField::labelWithString(&NSString::from_str("●"), mtm);
        let dot_color = if row.broken {
            NSColor::systemRedColor()
        } else if row.running {
            NSColor::systemGreenColor()
        } else {
            NSColor::tertiaryLabelColor()
        };
        dot.setTextColor(Some(&dot_color));
        outer.addView_inGravity(&dot, NSStackViewGravity::Leading);

        let info = NSStackView::stackViewWithViews(&NSArray::new(), mtm);
        info.setOrientation(NSUserInterfaceLayoutOrientation::Vertical);
        info.setAlignment(NSLayoutAttribute::Leading);
        info.setSpacing(2.0);
        let name = NSTextField::labelWithString(&NSString::from_str(&row.name), mtm);
        name.setFont(Some(&NSFont::boldSystemFontOfSize(13.0)));
        info.addView_inGravity(&name, NSStackViewGravity::Top);
        let mut detail = row.summary.clone();
        if row.running {
            detail.push_str(&format!(" · running (pid {})", row.pid));
        }
        let summary = NSTextField::labelWithString(&NSString::from_str(&detail), mtm);
        summary.setFont(Some(&NSFont::systemFontOfSize(11.0)));
        summary.setTextColor(Some(&NSColor::secondaryLabelColor()));
        info.addView_inGravity(&summary, NSStackViewGravity::Top);
        outer.addView_inGravity(&info, NSStackViewGravity::Leading);

        if busy {
            let working = NSTextField::labelWithString(&NSString::from_str("Working…"), mtm);
            working.setTextColor(Some(&NSColor::secondaryLabelColor()));
            outer.addView_inGravity(&working, NSStackViewGravity::Trailing);
            return Retained::into_super(outer);
        }

        // Per-row actions: what the state allows, nothing else.
        let mut buttons: Vec<(&str, Sel)> = Vec::new();
        if row.broken {
            buttons.push(("Delete", sel!(deleteClicked:)));
        } else if row.running {
            buttons.push(("Stop", sel!(stopClicked:)));
            buttons.push(("Reset", sel!(resetClicked:)));
        } else {
            buttons.push(("Start", sel!(startClicked:)));
            buttons.push(("Configure…", sel!(configureClicked:)));
            buttons.push(("Delete", sel!(deleteClicked:)));
        }
        for (title, action) in buttons {
            let b = unsafe {
                NSButton::buttonWithTitle_target_action(
                    &NSString::from_str(title),
                    Some(self.as_ref()),
                    Some(action),
                    mtm,
                )
            };
            b.setTag(index as isize);
            outer.addView_inGravity(&b, NSStackViewGravity::Trailing);
        }

        Retained::into_super(outer)
    }

    /// The row a clicked button belongs to (its `tag` indexes the applied snapshot).
    fn row_for(&self, sender: &NSButton) -> Option<VmRow> {
        let idx = usize::try_from(sender.tag()).ok()?;
        self.ivars().rows.borrow().get(idx).cloned()
    }

    fn alert(&self, title: &str, text: &str) {
        let alert = NSAlert::new(self.mtm());
        alert.setMessageText(&NSString::from_str(title));
        alert.setInformativeText(&NSString::from_str(text));
        alert.runModal();
    }

    /// Import…: pick a disk image, name the VM, clone it into a new bundle in the
    /// library. The clone runs off the main thread (instant on-volume via APFS
    /// clonefile, but a cross-volume copy of a big image takes a while).
    fn run_import_flow(&self) {
        let mtm = self.mtm();
        let panel = NSOpenPanel::openPanel(mtm);
        panel.setCanChooseFiles(true);
        panel.setCanChooseDirectories(false);
        panel.setAllowsMultipleSelection(false);
        if panel.runModal() != NSModalResponseOK {
            return;
        }
        let Some(disk) = panel
            .URL()
            .and_then(|u| u.path())
            .map(|p| PathBuf::from(p.to_string()))
        else {
            return;
        };
        let default_name = disk
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("New VM")
            .to_string();
        let Some(name) = self.prompt_text(
            "Import Disk Image",
            "Name for the new virtual machine:",
            &default_name,
        ) else {
            return;
        };
        let name = name.trim().to_string();
        if name.is_empty() {
            self.alert("Import failed", "The VM needs a non-empty name.");
            return;
        }

        let errors = self.ivars().errors.clone();
        std::thread::spawn(move || {
            let run = || -> anyhow::Result<()> {
                let dest = vmlib::bundle::library_dir();
                std::fs::create_dir_all(&dest)?;
                vmlib::import::create(
                    &vmlib::import::CreateOpts {
                        name: name.clone(),
                        disk: Some(disk.clone()),
                        import_mode: vmlib::import::ImportMode::CloneIntoBundle,
                        cpus: 4,
                        memory: vmlib::schema::Memory::default(),
                        ssh_port: 0,
                        window: true,
                    },
                    &dest,
                )?;
                Ok(())
            };
            if let Err(e) = run() {
                errors
                    .lock()
                    .unwrap()
                    .push(format!("Importing {}: {e:#}", disk.display()));
            }
        });
    }

    /// Configure…: vCPUs / memory / SSH port in a modal alert. Only reachable for
    /// stopped VMs (the row offers no Configure while running).
    fn run_configure_sheet(&self, row: &VmRow) {
        let mtm = self.mtm();
        let mut cfg = match row.bundle.load() {
            Ok(c) => c,
            Err(e) => {
                self.alert("Cannot configure", &format!("{e:#}"));
                return;
            }
        };

        let mem_now = match &cfg.hardware.memory {
            vmlib::schema::Memory::Fixed(s) => s.clone(),
            vmlib::schema::Memory::Range { min, max } => format!("{min}..{max}"),
        };
        let ssh_now = cfg.networks.first().map(|n| n.ssh_port).unwrap_or(0);

        let alert = NSAlert::new(mtm);
        alert.setMessageText(&NSString::from_str(&format!("Configure “{}”", row.name)));
        alert.setInformativeText(&NSString::from_str(
            "Memory: a fixed size (\"4096M\", \"8G\") or a dynamic range (\"2G..8G\"). \
             SSH port 0 = pick automatically.",
        ));
        alert.addButtonWithTitle(&NSString::from_str("Save"));
        alert.addButtonWithTitle(&NSString::from_str("Cancel"));

        let accessory = NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, 300.0, 92.0));
        let cpus_field = labeled_field(
            mtm,
            &accessory,
            64.0,
            "vCPUs:",
            &cfg.hardware.cpus.to_string(),
        );
        let mem_field = labeled_field(mtm, &accessory, 34.0, "Memory:", &mem_now);
        let ssh_field = labeled_field(mtm, &accessory, 4.0, "SSH port:", &ssh_now.to_string());
        alert.setAccessoryView(Some(&accessory));

        if alert.runModal() != NSAlertFirstButtonReturn {
            return;
        }

        // Validate with the same parsers the CLI uses; reject without saving.
        let apply = || -> anyhow::Result<vmlib::schema::VmConfig> {
            let cpus: u8 = cpus_field.stringValue().to_string().trim().parse()?;
            anyhow::ensure!(cpus > 0, "vCPUs must be at least 1");
            let memory = vmlib::schema::Memory::parse(&mem_field.stringValue().to_string())?;
            let ssh_port: u16 = ssh_field.stringValue().to_string().trim().parse()?;
            cfg.hardware.cpus = cpus;
            cfg.hardware.memory = memory;
            if let Some(net) = cfg.networks.first_mut() {
                net.ssh_port = ssh_port;
            } else if ssh_port != 0 {
                cfg.networks.push(vmlib::schema::NetworkEntry {
                    mode: vmlib::schema::NetMode::Nat,
                    mac: vmlib::schema::mac_for_uuid(&cfg.identity.uuid),
                    ssh_port,
                });
            }
            Ok(cfg)
        };
        match apply() {
            Ok(cfg) => {
                if let Err(e) = row.bundle.save(&cfg) {
                    self.alert("Could not save the configuration", &format!("{e:#}"));
                }
            }
            Err(e) => self.alert("Invalid configuration", &format!("{e:#}")),
        }
    }

    /// A modal alert with a single text field; `Some(text)` on OK.
    fn prompt_text(&self, title: &str, message: &str, initial: &str) -> Option<String> {
        let mtm = self.mtm();
        let alert = NSAlert::new(mtm);
        alert.setMessageText(&NSString::from_str(title));
        alert.setInformativeText(&NSString::from_str(message));
        alert.addButtonWithTitle(&NSString::from_str("OK"));
        alert.addButtonWithTitle(&NSString::from_str("Cancel"));
        let field = NSTextField::textFieldWithString(&NSString::from_str(initial), mtm);
        field.setFrame(rect(0.0, 0.0, 240.0, 24.0));
        alert.setAccessoryView(Some(&field));
        (alert.runModal() == NSAlertFirstButtonReturn).then(|| field.stringValue().to_string())
    }
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

/// A `Label:` + editable field pair placed at `y` inside the accessory view.
fn labeled_field(
    mtm: MainThreadMarker,
    parent: &NSView,
    y: f64,
    label: &str,
    value: &str,
) -> Retained<NSTextField> {
    let l = NSTextField::labelWithString(&NSString::from_str(label), mtm);
    l.setFrame(rect(0.0, y + 3.0, 84.0, 18.0));
    parent.addSubview(&l);
    let f = NSTextField::textFieldWithString(&NSString::from_str(value), mtm);
    f.setFrame(rect(90.0, y, 200.0, 24.0));
    parent.addSubview(&f);
    f
}
