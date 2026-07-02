// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The control-center's one ObjC class: app delegate + row-button action target.
//!
//! The visible list is a vertical `NSStackView` of row views rebuilt from a
//! [`model::snapshot`] only when it changes. Each row is: status dot + a multi-line
//! info block (name / config+status / disks / ssh) + an action row of SF-symbol
//! icon buttons. Buttons carry the row index in their `tag`.
//!
//! Lifecycle affordances: Stop asks for the graceful ladder and the button then
//! MORPHS into a force-stop (bolt) icon — the discoverable UI for the "second
//! signal skips the grace" escalation. Deletion moves the bundle to the macOS
//! Trash (never a hard rm from the UI), optionally relocating in-bundle disk
//! images out first ("Keep Disks").
//!
//! Long-running operations (reset, import copies) run on background threads; they
//! report errors into a shared queue the next refresh drains into an alert, and
//! mark their bundle "busy" so the row shows a placeholder meanwhile.

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use objc2::rc::Retained;
use objc2::runtime::Sel;
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSAlertSecondButtonReturn, NSApplication,
    NSApplicationDelegate, NSAutoresizingMaskOptions, NSBox, NSBoxType, NSButton, NSColor, NSFont,
    NSImage, NSLayoutAttribute, NSLayoutConstraint, NSModalResponseOK, NSOpenPanel, NSPasteboard,
    NSPasteboardTypeString, NSScrollView, NSStackView, NSStackViewGravity, NSTextField,
    NSUserInterfaceLayoutOrientation, NSView, NSWindow,
};
use objc2_foundation::{
    NSArray, NSEdgeInsets, NSFileManager, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize,
    NSString, NSURL,
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
    /// show a placeholder instead of buttons. Arc: background threads clear it.
    busy: Arc<Mutex<HashSet<PathBuf>>>,
    /// Bundles the user asked to stop (graceful ladder in flight): their Stop
    /// button morphs into the force-stop bolt. Main-thread only; pruned when the
    /// row is no longer running.
    stopping: RefCell<HashSet<PathBuf>>,
    /// Errors from background threads, drained into an alert on the next refresh.
    errors: Arc<Mutex<Vec<String>>>,
    /// busy+stopping as of the last rebuild, so a state flip alone triggers one.
    last_busy: RefCell<HashSet<PathBuf>>,
    last_stopping: RefCell<HashSet<PathBuf>>,
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
                    return;
                }
                // Graceful ladder is in flight: morph this row's Stop into the
                // force-stop bolt (the discoverable second tap).
                self.ivars()
                    .stopping
                    .borrow_mut()
                    .insert(row.bundle.path.clone());
                self.refresh(true);
            }
        }

        #[unsafe(method(forceStopClicked:))]
        fn force_stop_clicked(&self, sender: &NSButton) {
            if let Some(row) = self.row_for(sender) {
                if let Err(e) = spawn::stop_vm(&row.bundle, true) {
                    self.alert("Could not force-stop the VM", &format!("{e:#}"));
                }
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
            if let Some(row) = self.row_for(sender) {
                self.run_delete_flow(&row);
                self.refresh(true);
            }
        }

        #[unsafe(method(copySshClicked:))]
        fn copy_ssh_clicked(&self, sender: &NSButton) {
            if let Some(row) = self.row_for(sender) {
                if let Some(ssh) = &row.ssh {
                    let pb = NSPasteboard::generalPasteboard();
                    pb.clearContents();
                    pb.setString_forType(&NSString::from_str(ssh), unsafe {
                        NSPasteboardTypeString
                    });
                }
            }
        }

        #[unsafe(method(newVmClicked:))]
        fn new_vm_clicked(&self, _sender: &NSButton) {
            self.run_new_vm_flow();
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
            stopping: RefCell::new(HashSet::new()),
            errors: Arc::new(Mutex::new(Vec::new())),
            last_busy: RefCell::new(HashSet::new()),
            last_stopping: RefCell::new(HashSet::new()),
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
        let bounds = content.bounds();
        let (w, h) = (bounds.size.width, bounds.size.height);
        const MARGIN: f64 = 16.0;
        const HEADER_H: f64 = 28.0;

        // Header (title + Import…) pinned to the top edge, stretching with the window.
        let header = NSStackView::stackViewWithViews(&NSArray::new(), mtm);
        header.setOrientation(NSUserInterfaceLayoutOrientation::Horizontal);
        header.setAlignment(NSLayoutAttribute::CenterY);
        header.setFrame(rect(
            MARGIN,
            h - MARGIN - HEADER_H,
            w - 2.0 * MARGIN,
            HEADER_H,
        ));
        header.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMinYMargin,
        );
        let title = NSTextField::labelWithString(&NSString::from_str("Virtual Machines"), mtm);
        title.setFont(Some(&NSFont::boldSystemFontOfSize(16.0)));
        header.addView_inGravity(&title, NSStackViewGravity::Leading);
        let new_vm = unsafe {
            NSButton::buttonWithTitle_target_action(
                &NSString::from_str("New…"),
                Some(self.as_ref()),
                Some(sel!(newVmClicked:)),
                mtm,
            )
        };
        header.addView_inGravity(&new_vm, NSStackViewGravity::Trailing);
        content.addSubview(&header);

        // The row list lives in a scroll view filling the rest of the window, so a
        // large library scrolls instead of clipping. The stack is the document view,
        // pinned to the clip view's top/leading/width by Auto Layout; its height is
        // intrinsic (>= the clip so short content stays top-anchored).
        let scroll = NSScrollView::initWithFrame(
            NSScrollView::alloc(mtm),
            rect(
                MARGIN,
                MARGIN,
                w - 2.0 * MARGIN,
                h - 2.0 * MARGIN - HEADER_H - 8.0,
            ),
        );
        scroll.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        scroll.setHasVerticalScroller(true);
        scroll.setDrawsBackground(false);

        let list = NSStackView::stackViewWithViews(&NSArray::new(), mtm);
        list.setOrientation(NSUserInterfaceLayoutOrientation::Vertical);
        list.setAlignment(NSLayoutAttribute::Width);
        list.setSpacing(8.0);
        // Keep the rows clear of the overlay scroller on the right.
        list.setEdgeInsets(NSEdgeInsets {
            top: 2.0,
            left: 0.0,
            bottom: 2.0,
            right: 6.0,
        });
        list.setTranslatesAutoresizingMaskIntoConstraints(false);
        scroll.setDocumentView(Some(&list));
        let clip = scroll.contentView();
        NSLayoutConstraint::activateConstraints(&NSArray::from_retained_slice(&[
            list.topAnchor().constraintEqualToAnchor(&clip.topAnchor()),
            list.leadingAnchor()
                .constraintEqualToAnchor(&clip.leadingAnchor()),
            list.widthAnchor()
                .constraintEqualToAnchor(&clip.widthAnchor()),
            list.heightAnchor()
                .constraintGreaterThanOrEqualToAnchor(&clip.heightAnchor()),
        ]));
        content.addSubview(&scroll);

        let empty = NSTextField::labelWithString(
            &NSString::from_str(
                "No virtual machines yet.\nClick New… to import a disk image or create an empty VM.",
            ),
            mtm,
        );
        empty.setTextColor(Some(&NSColor::secondaryLabelColor()));
        empty.setFrame(rect(
            MARGIN + 4.0,
            h - MARGIN - HEADER_H - 60.0,
            w - 2.0 * MARGIN,
            40.0,
        ));
        empty.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMinYMargin,
        );
        content.addSubview(&empty);

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
        // Prune transient per-bundle state that resolved itself: a bundle that is
        // no longer running is done "stopping".
        self.ivars()
            .stopping
            .borrow_mut()
            .retain(|p| snap.iter().any(|r| &r.bundle.path == p && r.running));

        let busy_now = self.ivars().busy.lock().unwrap().clone();
        let stopping_now = self.ivars().stopping.borrow().clone();
        let changed = force
            || snap != *self.ivars().rows.borrow()
            || busy_now != *self.ivars().last_busy.borrow()
            || stopping_now != *self.ivars().last_stopping.borrow();
        if !changed {
            return;
        }
        self.rebuild_rows(&snap, &busy_now, &stopping_now);
        *self.ivars().rows.borrow_mut() = snap;
        *self.ivars().last_busy.borrow_mut() = busy_now;
        *self.ivars().last_stopping.borrow_mut() = stopping_now;
    }

    fn rebuild_rows(&self, rows: &[VmRow], busy: &HashSet<PathBuf>, stopping: &HashSet<PathBuf>) {
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
            let view = self.row_view(
                mtm,
                i,
                row,
                busy.contains(&row.bundle.path),
                stopping.contains(&row.bundle.path),
            );
            list.addView_inGravity(&view, NSStackViewGravity::Top);
            if i + 1 < rows.len() {
                list.addView_inGravity(&separator(mtm), NSStackViewGravity::Top);
            }
        }
        if let Some(empty) = self.ivars().empty_label.borrow().as_ref() {
            empty.setHidden(!rows.is_empty());
        }
    }

    /// One VM row: `● | name / config·status / disks / ssh (copy) / [actions…]`.
    fn row_view(
        &self,
        mtm: MainThreadMarker,
        index: usize,
        row: &VmRow,
        busy: bool,
        stopping: bool,
    ) -> Retained<NSView> {
        let outer = NSStackView::stackViewWithViews(&NSArray::new(), mtm);
        outer.setOrientation(NSUserInterfaceLayoutOrientation::Horizontal);
        outer.setAlignment(NSLayoutAttribute::Top);
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
        info.setAlignment(NSLayoutAttribute::Width);
        info.setSpacing(3.0);

        let name = NSTextField::labelWithString(&NSString::from_str(&row.name), mtm);
        name.setFont(Some(&NSFont::boldSystemFontOfSize(13.0)));
        info.addView_inGravity(&name, NSStackViewGravity::Top);

        let mut status = row.summary.clone();
        if busy {
            status.push_str(" · working…");
        } else if stopping {
            status.push_str(" · stopping…");
        } else if row.running {
            status.push_str(&format!(" · running (pid {})", row.pid));
        } else if !row.broken {
            status.push_str(" · stopped");
        }
        info.addView_inGravity(&small_label(mtm, &status), NSStackViewGravity::Top);

        if !row.disks.is_empty() {
            info.addView_inGravity(&small_label(mtm, &row.disks), NSStackViewGravity::Top);
        }
        if let Some(ssh) = &row.ssh {
            let ssh_row = NSStackView::stackViewWithViews(&NSArray::new(), mtm);
            ssh_row.setOrientation(NSUserInterfaceLayoutOrientation::Horizontal);
            ssh_row.setAlignment(NSLayoutAttribute::CenterY);
            ssh_row.setSpacing(4.0);
            ssh_row.addView_inGravity(&small_label(mtm, ssh), NSStackViewGravity::Leading);
            if row.running {
                let copy = self.icon_button(
                    mtm,
                    "doc.on.doc",
                    "Copy the SSH command",
                    sel!(copySshClicked:),
                    index,
                );
                ssh_row.addView_inGravity(&copy, NSStackViewGravity::Leading);
            }
            info.addView_inGravity(&ssh_row, NSStackViewGravity::Top);
        }

        // The action row: lifecycle icons leading, Configure…/trash trailing.
        let actions = NSStackView::stackViewWithViews(&NSArray::new(), mtm);
        actions.setOrientation(NSUserInterfaceLayoutOrientation::Horizontal);
        actions.setAlignment(NSLayoutAttribute::CenterY);
        actions.setSpacing(6.0);
        if busy {
            actions.addView_inGravity(&small_label(mtm, "Working…"), NSStackViewGravity::Leading);
        } else if row.broken {
            let trash =
                self.icon_button(mtm, "trash", "Move to Trash", sel!(deleteClicked:), index);
            actions.addView_inGravity(&trash, NSStackViewGravity::Trailing);
        } else if row.running {
            if stopping {
                // The graceful ladder is running: the second tap is the kill.
                let force = self.icon_button(
                    mtm,
                    "bolt.fill",
                    "Force Stop — kill immediately (shutdown in progress…)",
                    sel!(forceStopClicked:),
                    index,
                );
                actions.addView_inGravity(&force, NSStackViewGravity::Leading);
            } else {
                let stop = self.icon_button(
                    mtm,
                    "power",
                    "Shut Down (then click again to force-kill)",
                    sel!(stopClicked:),
                    index,
                );
                actions.addView_inGravity(&stop, NSStackViewGravity::Leading);
            }
            let reset = self.icon_button(
                mtm,
                "arrow.counterclockwise",
                "Reset (kill and start again)",
                sel!(resetClicked:),
                index,
            );
            actions.addView_inGravity(&reset, NSStackViewGravity::Leading);
        } else {
            let start = self.icon_button(mtm, "play.fill", "Start", sel!(startClicked:), index);
            actions.addView_inGravity(&start, NSStackViewGravity::Leading);
            let configure = unsafe {
                NSButton::buttonWithTitle_target_action(
                    &NSString::from_str("Configure…"),
                    Some(self.as_ref()),
                    Some(sel!(configureClicked:)),
                    mtm,
                )
            };
            configure.setTag(index as isize);
            actions.addView_inGravity(&configure, NSStackViewGravity::Trailing);
            let trash =
                self.icon_button(mtm, "trash", "Move to Trash", sel!(deleteClicked:), index);
            actions.addView_inGravity(&trash, NSStackViewGravity::Trailing);
        }
        info.addView_inGravity(&actions, NSStackViewGravity::Top);

        outer.addView_inGravity(&info, NSStackViewGravity::Leading);
        Retained::into_super(outer)
    }

    /// An SF-symbol icon button with a tooltip (falls back to a title button if the
    /// symbol is unavailable). `tag` = row index.
    fn icon_button(
        &self,
        mtm: MainThreadMarker,
        symbol: &str,
        tooltip: &str,
        action: Sel,
        tag: usize,
    ) -> Retained<NSButton> {
        let b = match NSImage::imageWithSystemSymbolName_accessibilityDescription(
            &NSString::from_str(symbol),
            Some(&NSString::from_str(tooltip)),
        ) {
            Some(img) => unsafe {
                NSButton::buttonWithImage_target_action(
                    &img,
                    Some(self.as_ref()),
                    Some(action),
                    mtm,
                )
            },
            None => unsafe {
                NSButton::buttonWithTitle_target_action(
                    &NSString::from_str(tooltip),
                    Some(self.as_ref()),
                    Some(action),
                    mtm,
                )
            },
        };
        b.setTag(tag as isize);
        b.setToolTip(Some(&NSString::from_str(tooltip)));
        b
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

    /// Delete = move to the macOS Trash (recoverable), with a choice about disks:
    /// "Move to Trash" trashes the whole bundle (in-bundle disks included);
    /// "Keep Disks" first relocates in-bundle disk images out beside the bundle,
    /// then trashes the rest. Disks referenced by absolute path are never touched.
    fn run_delete_flow(&self, row: &VmRow) {
        if row.running {
            self.alert("VM is running", "Stop the VM before deleting it.");
            return;
        }
        let mtm = self.mtm();
        // Which disks live inside the bundle? (broken bundle: unknown → whole-bundle trash only)
        let bundle_disks: Vec<PathBuf> = row
            .bundle
            .load()
            .map(|cfg| {
                cfg.disks
                    .iter()
                    .filter(|d| !d.path.is_absolute())
                    .map(|d| d.path.clone())
                    .collect()
            })
            .unwrap_or_default();

        let alert = NSAlert::new(mtm);
        alert.setMessageText(&NSString::from_str(&format!(
            "Move “{}” to the Trash?",
            row.name
        )));
        alert.setInformativeText(&NSString::from_str(if bundle_disks.is_empty() {
            "The VM bundle will be moved to the Trash. \
             Disk images outside the bundle are never touched."
        } else {
            "The VM and the disk images inside its bundle will be moved to the Trash \
             (recoverable from there). “Keep Disks” moves the disk images out into the \
             VM library first. Disk images outside the bundle are never touched."
        }));
        alert.addButtonWithTitle(&NSString::from_str("Move to Trash"));
        if !bundle_disks.is_empty() {
            alert.addButtonWithTitle(&NSString::from_str("Keep Disks"));
        }
        alert.addButtonWithTitle(&NSString::from_str("Cancel"));

        let response = alert.runModal();
        let keep_disks = !bundle_disks.is_empty() && response == NSAlertSecondButtonReturn;
        if response != NSAlertFirstButtonReturn && !keep_disks {
            return; // Cancel
        }

        let result = (|| -> anyhow::Result<()> {
            if keep_disks {
                let dest_dir = vmlib::bundle::library_dir();
                for rel in &bundle_disks {
                    let src = row.bundle.resolve_path(rel);
                    if !src.exists() {
                        continue;
                    }
                    let file = rel
                        .file_name()
                        .map(|f| f.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "disk.raw".into());
                    let dest = unique_path(&dest_dir.join(format!("{}-{}", row.name, file)));
                    std::fs::rename(&src, &dest).map_err(|e| {
                        anyhow::anyhow!("moving {} out of the bundle: {e}", src.display())
                    })?;
                }
            }
            trash(&row.bundle.path)
        })();
        if let Err(e) = result {
            self.alert("Could not delete the VM", &format!("{e:#}"));
        }
    }

    /// New…: the one entry point for making VMs — import an existing disk image,
    /// or create an empty VM (blank sparse disk + optional installer ISO).
    fn run_new_vm_flow(&self) {
        let alert = NSAlert::new(self.mtm());
        alert.setMessageText(&NSString::from_str("New Virtual Machine"));
        alert.setInformativeText(&NSString::from_str(
            "Import an existing disk image as a VM, or create an empty VM with a \
             blank disk (attach an installer ISO to install an OS into it).",
        ));
        alert.addButtonWithTitle(&NSString::from_str("Import a Disk Image…"));
        alert.addButtonWithTitle(&NSString::from_str("New Empty VM…"));
        alert.addButtonWithTitle(&NSString::from_str("Cancel"));
        match alert.runModal() {
            r if r == NSAlertFirstButtonReturn => self.run_import_flow(),
            r if r == NSAlertSecondButtonReturn => self.run_blank_vm_flow(),
            _ => {}
        }
    }

    /// New empty VM: name + blank-disk size, then an optional installer ISO. With a
    /// blank disk and an ISO, the firmware finds no bootable disk and boots the ISO
    /// (El Torito → the installer) — the OS-install path.
    fn run_blank_vm_flow(&self) {
        let mtm = self.mtm();
        let alert = NSAlert::new(mtm);
        alert.setMessageText(&NSString::from_str("New Empty VM"));
        alert.setInformativeText(&NSString::from_str(
            "The boot disk is created blank and sparse — it only uses real space as \
             the guest writes.",
        ));
        alert.addButtonWithTitle(&NSString::from_str("Create"));
        alert.addButtonWithTitle(&NSString::from_str("Cancel"));
        let accessory = NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, 300.0, 62.0));
        let name_field = labeled_field(mtm, &accessory, 34.0, "Name:", "New VM");
        let size_field = labeled_field(mtm, &accessory, 4.0, "Disk size:", "40G");
        alert.setAccessoryView(Some(&accessory));
        if alert.runModal() != NSAlertFirstButtonReturn {
            return;
        }
        let name = name_field.stringValue().to_string().trim().to_string();
        if name.is_empty() {
            self.alert("Cannot create the VM", "The VM needs a non-empty name.");
            return;
        }
        let size = match crate::parse_disk_size(&size_field.stringValue().to_string()) {
            Ok(s) => s,
            Err(e) => {
                self.alert("Invalid disk size", &format!("{e:#}"));
                return;
            }
        };

        // Optional installer ISO (referenced where it is, read-only by nature).
        let iso_alert = NSAlert::new(mtm);
        iso_alert.setMessageText(&NSString::from_str("Attach an installer ISO?"));
        iso_alert.setInformativeText(&NSString::from_str(
            "With a blank disk and an ISO attached, starting the VM boots the OS installer.",
        ));
        iso_alert.addButtonWithTitle(&NSString::from_str("Choose ISO…"));
        iso_alert.addButtonWithTitle(&NSString::from_str("No ISO"));
        iso_alert.addButtonWithTitle(&NSString::from_str("Cancel"));
        let cdrom = match iso_alert.runModal() {
            r if r == NSAlertFirstButtonReturn => {
                let panel = NSOpenPanel::openPanel(mtm);
                panel.setCanChooseFiles(true);
                panel.setCanChooseDirectories(false);
                panel.setAllowsMultipleSelection(false);
                if panel.runModal() != NSModalResponseOK {
                    return; // backing out of the picker cancels the flow
                }
                panel
                    .URL()
                    .and_then(|u| u.path())
                    .map(|p| PathBuf::from(p.to_string()))
            }
            r if r == NSAlertSecondButtonReturn => None,
            _ => return,
        };

        // Creating the blank sparse disk is instant; no background thread needed.
        let result = (|| -> anyhow::Result<()> {
            let dest = vmlib::bundle::library_dir();
            std::fs::create_dir_all(&dest)?;
            vmlib::import::create(
                &vmlib::import::CreateOpts {
                    name,
                    disk: None,
                    import_mode: vmlib::import::ImportMode::CloneIntoBundle,
                    blank_size: Some(size),
                    cdrom,
                    cpus: 4,
                    memory: vmlib::schema::Memory::default(),
                    ssh_port: 0,
                    window: true,
                },
                &dest,
            )?;
            Ok(())
        })();
        if let Err(e) = result {
            self.alert("Could not create the VM", &format!("{e:#}"));
        }
        self.refresh(true);
    }

    /// Import: pick a disk image, name the VM, clone it into a new bundle in the
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
                        blank_size: None,
                        cdrom: None,
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

