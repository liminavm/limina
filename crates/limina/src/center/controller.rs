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
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::Sel;
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly, Message};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSAlertSecondButtonReturn, NSApplication,
    NSApplicationDelegate, NSBezelStyle, NSBox, NSBoxType, NSButton, NSColor, NSFont, NSImage,
    NSLayoutAttribute, NSLayoutConstraint, NSLayoutConstraintOrientation, NSModalResponseOK,
    NSOpenPanel, NSPasteboard, NSPasteboardTypeString, NSPopUpButton, NSPopover, NSPopoverBehavior,
    NSScrollView, NSStackView, NSStackViewGravity, NSTextField, NSUserInterfaceLayoutOrientation,
    NSView, NSViewController, NSWindow,
};
use objc2_foundation::{
    NSArray, NSEdgeInsets, NSFileManager, NSNotification, NSObject, NSObjectProtocol, NSPoint,
    NSRect, NSRectEdge, NSRunLoop, NSRunLoopCommonModes, NSSize, NSString, NSTimer, NSURL,
};

use super::{model, model::VmRow, spawn};
use crate::balloon_policy::ReclaimMode;
use crate::vmlib;

/// Window-content margin, shared by the chrome layout and the fit-to-content math.
const MARGIN: f64 = 16.0;

/// The list stack's right edge inset (keeps rows clear of the overlay scroller).
const LIST_RIGHT_INSET: f64 = 6.0;

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
    /// Non-list window chrome height (margins + header + gap), measured by
    /// `build_ui`; `fit_window_to_content` adds the list's fitting height to it.
    chrome_h: std::cell::Cell<f64>,
    /// The center window, for re-showing after close (the center is persistent:
    /// closing the window hides it, Dock-click/reopen or the show-center
    /// notification brings it back). Set once by `new`.
    window: RefCell<Option<Retained<NSWindow>>>,
    /// The Configure sheet's resolution popup, present only while the (modal)
    /// sheet runs — the Display popup's action toggles its enabled state.
    cfg_resolution_popup: RefCell<Option<Retained<NSPopUpButton>>>,
    /// The currently shown "?" help popover (kept alive while displayed).
    cfg_help_popover: RefCell<Option<Retained<NSPopover>>>,
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
        // The center is persistent: closing its window only hides it (the Dock
        // icon, a reopen, or the show-center notification brings it back), and
        // Cmd-Q is the real quit. Running VMs are independent children either way.
        #[unsafe(method(applicationShouldTerminateAfterLastWindowClosed:))]
        fn should_terminate_after_last_window_closed(&self, _app: &NSApplication) -> bool {
            false
        }

        // Dock-icon click (or app reopen) with the window closed: re-show it.
        #[unsafe(method(applicationShouldHandleReopen:hasVisibleWindows:))]
        fn should_handle_reopen(&self, _app: &NSApplication, has_visible: bool) -> bool {
            if !has_visible {
                self.show_window();
            }
            true
        }

        // Finder opened one or more `.liminavm` bundles (double-click, or a drop
        // on the Dock icon — CFBundleDocumentTypes in build-app.sh routes them
        // here). Start each stopped one; a running one just stays as it is.
        #[unsafe(method(application:openURLs:))]
        fn application_open_urls(&self, _app: &NSApplication, urls: &NSArray<NSURL>) {
            for url in urls.iter() {
                let Some(path) = url.path().map(|p| PathBuf::from(p.to_string())) else {
                    continue;
                };
                let bundle = vmlib::bundle::VmBundle::new(&path);
                if !bundle.vm_toml().is_file() {
                    self.alert(
                        "Not a Limina VM",
                        &format!("{} does not contain a vm.toml.", path.display()),
                    );
                    continue;
                }
                if !vmlib::runtime::status(&bundle).is_running() {
                    let errors = self.ivars().errors.clone();
                    if let Err(e) = spawn::start_vm(&bundle, &errors) {
                        self.alert("Could not start the VM", &format!("{e:#}"));
                    }
                }
            }
            self.refresh(true);
        }
    }

    impl CenterController {
        #[unsafe(method(startClicked:))]
        fn start_clicked(&self, sender: &NSButton) {
            if let Some(row) = self.row_for(sender) {
                let errors = self.ivars().errors.clone();
                if let Err(e) = spawn::start_vm(&row.bundle, &errors) {
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
                if let Err(e) = spawn::reset_vm(&bundle, &errors) {
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

        // The Configure sheet's Display popup changed: the Resolution popup only
        // applies to Fixed Size.
        #[unsafe(method(configureDisplayModeChanged:))]
        fn configure_display_mode_changed(&self, sender: &NSPopUpButton) {
            let fixed = sender.indexOfSelectedItem() == DISPLAY_FIXED_INDEX;
            if let Some(popup) = self.ivars().cfg_resolution_popup.borrow().as_ref() {
                popup.setEnabled(fixed);
            }
        }

        // A Configure-sheet "?" was clicked: show its row's explanation (the
        // button's tooltip text) in a transient popover, immediately — the button
        // looks clickable, so clicking must not be slower than hovering.
        #[unsafe(method(configureHelpClicked:))]
        fn configure_help_clicked(&self, sender: &NSButton) {
            let Some(tip) = sender.toolTip() else { return };
            let mtm = self.mtm();
            let label = NSTextField::wrappingLabelWithString(&tip, mtm);
            label.setPreferredMaxLayoutWidth(280.0);
            let size = label.fittingSize();
            label.setFrame(rect(12.0, 12.0, size.width, size.height));
            let container = NSView::initWithFrame(
                NSView::alloc(mtm),
                rect(0.0, 0.0, size.width + 24.0, size.height + 24.0),
            );
            container.addSubview(&label);
            let vc = NSViewController::new(mtm);
            vc.setView(&container);
            let popover = NSPopover::new(mtm);
            popover.setContentViewController(Some(&vc));
            popover.setBehavior(NSPopoverBehavior::Transient);
            // Keep it alive while shown (replacing the previous one closes it).
            *self.ivars().cfg_help_popover.borrow_mut() = Some(popover.clone());
            popover.showRelativeToRect_ofView_preferredEdge(
                sender.bounds(),
                sender,
                NSRectEdge::MaxX,
            );
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
                    // Feedback in place: flip the clicked command to "Copied ✓",
                    // then force a row rebuild to restore it.
                    sender.setTitle(&NSString::from_str("Copied ✓"));
                    let controller = self.retain();
                    let block = RcBlock::new(move |_t: NonNull<NSTimer>| {
                        controller.refresh(true);
                    });
                    let timer =
                        unsafe { NSTimer::timerWithTimeInterval_repeats_block(1.2, false, &block) };
                    unsafe {
                        NSRunLoop::currentRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes);
                    }
                }
            }
        }

        #[unsafe(method(newVmClicked:))]
        fn new_vm_clicked(&self, _sender: &NSButton) {
            self.run_new_vm_flow();
        }

        // The show-center distributed notification (posted by a second `limina
        // center` or a VM window's "Control Center…" menu item).
        #[unsafe(method(showCenterRequested:))]
        fn show_center_requested(&self, _note: &NSNotification) {
            self.show_window();
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
            chrome_h: std::cell::Cell::new(0.0),
            window: RefCell::new(None),
            cfg_resolution_popup: RefCell::new(None),
            cfg_help_popover: RefCell::new(None),
        });
        // SAFETY: NSObject's init signature.
        let this: Retained<Self> = unsafe { msg_send![super(this), init] };
        *this.ivars().window.borrow_mut() = Some(window.retain());
        this.build_ui(mtm, window);
        this
    }

    /// Re-show the (possibly closed/hidden) center window and bring the app forward.
    fn show_window(&self) {
        if let Some(window) = self.ivars().window.borrow().as_ref() {
            window.makeKeyAndOrderFront(None);
        }
        let app = NSApplication::sharedApplication(self.mtm());
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
        self.refresh(true);
    }

    /// Build the static chrome: header (title + New…), the scrolling row list, and
    /// the empty-state label — all positioned by Auto Layout. (No manual frames:
    /// `stackViewWithViews` opts out of autoresizing-mask translation, so masks
    /// would be silently ignored and the header would drift on window resize.)
    fn build_ui(&self, mtm: MainThreadMarker, window: &NSWindow) {
        let content = window.contentView().expect("window content view");

        let header = NSStackView::stackViewWithViews(&NSArray::new(), mtm);
        header.setOrientation(NSUserInterfaceLayoutOrientation::Horizontal);
        header.setAlignment(NSLayoutAttribute::CenterY);
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
        let scroll = NSScrollView::new(mtm);
        scroll.setTranslatesAutoresizingMaskIntoConstraints(false);
        scroll.setHasVerticalScroller(true);
        scroll.setDrawsBackground(false);

        let list = NSStackView::stackViewWithViews(&NSArray::new(), mtm);
        list.setOrientation(NSUserInterfaceLayoutOrientation::Vertical);
        // Rows are left-anchored and stretched to the list width by explicit
        // constraints in rebuild_rows — alignment alone left short rows hugging
        // their content, so dots/buttons drifted between rows of different widths.
        list.setAlignment(NSLayoutAttribute::Leading);
        list.setSpacing(8.0);
        // Keep the rows clear of the overlay scroller on the right.
        list.setEdgeInsets(NSEdgeInsets {
            top: 2.0,
            left: 0.0,
            bottom: 2.0,
            right: LIST_RIGHT_INSET,
        });
        list.setTranslatesAutoresizingMaskIntoConstraints(false);
        scroll.setDocumentView(Some(&list));
        let clip = scroll.contentView();
        content.addSubview(&scroll);

        let empty = NSTextField::labelWithString(
            &NSString::from_str(
                "No virtual machines yet.\nClick New… to import a disk image or create an empty VM.",
            ),
            mtm,
        );
        empty.setTextColor(Some(&NSColor::secondaryLabelColor()));
        empty.setTranslatesAutoresizingMaskIntoConstraints(false);
        content.addSubview(&empty);

        NSLayoutConstraint::activateConstraints(&NSArray::from_retained_slice(&[
            // Header: pinned to the top edge, full width.
            header
                .topAnchor()
                .constraintEqualToAnchor_constant(&content.topAnchor(), MARGIN),
            header
                .leadingAnchor()
                .constraintEqualToAnchor_constant(&content.leadingAnchor(), MARGIN),
            header
                .trailingAnchor()
                .constraintEqualToAnchor_constant(&content.trailingAnchor(), -MARGIN),
            // Scroll view: everything below the header.
            scroll
                .topAnchor()
                .constraintEqualToAnchor_constant(&header.bottomAnchor(), 8.0),
            scroll
                .leadingAnchor()
                .constraintEqualToAnchor_constant(&content.leadingAnchor(), MARGIN),
            scroll
                .trailingAnchor()
                .constraintEqualToAnchor_constant(&content.trailingAnchor(), -MARGIN),
            scroll
                .bottomAnchor()
                .constraintEqualToAnchor_constant(&content.bottomAnchor(), -MARGIN),
            // Document stack inside the clip view (top-anchored short content).
            list.topAnchor().constraintEqualToAnchor(&clip.topAnchor()),
            list.leadingAnchor()
                .constraintEqualToAnchor(&clip.leadingAnchor()),
            list.widthAnchor()
                .constraintEqualToAnchor(&clip.widthAnchor()),
            list.heightAnchor()
                .constraintGreaterThanOrEqualToAnchor(&clip.heightAnchor()),
            // Empty-state placeholder just below the header.
            empty
                .topAnchor()
                .constraintEqualToAnchor_constant(&header.bottomAnchor(), 24.0),
            empty
                .leadingAnchor()
                .constraintEqualToAnchor_constant(&content.leadingAnchor(), MARGIN + 4.0),
        ]));

        *self.ivars().list.borrow_mut() = Some(list);
        *self.ivars().empty_label.borrow_mut() = Some(empty);
        self.ivars()
            .chrome_h
            .set(2.0 * MARGIN + header.fittingSize().height + 8.0);
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
        self.fit_window_to_content();
    }

    /// Track the list with the window size: after a rebuild, resize (animated, top-left
    /// corner pinned) so the rows fit exactly — height shrinks/grows with the VM count,
    /// width follows the widest card (a lone short-named VM gets a compact window, a
    /// long ssh line widens it) — clamped to the screen. Position stays the user's.
    fn fit_window_to_content(&self) {
        let list_ref = self.ivars().list.borrow();
        let Some(list) = list_ref.as_ref() else {
            return;
        };
        let Some(window) = list.window() else {
            return;
        };
        let fitting = list.fittingSize();
        let (max_h, max_w) = window
            .screen()
            .map(|s| {
                let f = s.visibleFrame().size;
                (f.height - 40.0, f.width - 80.0)
            })
            .unwrap_or((800.0, 1200.0));
        let desired_h =
            (self.ivars().chrome_h.get() + fitting.height.max(60.0)).clamp(160.0, max_h.max(160.0));
        let desired_w = (fitting.width + 2.0 * MARGIN).clamp(380.0, max_w.max(380.0));
        let frame = window.frame();
        let content = window.contentRectForFrameRect(frame);
        if (content.size.height - desired_h).abs() < 1.0
            && (content.size.width - desired_w).abs() < 1.0
        {
            return;
        }
        let mut new_frame = window.frameRectForContentRect(NSRect::new(
            content.origin,
            NSSize::new(desired_w, desired_h),
        ));
        new_frame.origin.y = frame.origin.y + frame.size.height - new_frame.size.height;
        new_frame.origin.x = frame.origin.x;
        window.setFrame_display_animate(new_frame, true, true);
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
        // Every card (and separator) spans the full list width, so the widest row
        // sets the pace and status dots / action buttons line up across VMs.
        let full_width = |v: &NSView| {
            v.widthAnchor()
                .constraintEqualToAnchor_constant(&list.widthAnchor(), -LIST_RIGHT_INSET)
                .setActive(true);
        };
        for (i, row) in rows.iter().enumerate() {
            let view = self.row_view(
                mtm,
                i,
                row,
                busy.contains(&row.bundle.path),
                stopping.contains(&row.bundle.path),
            );
            list.addView_inGravity(&view, NSStackViewGravity::Top);
            full_width(&view);
            if i + 1 < rows.len() {
                let sep = separator(mtm);
                list.addView_inGravity(&sep, NSStackViewGravity::Top);
                full_width(&sep);
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

        // Text lines anchor left (Width is NOT a valid stack alignment — it silently
        // fell back to centering, which is what skewed rows against each other); the
        // ssh and action rows get explicit full-width constraints below so their
        // trailing content reaches the card's right edge.
        let info = NSStackView::stackViewWithViews(&NSArray::new(), mtm);
        info.setOrientation(NSUserInterfaceLayoutOrientation::Vertical);
        info.setAlignment(NSLayoutAttribute::Leading);
        info.setSpacing(3.0);
        let info_wide = |v: &NSView| {
            v.widthAnchor()
                .constraintEqualToAnchor(&info.widthAnchor())
                .setActive(true);
        };

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
            // Right-aligned on its own line. A real command is itself the copy
            // affordance: a borderless text button that copies on click (the action
            // flips its title to "Copied ✓" for a moment).
            let ssh_row = NSStackView::stackViewWithViews(&NSArray::new(), mtm);
            ssh_row.setOrientation(NSUserInterfaceLayoutOrientation::Horizontal);
            ssh_row.setAlignment(NSLayoutAttribute::CenterY);
            if ssh.starts_with("ssh -p") {
                let cmd = unsafe {
                    NSButton::buttonWithTitle_target_action(
                        &NSString::from_str(ssh),
                        Some(self.as_ref()),
                        Some(sel!(copySshClicked:)),
                        mtm,
                    )
                };
                cmd.setBordered(false);
                cmd.setFont(Some(&NSFont::systemFontOfSize(11.0)));
                cmd.setContentTintColor(Some(&NSColor::linkColor()));
                cmd.setToolTip(Some(&NSString::from_str("Click to copy")));
                cmd.setTag(index as isize);
                ssh_row.addView_inGravity(&cmd, NSStackViewGravity::Trailing);
            } else {
                ssh_row.addView_inGravity(&small_label(mtm, ssh), NSStackViewGravity::Trailing);
            }
            info.addView_inGravity(&ssh_row, NSStackViewGravity::Top);
            info_wide(&ssh_row);
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
            // A start that pre-flight already knows cannot work: leave the button visible so
            // the row still reads as a VM, but disabled and carrying the reason. Clicking a
            // control that silently does nothing is the bug this whole path exists to fix.
            let start = match &row.blocked {
                Some(why) => self.icon_button(
                    mtm,
                    "play.fill",
                    &format!("Cannot start: {why}"),
                    sel!(startClicked:),
                    index,
                ),
                None => self.icon_button(mtm, "play.fill", "Start", sel!(startClicked:), index),
            };
            start.setEnabled(row.blocked.is_none());
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
        info_wide(&actions);

        // The info column absorbs the row's slack width: gravity areas never stretch
        // their views on their own (hugging low alone did nothing), so pin info's
        // trailing edge to the row's — that way the ssh command and the trailing
        // action buttons reach the same right edge on every card.
        info.setContentHuggingPriority_forOrientation(
            1.0,
            NSLayoutConstraintOrientation::Horizontal,
        );
        outer.addView_inGravity(&info, NSStackViewGravity::Leading);
        info.trailingAnchor()
            .constraintEqualToAnchor(&outer.trailingAnchor())
            .setActive(true);
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

    /// Configure…: vCPUs / memory / reclaim / pages / SSH port / display in a modal alert.
    /// Only reachable for stopped VMs (the row offers no Configure while running).
    /// Every row carries a “?” hover with its explanation instead of a wall of
    /// informative text; Display is a popup, with a resolution popup (the sizes
    /// this Mac's display offers) that lights up for Fixed Size.
    fn run_configure_sheet(&self, row: &VmRow) {
        let mtm = self.mtm();
        let mut cfg = match row.bundle.load() {
            Ok(c) => c,
            Err(e) => {
                self.alert("Cannot configure", &format!("{e:#}"));
                return;
            }
        };

        let mem_now = cfg.hardware.memory.0.clone();
        let ssh_now = cfg.networks.first().map(|n| n.ssh_port).unwrap_or(0);

        let alert = NSAlert::new(mtm);
        alert.setMessageText(&NSString::from_str(&format!("Configure “{}”", row.name)));
        alert.setInformativeText(&NSString::from_str(
            "Changes apply the next time the VM starts.",
        ));
        alert.addButtonWithTitle(&NSString::from_str("Save"));
        alert.addButtonWithTitle(&NSString::from_str("Cancel"));

        let accessory = NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, 322.0, 272.0));
        let cpus_field = labeled_field(
            mtm,
            &accessory,
            242.0,
            "vCPUs:",
            &cfg.hardware.cpus.to_string(),
        );
        self.row_help(
            &accessory,
            242.0,
            &cpus_field,
            "How many virtual CPUs the guest sees.",
        );
        let mem_field = labeled_field(mtm, &accessory, 212.0, "Memory:", &mem_now);
        self.row_help(
            &accessory,
            212.0,
            &mem_field,
            "The maximum guest RAM — \"4G\", \"8GiB\", or a bare MiB count. Idle guest \
             memory is returned to the Mac according to Reclaim.",
        );
        let reclaim_popup = labeled_popup(
            mtm,
            &accessory,
            182.0,
            "Reclaim:",
            &RECLAIM_CHOICES,
            reclaim_index(cfg.hardware.reclaim),
        );
        self.row_help(
            &accessory,
            182.0,
            &reclaim_popup,
            "How hard idle guest memory is returned to the Mac. Moderate keeps some \
             guest disk cache unless the Mac is under memory pressure; Light engages \
             only when the Mac is; Aggressive always squeezes; Disabled never reclaims.",
        );
        let granule_popup = labeled_popup(
            mtm,
            &accessory,
            152.0,
            "Memory pages:",
            &GRANULE_CHOICES,
            granule_index(cfg.hardware.ipa_granule),
        );
        self.row_help(
            &accessory,
            152.0,
            &granule_popup,
            "How finely the Mac maps this VM's memory. 4 KB suits any guest, including a \
             stock Linux distro whose own pages are 4 KB — such a guest gets no hardware \
             Vulkan without it, though OpenGL stays accelerated either way. Choose 16 KB \
             only if this guest runs a kernel with 16 KB pages: it then matches the Mac's \
             own page size and runs a few percent faster. Shut the VM down fully before \
             changing this — a suspended guest resumes with the memory layout it was \
             saved with.",
        );
        let ssh_field = labeled_field(mtm, &accessory, 122.0, "SSH port:", &ssh_now.to_string());
        self.row_help(
            &accessory,
            122.0,
            &ssh_field,
            "The host port forwarded to the guest's SSH. 0 picks a free port \
             automatically at each start.",
        );

        let display_popup = labeled_popup(
            mtm,
            &accessory,
            92.0,
            "Display:",
            &DISPLAY_CHOICES,
            display_index(cfg.display.resolution),
        );
        // SAFETY: standard target/action wiring; self outlives the modal sheet.
        unsafe {
            display_popup.setTarget(Some(self.as_ref()));
            display_popup.setAction(Some(sel!(configureDisplayModeChanged:)));
        }
        self.row_help(
            &accessory,
            92.0,
            &display_popup,
            "Match Host drives the guest to the resolution of the screen the window is \
             on, letterboxing the window as needed (fullscreen fits exactly). Dynamic \
             makes the guest follow the window as you resize it. Fixed Size drives one \
             resolution at boot and never changes it.",
        );

        // The resolution list: what this Mac's display offers (plus the currently
        // configured size, so an existing choice never silently changes).
        let mut resolutions = mac_display_points();
        if let vmlib::schema::DisplayResolution::Fixed(w, h) = cfg.display.resolution {
            if !resolutions.contains(&(w, h)) {
                resolutions.push((w, h));
                resolutions.sort_by_key(|&(w, h)| std::cmp::Reverse(u64::from(w) * u64::from(h)));
            }
        }
        let titles: Vec<String> = resolutions
            .iter()
            .map(|(w, h)| format!("{w} × {h}"))
            .collect();
        let title_refs: Vec<&str> = titles.iter().map(String::as_str).collect();
        let selected = match cfg.display.resolution {
            vmlib::schema::DisplayResolution::Fixed(w, h) => {
                resolutions.iter().position(|&r| r == (w, h)).unwrap_or(0) as isize
            }
            // Not fixed (yet): preselect what the guest gets today — the screen size.
            _ => {
                let (w, h) = main_screen_points();
                resolutions.iter().position(|&r| r == (w, h)).unwrap_or(0) as isize
            }
        };
        let resolution_popup =
            labeled_popup(mtm, &accessory, 62.0, "Resolution:", &title_refs, selected);
        resolution_popup.setEnabled(matches!(
            cfg.display.resolution,
            vmlib::schema::DisplayResolution::Fixed(..)
        ));
        self.row_help(
            &accessory,
            62.0,
            &resolution_popup,
            "The fixed guest resolution — the sizes this Mac offers for its display. \
             Only used with Fixed Size.",
        );

        let notch_popup = labeled_popup(
            mtm,
            &accessory,
            32.0,
            "Notch:",
            &NOTCH_CHOICES,
            notch_index(cfg.display.notch),
        );
        self.row_help(
            &accessory,
            32.0,
            &notch_popup,
            "What a full-screen guest does with the camera housing on a MacBook's \
             built-in display. Avoid keeps the guest below it and leaves that strip \
             black; Use Full Screen gives the guest the whole panel, with the housing \
             covering part of its top edge. No effect on an external display.",
        );

        // Presets, plus a "Custom" entry when the VM carries a hand-edited value — so
        // opening this sheet and pressing Save can never silently round someone's
        // vm.toml down to the nearest preset.
        let resistance_titles = hold_titles();
        let resistance_refs: Vec<&str> = resistance_titles.iter().map(String::as_str).collect();
        let resistance_popup = labeled_popup(
            mtm,
            &accessory,
            2.0,
            "Edge hold:",
            &resistance_refs,
            hold_index(cfg.display.edge_resistance),
        );
        self.row_help(
            &accessory,
            2.0,
            &resistance_popup,
            "In full screen the pointer is held inside the guest, so a stray flick \
             can't reveal the Mac's menu bar or land on the next display. Press it \
             against an edge and hold to let it out — this is how long that press has \
             to last at the top edge, where it also brings the Mac's menu bar back; \
             the sides let go sooner. Off leaves the pointer free, exactly as in a \
             window. Needs \
             Accessibility permission, like pointer capture; without it the pointer \
             stays free whatever this says.",
        );

        alert.setAccessoryView(Some(&accessory));

        // Published so configureDisplayModeChanged: can toggle it while the sheet runs.
        *self.ivars().cfg_resolution_popup.borrow_mut() = Some(resolution_popup.clone());
        let response = alert.runModal();
        *self.ivars().cfg_resolution_popup.borrow_mut() = None;
        if response != NSAlertFirstButtonReturn {
            return;
        }

        // Validate with the same parsers the CLI uses; reject without saving.
        let apply = || -> anyhow::Result<vmlib::schema::VmConfig> {
            let cpus: u8 = cpus_field.stringValue().to_string().trim().parse()?;
            anyhow::ensure!(cpus > 0, "vCPUs must be at least 1");
            let memory = vmlib::schema::Memory::parse(&mem_field.stringValue().to_string())?;
            let ssh_port: u16 = ssh_field.stringValue().to_string().trim().parse()?;
            let resolution = match display_popup.indexOfSelectedItem() {
                0 => vmlib::schema::DisplayResolution::Host,
                1 => vmlib::schema::DisplayResolution::Dynamic,
                _ => {
                    let i = resolution_popup.indexOfSelectedItem().max(0) as usize;
                    let (w, h) = resolutions
                        .get(i)
                        .copied()
                        .ok_or_else(|| anyhow::anyhow!("no resolution selected"))?;
                    vmlib::schema::DisplayResolution::Fixed(w, h)
                }
            };
            cfg.hardware.cpus = cpus;
            cfg.hardware.memory = memory;
            cfg.hardware.reclaim = reclaim_from_index(reclaim_popup.indexOfSelectedItem());
            cfg.hardware.ipa_granule = granule_from_index(granule_popup.indexOfSelectedItem());
            cfg.display.resolution = resolution;
            cfg.display.notch = notch_from_index(notch_popup.indexOfSelectedItem());
            cfg.display.edge_resistance = hold_from_index(
                resistance_popup.indexOfSelectedItem(),
                cfg.display.edge_resistance,
            );
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

/// Popup titles for the reclaim mode, in a fixed order the index helpers share.
const RECLAIM_CHOICES: [&str; 4] = ["Disabled", "Light", "Moderate", "Aggressive"];

fn reclaim_index(mode: ReclaimMode) -> isize {
    match mode {
        ReclaimMode::Disabled => 0,
        ReclaimMode::Light => 1,
        ReclaimMode::Moderate => 2,
        ReclaimMode::Aggressive => 3,
    }
}

fn reclaim_from_index(index: isize) -> ReclaimMode {
    match index {
        0 => ReclaimMode::Disabled,
        1 => ReclaimMode::Light,
        3 => ReclaimMode::Aggressive,
        _ => ReclaimMode::Moderate,
    }
}

/// Popup titles for the stage-2 granule, in [`vmlib::schema::IpaGranule`] order. Named for
/// the guest page size each one accommodates, which is the only thing the choice depends on.
const GRANULE_CHOICES: [&str; 2] = ["4 KB", "16 KB"];

fn granule_index(granule: vmlib::schema::IpaGranule) -> isize {
    match granule {
        vmlib::schema::IpaGranule::FourK => 0,
        vmlib::schema::IpaGranule::SixteenK => 1,
    }
}

fn granule_from_index(index: isize) -> vmlib::schema::IpaGranule {
    match index {
        1 => vmlib::schema::IpaGranule::SixteenK,
        _ => vmlib::schema::IpaGranule::FourK,
    }
}

/// Popup titles for the camera-housing policy, in `NotchPolicy` order.
const NOTCH_CHOICES: [&str; 2] = ["Avoid", "Use Full Screen"];

fn notch_index(policy: vmlib::schema::NotchPolicy) -> isize {
    match policy {
        vmlib::schema::NotchPolicy::Avoid => 0,
        vmlib::schema::NotchPolicy::Extend => 1,
    }
}

fn notch_from_index(index: isize) -> vmlib::schema::NotchPolicy {
    match index {
        1 => vmlib::schema::NotchPolicy::Extend,
        _ => vmlib::schema::NotchPolicy::Avoid,
    }
}

/// The edge-hold presets, in popup order — [`vmlib::schema::EdgeHold`] owns the durations.
///
/// There is no "Custom" entry any more. There used to be, so that a hand-edited points value
/// survived a Save; the unit is a duration now and `EdgeHold::from_toml` snaps anything else onto
/// the nearest preset, so the popup can always show a real selection.
fn hold_titles() -> Vec<String> {
    vmlib::schema::EdgeHold::ALL
        .iter()
        .map(|p| p.title().to_string())
        .collect()
}

fn hold_index(current: f64) -> isize {
    let hold = vmlib::schema::EdgeHold::from_toml(current);
    vmlib::schema::EdgeHold::ALL
        .iter()
        .position(|p| *p == hold)
        .unwrap_or(0) as isize
}

/// The chosen preset's duration, or `current` if the index is somehow out of range.
fn hold_from_index(index: isize, current: f64) -> f64 {
    usize::try_from(index)
        .ok()
        .and_then(|i| vmlib::schema::EdgeHold::ALL.get(i).copied())
        .map_or(current, |p| p.seconds())
}

/// Popup titles for the display mode, in a fixed order the index helpers share.
const DISPLAY_CHOICES: [&str; 3] = ["Match Host", "Dynamic", "Fixed Size"];
/// The `DISPLAY_CHOICES` index of "Fixed Size" (gates the resolution popup).
const DISPLAY_FIXED_INDEX: isize = 2;

fn display_index(resolution: vmlib::schema::DisplayResolution) -> isize {
    match resolution {
        vmlib::schema::DisplayResolution::Host => 0,
        vmlib::schema::DisplayResolution::Dynamic => 1,
        vmlib::schema::DisplayResolution::Fixed(..) => DISPLAY_FIXED_INDEX,
    }
}

/// The point resolutions this Mac's main display offers (the list System Settings
/// shows), deduped and sorted large→small — candidates for a Fixed Size guest.
/// Guest resolutions are point-sized just like Match Host's pushes, so the Mac's
/// own mode list is the natural menu.
fn mac_display_points() -> Vec<(u32, u32)> {
    use std::ffi::c_void;
    extern "C" {
        fn CGMainDisplayID() -> u32;
        fn CGDisplayCopyAllDisplayModes(display: u32, options: *const c_void) -> *mut c_void;
        fn CFArrayGetCount(array: *const c_void) -> isize;
        fn CFArrayGetValueAtIndex(array: *const c_void, index: isize) -> *const c_void;
        fn CGDisplayModeGetWidth(mode: *const c_void) -> usize;
        fn CGDisplayModeGetHeight(mode: *const c_void) -> usize;
        fn CGDisplayModeIsUsableForDesktopGUI(mode: *const c_void) -> bool;
        fn CFRelease(cf: *const c_void);
    }
    let mut out: Vec<(u32, u32)> = Vec::new();
    // SAFETY: CFArray of CGDisplayModeRef per CGDisplayCopyAllDisplayModes' contract;
    // the array is ours to release, its elements are borrowed from it.
    unsafe {
        let modes = CGDisplayCopyAllDisplayModes(CGMainDisplayID(), std::ptr::null());
        if !modes.is_null() {
            for i in 0..CFArrayGetCount(modes) {
                let mode = CFArrayGetValueAtIndex(modes, i);
                if mode.is_null() || !CGDisplayModeIsUsableForDesktopGUI(mode) {
                    continue;
                }
                let wh = (
                    CGDisplayModeGetWidth(mode) as u32,
                    CGDisplayModeGetHeight(mode) as u32,
                );
                if wh.0 >= 64 && wh.1 >= 64 && !out.contains(&wh) {
                    out.push(wh);
                }
            }
            CFRelease(modes);
        }
    }
    if out.is_empty() {
        // Headless or CG failure: still offer something sane.
        out = vec![(1920, 1080), (1280, 800)];
    }
    out.sort_by_key(|&(w, h)| std::cmp::Reverse(u64::from(w) * u64::from(h)));
    out
}

/// The main display's current point size (what Match Host would drive the guest to).
fn main_screen_points() -> (u32, u32) {
    extern "C" {
        fn CGMainDisplayID() -> u32;
        fn CGDisplayBounds(display: u32) -> NSRect;
    }
    // SAFETY: plain value-returning CG calls.
    let b = unsafe { CGDisplayBounds(CGMainDisplayID()) };
    (b.size.width as u32, b.size.height as u32)
}

impl CenterController {
    /// A "?" at the end of the row at `y`: hovering shows the explanation as a
    /// (fast — see the `NSInitialToolTipDelay` registration) tooltip on either the
    /// button or the control, and clicking shows the same text immediately in a
    /// popover (`configureHelpClicked:`).
    fn row_help(&self, parent: &NSView, y: f64, control: &NSView, text: &str) {
        let mtm = self.mtm();
        let tip = NSString::from_str(text);
        control.setToolTip(Some(&tip));
        let b = NSButton::initWithFrame(NSButton::alloc(mtm), rect(298.0, y - 1.0, 23.0, 25.0));
        b.setTitle(&NSString::from_str(""));
        b.setBezelStyle(NSBezelStyle::HelpButton);
        b.setToolTip(Some(&tip));
        // SAFETY: standard target/action wiring; self outlives the modal sheet.
        unsafe {
            b.setTarget(Some(self.as_ref()));
            b.setAction(Some(sel!(configureHelpClicked:)));
        }
        parent.addSubview(&b);
    }
}

/// A `Label:` + popup pair placed at `y` inside the accessory view.
fn labeled_popup(
    mtm: MainThreadMarker,
    parent: &NSView,
    y: f64,
    label: &str,
    choices: &[&str],
    selected: isize,
) -> Retained<NSPopUpButton> {
    let l = NSTextField::labelWithString(&NSString::from_str(label), mtm);
    l.setFrame(rect(0.0, y + 3.0, 84.0, 18.0));
    parent.addSubview(&l);
    let p = NSPopUpButton::initWithFrame_pullsDown(
        NSPopUpButton::alloc(mtm),
        rect(88.0, y - 2.0, 204.0, 26.0),
        false,
    );
    for title in choices {
        p.addItemWithTitle(&NSString::from_str(title));
    }
    p.selectItemAtIndex(selected);
    parent.addSubview(&p);
    p
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notch_popup_round_trips_both_policies() {
        for policy in [
            vmlib::schema::NotchPolicy::Avoid,
            vmlib::schema::NotchPolicy::Extend,
        ] {
            assert_eq!(notch_from_index(notch_index(policy)), policy);
        }
    }

    #[test]
    fn an_unknown_notch_index_falls_back_to_avoid() {
        // Defensive: AppKit returns -1 for "no selection".
        assert_eq!(
            notch_from_index(-1),
            vmlib::schema::NotchPolicy::Avoid,
            "the safe default is the one that matches pre-notch-option behavior"
        );
    }

    #[test]
    fn the_hold_popup_round_trips_every_preset() {
        for preset in vmlib::schema::EdgeHold::ALL {
            let secs = preset.seconds();
            assert_eq!(hold_from_index(hold_index(secs), secs), secs, "{preset:?}");
        }
        assert_eq!(hold_titles().len(), vmlib::schema::EdgeHold::ALL.len());
    }

    #[test]
    fn a_pre_migration_points_value_opens_on_its_migrated_preset() {
        // A vm.toml still carrying the old points encoding must show the preset it maps to, not
        // fall back to Off — silently disabling a feature someone configured is the worse
        // failure, and Off is index 0, which is where an unrecognised value would otherwise land.
        assert_eq!(hold_index(100.0), 2, "old Standard (100 pt) is Standard");
        assert_eq!(hold_index(50.0), 1);
        assert_eq!(hold_index(200.0), 3);
        assert_eq!(
            hold_index(137.0),
            2,
            "an unrecognised old value lands on Standard"
        );
        // And saving that sheet writes the duration, retiring the points value for good.
        assert_eq!(
            hold_from_index(hold_index(137.0), 137.0),
            vmlib::schema::EdgeHold::Standard.seconds()
        );
    }

    #[test]
    fn picking_off_turns_the_grab_off() {
        assert_eq!(hold_from_index(0, 0.30), 0.0);
    }
}
