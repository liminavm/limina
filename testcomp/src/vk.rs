// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva

//! Vulkan device setup and scanout-image allocation.
//!
//! The allocation sequence is the one `crates/limina-test/guest/kmschurn.py` proved against
//! venus (its `-vk` arm); this is that sequence in Rust. It is deliberately *not* gbm: a gbm
//! buffer under zink costs two exportable `VkImage`s (zink's linear scanout shadow), which
//! doubles the host-side IOSurface count and confounds every retention measurement. Allocating
//! the scanout image directly gives one host surface per buffer.
//!
//! Sequence, in order, each step load-bearing:
//!   1. `vkCreateImage` with `DRM_FORMAT_MODIFIER` tiling and a modifier *list* of `[LINEAR]`
//!      (venus exposes only `0x0` for these formats), chained with
//!      `VkExternalMemoryImageCreateInfo{handleTypes = DMA_BUF}`.
//!   2. `vkAllocateMemory`, both **dedicated** and **exported**. Without both, venus
//!      suballocates guest-side and the host allocator — the thing under test — is never
//!      reached.
//!   3. `vkGetMemoryFdKHR(DMA_BUF)` for a dmabuf fd (a prime export of the venus blob GEM).
//!   4. `vkGetImageSubresourceLayout` with the **`MEMORY_PLANE_0`** aspect, not `COLOR`: under
//!      modifier tiling `COLOR` is invalid, and the returned `rowPitch` is the only honest
//!      source for the KMS pitch — never compute it as `width * 4`.
//!
//! Ash usage patterns (builder shapes, extension-loader construction, the memory-type
//! intersection) follow synoik's `synoik-vk` crate, which is GPL-3.0-only by the same author;
//! see `README.md` on why that is licence-compatible here and nowhere else in this repository.

use anyhow::{bail, Context, Result};
use ash::vk;
use std::ffi::CStr;

/// `DRM_FORMAT_MOD_LINEAR`. The only modifier venus exposes for our formats, so the whole
/// vehicle is scoped to it — an explicit choice, not an oversight.
pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;

/// DRM `XRGB8888` little-endian memory order is `[B, G, R, X]`, which is
/// `VK_FORMAT_B8G8R8A8_UNORM` on the Vulkan side. The pair must agree or KMS scans out
/// channel-swapped pixels that still "work".
const SCANOUT_FORMAT: vk::Format = vk::Format::B8G8R8A8_UNORM;

/// Device extensions the scanout path cannot be built without; all four are required, so a
/// missing one is a hard failure rather than a degraded mode.
const DEVICE_EXTENSIONS: [&CStr; 4] = [
    ash::khr::external_memory::NAME,
    ash::khr::external_memory_fd::NAME,
    ash::ext::external_memory_dma_buf::NAME,
    ash::ext::image_drm_format_modifier::NAME,
];

pub struct Vk {
    pub device: ash::Device,
    pub queue: vk::Queue,
    external_memory_fd: ash::khr::external_memory_fd::Device,
    command_pool: vk::CommandPool,
    command_buffer: vk::CommandBuffer,
    fence: vk::Fence,
    // Dropped last: `ash::Device` and the extension loaders borrow nothing from these, but the
    // Vulkan objects above are only valid while the instance lives.
    instance: ash::Instance,
    _entry: ash::Entry,
}

impl Vk {
    /// Open the device whose driver name contains `want` (`"Venus"` in a limina guest).
    ///
    /// Failing loudly when no such device exists is the point: if the venus ICD was not
    /// selected, some *other* driver would otherwise be picked silently and every subsequent
    /// measurement would describe the wrong stack. `VK_DRIVER_FILES` is the knob.
    pub fn new(want: &str) -> Result<Self> {
        let entry = unsafe { ash::Entry::load() }.context("load the Vulkan loader")?;

        let app_info = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_1);
        let instance_ci = vk::InstanceCreateInfo::default().application_info(&app_info);
        let instance =
            unsafe { entry.create_instance(&instance_ci, None) }.context("vkCreateInstance")?;

