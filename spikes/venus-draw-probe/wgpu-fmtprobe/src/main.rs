// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// wgpu-fmtprobe — reproduce ghost-ui's wgpu adapter/surface setup to see, on a given Vulkan ICD,
// (1) whether wgpu exposes TEXTURE_FORMAT_16BIT_NORM, and (2) what surface formats are advertised
// and which one ghost-ui's "first non-sRGB else formats[0]" rule lands on. Run headless for (1);
// set PROBE_SURFACE=1 (under a Wayland/X session) for (2). Pick the driver with VK_DRIVER_FILES.
use std::sync::Arc;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

const SIXTEEN_BIT: &[wgpu::TextureFormat] = &[
    wgpu::TextureFormat::R16Unorm,
    wgpu::TextureFormat::R16Snorm,
    wgpu::TextureFormat::Rg16Unorm,
    wgpu::TextureFormat::Rg16Snorm,
    wgpu::TextureFormat::Rgba16Unorm,
    wgpu::TextureFormat::Rgba16Snorm,
];

fn instance() -> wgpu::Instance {
    // Match ghost-ui's instance construction exactly (wgpu 29: new() takes the descriptor by
    // value and InstanceDescriptor is not Default). enumerate/request below filter to Vulkan.
    wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle())
}

#[derive(Default)]
struct App {
    done: bool,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.done {
            return;
        }
        self.done = true;
        let window = Arc::new(
            el.create_window(Window::default_attributes().with_title("wgpu-fmtprobe"))
                .expect("create_window"),
        );
        let instance = instance();
        let surface = instance.create_surface(window.clone()).expect("create_surface");
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
        }))
        .expect("request_adapter");
        let info = adapter.get_info();
        println!(
            "== windowed: adapter {} (driver={:?}, type={:?}) ==",
            info.name, info.driver, info.device_type
        );
        println!(
            "   16BIT_NORM available: {}",
            adapter
                .features()
                .contains(wgpu::Features::TEXTURE_FORMAT_16BIT_NORM)
        );
        let caps = surface.get_capabilities(&adapter);
        println!("   surface formats ({}):", caps.formats.len());
        for f in &caps.formats {
            println!("       {:?}   srgb={}", f, f.is_srgb());
        }
        let chosen = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .unwrap_or(caps.formats[0]);
        println!(">>> ghost-ui rule (first non-srgb, else formats[0]) picks: {:?}", chosen);
        println!(
            ">>> that pick needs the 16BIT_NORM feature: {}",
            SIXTEEN_BIT.contains(&chosen)
        );

        // Decisive test: ENABLE the feature, configure the surface with the chosen (possibly
        // 16-bit) format, and actually render + present one frame through venus->KK->Metal->
        // limina present. Answers "if configured, does it work?" and whether our present path
        // can handle a 16-bit swapchain — which decides drop vs reorder.
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("fmtprobe"),
            required_features: wgpu::Features::TEXTURE_FORMAT_16BIT_NORM,
            ..Default::default()
        }))
        .expect("request_device(16BIT_NORM)");
        let size = window.inner_size();
        let mut config = surface
            .get_default_config(&adapter, size.width.max(64), size.height.max(64))
            .expect("get_default_config");
        config.format = chosen;
        config.usage = wgpu::TextureUsages::RENDER_ATTACHMENT;
        surface.configure(&device, &config);
        println!(">>> configured surface as {:?}; rendering+presenting one frame...", chosen);
        match surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => {
                let view = frame
                    .texture
                    .create_view(&wgpu::TextureViewDescriptor::default());
                let mut enc =
                    device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
                {
                    let _rp = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("clear"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: &view,
                            resolve_target: None,
                            depth_slice: None,
                            ops: wgpu::Operations {
                                load: wgpu::LoadOp::Clear(wgpu::Color {
                                    r: 0.0,
                                    g: 1.0,
                                    b: 0.0,
                                    a: 1.0,
                                }),
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
                frame.present();
                println!(
                    ">>> SUCCESS: rendered + presented a {:?} frame with the feature enabled",
                    chosen
                );
            }
            other => println!(">>> FAIL get_current_texture({:?}): {:?}", chosen, other),
        }
        el.exit();
    }
    fn window_event(&mut self, _el: &ActiveEventLoop, _id: WindowId, _ev: WindowEvent) {}
}

fn main() {
    // Part 1 — headless: enumerate adapters + the 16BIT_NORM feature. No display needed.
    let instance = instance();
    let adapters = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::VULKAN));
    println!("== headless: {} Vulkan adapter(s) ==", adapters.len());
    for a in &adapters {
        let info = a.get_info();
        println!("- {} (driver={:?}, type={:?})", info.name, info.driver, info.device_type);
        println!(
            "    TEXTURE_FORMAT_16BIT_NORM available: {}",
            a.features()
                .contains(wgpu::Features::TEXTURE_FORMAT_16BIT_NORM)
        );
    }

    // Part 2 — windowed: surface formats + ghost-ui's pick. Needs a Wayland/X session.
    if std::env::var("PROBE_SURFACE").is_ok() {
        let el = EventLoop::new().expect("event loop");
        el.set_control_flow(ControlFlow::Poll);
        el.run_app(&mut App::default()).expect("run_app");
    } else {
        println!("(set PROBE_SURFACE=1 under a display session to also dump surface formats)");
    }
}
