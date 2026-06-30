// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Minimal reproduction of a SIGSEGV when a venus (Virtio-GPU Venus) wgpu Vulkan
//! surface + device is torn down inside a libtest `#[test]`. Pure winit + wgpu;
//! no application code.
//!
//! Run each test on its own (a SIGSEGV takes down the whole process, so don't run
//! them together), under the venus ICD (the default when a real Wayland display is
//! present) — observe `signal: 11, SIGSEGV` — and under lavapipe — observe a clean
//! pass:
//!
//! ```sh
//! WAYLAND_DISPLAY=wayland-0 XDG_RUNTIME_DIR=/run/user/$(id -u) \
//!   cargo test --test teardown t1_create_drop -- --nocapture          # venus → SIGSEGV?
//! VK_DRIVER_FILES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json \
//! WAYLAND_DISPLAY=wayland-0 XDG_RUNTIME_DIR=/run/user/$(id -u) \
//!   cargo test --test teardown t1_create_drop -- --nocapture          # lavapipe → ok
//! ```
//!
//! libtest runs each test on a worker thread; the standalone-binary equivalent of
//! any of these tears down cleanly on venus, so the crash needs the libtest harness.

use std::sync::Arc;

use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::platform::wayland::EventLoopBuilderExtWayland;
use winit::window::{Window, WindowId};

/// What the per-test handler should do once the window exists, before it drops the
/// GPU resources (as `resumed`'s locals) and exits the loop.
#[derive(Clone, Copy)]
enum Work {
    /// Create surface + device, configure the swapchain, then drop. No present.
    CreateDrop,
    /// Also acquire one swapchain image, clear it in a render pass, and present.
    PresentOnce,
}

struct Probe {
    work: Work,
    done: bool,
}

impl ApplicationHandler for Probe {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.done {
            return;
        }
        self.done = true;

        let window = Arc::new(
            el.create_window(
                Window::default_attributes().with_inner_size(PhysicalSize::new(800, 600)),
            )
            .expect("create window"),
        );
        let size = window.inner_size();

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let surface = instance
            .create_surface(window.clone())
            .expect("create surface");
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
        }))
        .expect("request adapter");
        eprintln!(
            "adapter: backend={:?} driver={}",
            adapter.get_info().backend,
            adapter.get_info().driver
        );
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
                .expect("request device");
        let caps = surface.get_capabilities(&adapter);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: caps.formats[0],
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
        };
        surface.configure(&device, &config);

        if matches!(self.work, Work::PresentOnce) {
            if let wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) = surface.get_current_texture()
            {
                let view = frame
                    .texture
                    .create_view(&wgpu::TextureViewDescriptor::default());
                let mut enc =
                    device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
                {
                    let _pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: None,
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: &view,
                            depth_slice: None,
                            resolve_target: None,
                            ops: wgpu::Operations {
                                load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                                store: wgpu::StoreOp::Store,
                            },
                        })],
                        depth_stencil_attachment: None,
                        timestamp_writes: None,
                        occlusion_query_set: None,
                        multiview_mask: None,
                    });
                }
                queue.submit([enc.finish()]);
                window.pre_present_notify();
                frame.present();
                eprintln!("presented one frame");
            }
        }

        eprintln!("dropping GPU resources (as resumed's locals) and exiting loop");
        el.exit();
        // `window`, `surface`, `device`, `queue`, `instance` all drop here, as
        // `resumed` returns — inside libtest's worker thread. On venus: SIGSEGV.
    }

    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}

fn run(work: Work) {
    let event_loop = EventLoop::builder()
        .with_any_thread(true)
        .build()
        .expect("event loop");
    let mut probe = Probe { work, done: false };
    event_loop.run_app(&mut probe).expect("run app");
    eprintln!("event loop returned; test body about to return");
}

/// The smallest possible: no winit, no window, no surface — just a headless wgpu
/// Vulkan device, created and dropped inside a libtest `#[test]`. If this SIGSEGVs
/// on venus, the bug is purely wgpu/Vulkan/venus device lifecycle under libtest.
#[test]
fn t0_headless_device_only() {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        force_fallback_adapter: false,
        compatible_surface: None,
    }))
    .expect("request adapter");
    eprintln!(
        "adapter: backend={:?} driver={}",
        adapter.get_info().backend,
        adapter.get_info().driver
    );
    let (_device, _queue) =
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
            .expect("request device");
    eprintln!("created headless device; test body about to return (drops follow)");
}

#[test]
fn t1_create_drop() {
    run(Work::CreateDrop);
}

#[test]
fn t2_present_once() {
    run(Work::PresentOnce);
}