fn small_label(mtm: MainThreadMarker, text: &str) -> Retained<NSTextField> {
    let l = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    l.setFont(Some(&NSFont::systemFontOfSize(11.0)));
    l.setTextColor(Some(&NSColor::secondaryLabelColor()));
    l
}

fn separator(mtm: MainThreadMarker) -> Retained<NSView> {
    let sep = NSBox::initWithFrame(NSBox::alloc(mtm), rect(0.0, 0.0, 100.0, 1.0));
    sep.setBoxType(NSBoxType::Separator);
    Retained::into_super(sep)
}

/// Move a path to the macOS Trash (recoverable — the UI never hard-deletes).
fn trash(path: &Path) -> anyhow::Result<()> {
    let s = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("path is not valid UTF-8: {path:?}"))?;
    let url = NSURL::fileURLWithPath(&NSString::from_str(s));
    NSFileManager::defaultManager()
        .trashItemAtURL_resultingItemURL_error(&url, None)
        .map_err(|e| anyhow::anyhow!("moving {} to the Trash: {}", path.display(), e))
}

/// `p`, or `p` with `-1`/`-2`/… appended before the extension until it's free.
fn unique_path(p: &Path) -> PathBuf {
    if !p.exists() {
        return p.to_path_buf();
    }
    let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned());
    let ext = p.extension().map(|e| e.to_string_lossy().into_owned());
    let dir = p.parent().unwrap_or(Path::new("."));
    for n in 1..1000 {
        let name = match (&stem, &ext) {
            (Some(s), Some(e)) => format!("{s}-{n}.{e}"),
            (Some(s), None) => format!("{s}-{n}"),
            _ => format!("disk-{n}"),
        };
        let candidate = dir.join(name);
        if !candidate.exists() {
            return candidate;
        }
    }
    p.to_path_buf()
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
