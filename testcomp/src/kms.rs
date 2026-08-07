// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva

//! KMS: pick an output, import dmabufs as framebuffers, and drive the CRTC.
//!
//! We drive KMS ourselves rather than through smithay's `DrmCompositor`. That is not
//! minimalism — it is what makes this vehicle useful. `DrmCompositor` decides for itself when
//! a buffer is still referenced by a scanout, and those decisions are precisely the behaviour
//! under test; owning the flip means the release order is ours to vary deliberately.
//!
//! Master matters: without DRM master the framebuffer calls succeed and `set_crtc` fails, so
//! the run must own the tty (`systemctl isolate multi-user.target`, then run as root). There
//! is no seat management here on purpose — nothing in this vehicle reads input yet.

use anyhow::{bail, Context, Result};
use drm::buffer::{DrmFourcc, DrmModifier, Handle as BufferHandle, PlanarBuffer};
use drm::control::{connector, crtc, framebuffer, Device as ControlDevice, FbCmd2Flags, Mode};
use std::fs::{File, OpenOptions};
use std::os::fd::{AsFd, BorrowedFd};

/// A DRM card, wrapped so the `drm` crate's blanket trait impls apply.
pub struct Card(File);

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}
impl drm::Device for Card {}
impl ControlDevice for Card {}

/// The output we drive: one connected connector, a CRTC that can reach it, and its mode.
pub struct Output {
    pub card: Card,
    pub connector: connector::Handle,
    pub crtc: crtc::Handle,
    pub mode: Mode,
}

impl Output {
    /// Open `path` and pick the first connected connector that has a mode, plus a CRTC able to
    /// drive it.
    pub fn open(path: &str) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("open {path}"))?;
        let card = Card(file);

        let res = card.resource_handles().context("drmModeGetResources")?;

        for &handle in &res.connectors {
            let info = match card.get_connector(handle, true) {
                Ok(i) => i,
                Err(e) => {
                    log::debug!("connector {handle:?}: {e}");
                    continue;
                }
            };
            if info.state() != connector::State::Connected {
                continue;
            }
            let Some(&mode) = info.modes().first() else {
                continue;
            };

            // Prefer the CRTC already routed to this connector — taking it avoids a needless
            // re-route — and otherwise take the first CRTC any of its encoders can reach.
            let mut chosen = info
                .current_encoder()
                .and_then(|e| card.get_encoder(e).ok())
                .and_then(|e| e.crtc());
            if chosen.is_none() {
                for &enc in info.encoders() {
                    let Ok(enc) = card.get_encoder(enc) else {
                        continue;
                    };
                    if let Some(&c) = res.filter_crtcs(enc.possible_crtcs()).first() {
                        chosen = Some(c);
                        break;
                    }
                }
            }
            let Some(crtc) = chosen else { continue };

            let (w, h) = mode.size();
            log::info!(
                "output: connector {:?} crtc {crtc:?} mode {w}x{h}@{}",
                info.interface(),
                mode.vrefresh()
            );
            return Ok(Output {
                card,
                connector: handle,
                crtc,
                mode,
            });
        }
        bail!("no connected connector with a mode on {path}")
    }

    pub fn size(&self) -> (u32, u32) {
        let (w, h) = self.mode.size();
        (w as u32, h as u32)
    }

    /// Import a dmabuf fd as a GEM handle and add it as a framebuffer.
    ///
    /// The fd is only *borrowed*: `drmPrimeFDToHandle` takes its own reference, so the caller
    /// still owns and must close it. The returned handle, by contrast, is a reference this
    /// process now holds and must drop in [`Fb::release`] — leaking it would keep the buffer
    /// alive guest-side and quietly confound every retention measurement this vehicle exists
    /// to make.
    pub fn import(&self, fd: BorrowedFd<'_>, desc: FbDesc) -> Result<Fb> {
        let prime = drm_ffi::gem::fd_to_handle(self.card.as_fd(), fd)
            .context("drmPrimeFDToHandle — is this fd really a dmabuf?")?;
        let gem = prime.handle;

        let planar = PlanarFb { desc, gem };
        let fb = match self
            .card
            .add_planar_framebuffer(&planar, FbCmd2Flags::MODIFIERS)
        {
            Ok(fb) => fb,
            Err(e) => {
                // Drop the GEM reference we just took; otherwise a failed import leaks a
                // buffer per attempt and the next measurement inherits the debt.
                let _ = drm_ffi::gem::close(self.card.as_fd(), gem);
                return Err(e).with_context(|| {
                    format!(
                        "drmModeAddFB2WithModifiers {}x{} stride={} mod={:#x}",
                        desc.width, desc.height, desc.stride, desc.modifier
                    )
                });
            }
        };
        Ok(Fb { fb, gem })
    }

    /// Scan out `fb` on our CRTC at the connector's mode.
    pub fn set_crtc(&self, fb: &Fb) -> Result<()> {
        self.card
            .set_crtc(
                self.crtc,
                Some(fb.fb),
                (0, 0),
                &[self.connector],
                Some(self.mode),
            )
            .context("drmModeSetCrtc — does this process hold DRM master?")
    }

    /// Drop every reference this process holds to `fb`: the framebuffer first, then the GEM
    /// handle. Order is deliberate and is part of what the vehicle measures.
    pub fn release(&self, fb: Fb) {
        if let Err(e) = self.card.destroy_framebuffer(fb.fb) {
            log::warn!("drmModeRmFB {:?}: {e}", fb.fb);
        }
        if let Err(e) = drm_ffi::gem::close(self.card.as_fd(), fb.gem) {
            log::warn!("drmCloseBufferHandle {}: {e}", fb.gem);
        }
    }
}

/// The layout of an imported buffer, as reported by its allocator — never computed here. A
/// `width * 4` pitch is right until the first driver that pads, and then it is silently wrong.
#[derive(Clone, Copy)]
pub struct FbDesc {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub offset: u32,
    pub modifier: u64,
}

/// A framebuffer plus the GEM reference behind it.
pub struct Fb {
    fb: framebuffer::Handle,
    gem: u32,
}

/// Adapter for `add_planar_framebuffer`, which describes buffers through this trait.
struct PlanarFb {
    desc: FbDesc,
    gem: u32,
}

impl PlanarBuffer for PlanarFb {
    fn size(&self) -> (u32, u32) {
        (self.desc.width, self.desc.height)
    }
    /// `XRGB8888` rather than `ARGB8888`: the alpha of a scanout buffer is not composited by
    /// anything, and declaring it opaque keeps the plane on the simplest path.
    fn format(&self) -> DrmFourcc {
        DrmFourcc::Xrgb8888
    }
    fn modifier(&self) -> Option<DrmModifier> {
        Some(DrmModifier::from(self.desc.modifier))
    }
    fn pitches(&self) -> [u32; 4] {
        [self.desc.stride, 0, 0, 0]
    }
    fn handles(&self) -> [Option<BufferHandle>; 4] {
        [
            drm::control::RawResourceHandle::new(self.gem).map(BufferHandle::from),
            None,
            None,
            None,
        ]
    }
    fn offsets(&self) -> [u32; 4] {
        [self.desc.offset, 0, 0, 0]
    }
}