        let physical = Self::pick_physical(&instance, want)?;

        // One graphics queue is all this needs; the clear that dirties each buffer is the only
        // GPU work, and it is submitted serially.
        let queue_family = unsafe { instance.get_physical_device_queue_family_properties(physical) }
            .iter()
            .position(|p| p.queue_flags.contains(vk::QueueFlags::GRAPHICS))
            .context("no graphics queue family")? as u32;

        let priorities = [1.0f32];
        let queue_ci = vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family)
            .queue_priorities(&priorities);
        let ext_ptrs: Vec<_> = DEVICE_EXTENSIONS.iter().map(|e| e.as_ptr()).collect();
        let device_ci = vk::DeviceCreateInfo::default()
            .queue_create_infos(std::slice::from_ref(&queue_ci))
            .enabled_extension_names(&ext_ptrs);
        let device = unsafe { instance.create_device(physical, &device_ci, None) }
            .context("vkCreateDevice — is every scanout extension present?")?;

        let queue = unsafe { device.get_device_queue(queue_family, 0) };
        let external_memory_fd = ash::khr::external_memory_fd::Device::new(&instance, &device);

        let pool_ci = vk::CommandPoolCreateInfo::default()
            .queue_family_index(queue_family)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        let command_pool =
            unsafe { device.create_command_pool(&pool_ci, None) }.context("command pool")?;
        let alloc = vk::CommandBufferAllocateInfo::default()
            .command_pool(command_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        let command_buffer = unsafe { device.allocate_command_buffers(&alloc) }
            .context("command buffer")?[0];
        let fence = unsafe { device.create_fence(&vk::FenceCreateInfo::default(), None) }
            .context("fence")?;

        Ok(Vk {
            device,
            queue,
            external_memory_fd,
            command_pool,
            command_buffer,
            fence,
            instance,
            _entry: entry,
        })
    }

    fn pick_physical(instance: &ash::Instance, want: &str) -> Result<vk::PhysicalDevice> {
        let devices =
            unsafe { instance.enumerate_physical_devices() }.context("enumerate physical devices")?;
        let mut seen = Vec::new();
        for pd in devices {
            let props = unsafe { instance.get_physical_device_properties(pd) };
            let name = unsafe { CStr::from_ptr(props.device_name.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            if name.contains(want) {
                log::info!("vulkan device: {name}");
                return Ok(pd);
            }
            seen.push(name);
        }
        bail!(
            "no Vulkan device matching {want:?} (saw: {seen:?}). \
             In a limina guest set VK_DRIVER_FILES to the virtio ICD — picking whatever \
             device happens to be first would measure the wrong stack."
        )
    }

    /// Allocate one scanout image: modifier-tiled, dedicated, exported, and dirtied once.
    ///
    /// The returned dmabuf fd is owned by the caller, who imports it into KMS and closes it.
    pub fn scanout_image(&self, width: u32, height: u32) -> Result<ScanoutImage> {
        let modifiers = [DRM_FORMAT_MOD_LINEAR];
        let mut modifier_list = vk::ImageDrmFormatModifierListCreateInfoEXT::default()
            .drm_format_modifiers(&modifiers);
        let mut external = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let image_ci = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(SCANOUT_FORMAT)
            .extent(vk::Extent3D { width, height, depth: 1 })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut external)
            .push_next(&mut modifier_list);
        let image = unsafe { self.device.create_image(&image_ci, None) }
            .with_context(|| format!("vkCreateImage {width}x{height}"))?;

        let req = unsafe { self.device.get_image_memory_requirements(image) };
        let mut export = vk::ExportMemoryAllocateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let alloc_ci = vk::MemoryAllocateInfo::default()
            .allocation_size(req.size)
            .memory_type_index(req.memory_type_bits.trailing_zeros())
            .push_next(&mut dedicated)
            .push_next(&mut export);
        let memory = unsafe { self.device.allocate_memory(&alloc_ci, None) }
            .with_context(|| format!("vkAllocateMemory size={}", req.size))?;
        unsafe { self.device.bind_image_memory(image, memory, 0) }.context("bind scanout memory")?;

        let get_fd = vk::MemoryGetFdInfoKHR::default()
            .memory(memory)
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let dmabuf = unsafe { self.external_memory_fd.get_memory_fd(&get_fd) }
            .context("vkGetMemoryFdKHR")?;

        // MEMORY_PLANE_0, not COLOR: under modifier tiling the colour aspect is invalid here,
        // and `row_pitch` is the only honest pitch — a computed `width * 4` is a silent
        // corruption waiting for the first driver that pads.
        let subresource = vk::ImageSubresource {
            aspect_mask: vk::ImageAspectFlags::MEMORY_PLANE_0_EXT,
            mip_level: 0,
            array_layer: 0,
        };
        let layout = unsafe { self.device.get_image_subresource_layout(image, subresource) };

        Ok(ScanoutImage {
            image,
            memory,
            dmabuf,
            width,
            height,
            stride: layout.row_pitch as u32,
            offset: layout.offset as u32,
            modifier: DRM_FORMAT_MOD_LINEAR,
        })
    }

    /// Transition a fresh scanout image to `GENERAL` and clear it, so the buffer carries real
    /// GPU work rather than being allocated and dropped untouched.
    ///
    /// Waits on a fence rather than `vkQueueWaitIdle`: the wait must be scoped to *this*
    /// submission, since an idle-wait would also mask a stall belonging to another one.
    pub fn clear(&self, img: &ScanoutImage, colour: [f32; 4]) -> Result<()> {
        let d = &self.device;
        unsafe {
            d.reset_command_buffer(self.command_buffer, vk::CommandBufferResetFlags::empty())?;
            let begin = vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
            d.begin_command_buffer(self.command_buffer, &begin)?;

            let range = vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .level_count(1)
                .layer_count(1);
            let barrier = vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(img.image)
                .subresource_range(range);
            d.cmd_pipeline_barrier(
                self.command_buffer,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[barrier],
            );

            let clear = vk::ClearColorValue { float32: colour };
            d.cmd_clear_color_image(
                self.command_buffer,
                img.image,
                vk::ImageLayout::GENERAL,
                &clear,
                &[range],
            );
            d.end_command_buffer(self.command_buffer)?;

            let submit =
                vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&self.command_buffer));
            d.queue_submit(self.queue, &[submit], self.fence)?;
            d.wait_for_fences(&[self.fence], true, 5_000_000_000)
                .context("clear fence timed out after 5s")?;
            d.reset_fences(&[self.fence])?;
        }
        Ok(())
    }

    /// Release an image's Vulkan objects. Separate from `ScanoutImage::drop` on purpose: a
    /// `Drop` impl would need a device handle inside every buffer, and the release *order*
    /// across KMS and Vulkan is the thing this vehicle measures — it must stay explicit and
    /// visible at the call site.
    pub fn destroy_image(&self, img: &ScanoutImage) {
        unsafe {
            self.device.destroy_image(img.image, None);
            self.device.free_memory(img.memory, None);
        }
    }
}

impl Drop for Vk {
    fn drop(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_fence(self.fence, None);
            self.device.destroy_command_pool(self.command_pool, None);
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

/// One venus-allocated scanout image, exported as a dmabuf.
pub struct ScanoutImage {
    pub image: vk::Image,
    pub memory: vk::DeviceMemory,
    /// Exported dmabuf fd, owned by whoever holds this struct. Raw rather than `OwnedFd`
    /// because it is handed straight to `drmPrimeFDToHandle` and closed immediately after.
    pub dmabuf: i32,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub offset: u32,
    pub modifier: u64,
}
