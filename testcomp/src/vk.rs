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

/// Enabled when present, and only affecting the acquire barrier on an imported dmabuf. Kept out
/// of `DEVICE_EXTENSIONS` deliberately: making it a fifth hard requirement would turn a *better*
/// ownership transfer into a reason the vehicle refuses to start on a driver that is otherwise
/// perfectly able to run every test here.
const OPTIONAL_EXTENSIONS: [&CStr; 1] = [ash::ext::queue_family_foreign::NAME];

pub struct Vk {
    pub device: ash::Device,
    pub queue: vk::Queue,
    queue_family: u32,
    external_memory_fd: ash::khr::external_memory_fd::Device,
    /// Whether `VK_EXT_queue_family_foreign` was enabled; see `OPTIONAL_EXTENSIONS`.
    queue_family_foreign: bool,
    command_pool: vk::CommandPool,
    command_buffer: vk::CommandBuffer,
    fence: vk::Fence,
    /// Cached so staging allocations do not re-query the physical device per upload.
    memory_types: Vec<vk::MemoryType>,
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

        let available = unsafe { instance.enumerate_device_extension_properties(physical) }
            .context("enumerate device extensions")?;
        let has = |name: &CStr| {
            available
                .iter()
                .any(|e| unsafe { CStr::from_ptr(e.extension_name.as_ptr()) } == name)
        };
        let queue_family_foreign = has(ash::ext::queue_family_foreign::NAME);
        let mut ext_ptrs: Vec<_> = DEVICE_EXTENSIONS.iter().map(|e| e.as_ptr()).collect();
        for name in OPTIONAL_EXTENSIONS {
            if has(name) {
                ext_ptrs.push(name.as_ptr());
            } else {
                log::info!("optional device extension absent: {}", name.to_string_lossy());
            }
        }
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

        let mem_props = unsafe { instance.get_physical_device_memory_properties(physical) };
        let memory_types =
            mem_props.memory_types[..mem_props.memory_type_count as usize].to_vec();

        Ok(Vk {
            device,
            queue,
            queue_family,
            external_memory_fd,
            queue_family_foreign,
            command_pool,
            command_buffer,
            fence,
            memory_types,
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
        self.record(|cbuf| unsafe {
            let d = &self.device;
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
                cbuf,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[barrier],
            );

            let clear = vk::ClearColorValue { float32: colour };
            d.cmd_clear_color_image(cbuf, img.image, vk::ImageLayout::GENERAL, &clear, &[range]);
        })
    }

    /// Record `f` into the single command buffer, submit it, and wait for it to retire.
    ///
    /// Waits on a fence rather than `vkQueueWaitIdle`: the wait must be scoped to *this*
    /// submission, since an idle-wait would also mask a stall belonging to another one. The
    /// whole vehicle is serial, so one command buffer reused under a fence is enough — and it
    /// means every GPU write is complete before the flip that shows it.
    fn record(&self, f: impl FnOnce(vk::CommandBuffer)) -> Result<()> {
        let d = &self.device;
        unsafe {
            d.reset_command_buffer(self.command_buffer, vk::CommandBufferResetFlags::empty())?;
            let begin = vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
            d.begin_command_buffer(self.command_buffer, &begin)?;
            f(self.command_buffer);
            d.end_command_buffer(self.command_buffer)?;

            let submit = vk::SubmitInfo::default()
                .command_buffers(std::slice::from_ref(&self.command_buffer));
            d.queue_submit(self.queue, &[submit], self.fence)?;
            d.wait_for_fences(&[self.fence], true, 5_000_000_000)
                .context("GPU fence timed out after 5s")?;
            d.reset_fences(&[self.fence])?;
        }
        Ok(())
    }

    /// Composite one client's CPU-side pixels into a scanout image at `(dst_x, dst_y)`.
    ///
    /// This is `wl_shm`: the client's pixels live in shared *host* memory, so they have to be
    /// staged through a host-visible buffer and copied on the GPU. A single
    /// `vkCmdCopyBufferToImage` rather than a textured quad — M2 has no scaling, rotation or
    /// blending to do, and a pipeline would be machinery whose failures we would then have to
    /// rule out when something downstream looks wrong.
    ///
    /// `src_stride` is the client's row pitch and is honoured rather than recomputed, for the
    /// same reason M1 takes its pitch from `vkGetImageSubresourceLayout`: a client is free to
    /// pad its rows, and `width * 4` is right until one does.
    pub fn upload(
        &self,
        dst: &ScanoutImage,
        pixels: &[u8],
        width: u32,
        height: u32,
        src_stride: u32,
        dst_x: i32,
        dst_y: i32,
    ) -> Result<()> {
        if width == 0 || height == 0 {
            return Ok(());
        }
        let size = (src_stride as u64) * (height as u64);
        let staging = self.staging_buffer(size)?;
        unsafe {
            let ptr = self
                .device
                .map_memory(staging.memory, 0, size, vk::MemoryMapFlags::empty())
                .context("map staging buffer")? as *mut u8;
            let n = (size as usize).min(pixels.len());
            std::ptr::copy_nonoverlapping(pixels.as_ptr(), ptr, n);
            self.device.unmap_memory(staging.memory);
        }

        let result = self.record(|cbuf| unsafe {
            let d = &self.device;
            let copy = vk::BufferImageCopy::default()
                .buffer_offset(0)
                // In texels, not bytes — this is where a padded client stride would otherwise
                // shear the image one row at a time.
                .buffer_row_length(src_stride / 4)
                .buffer_image_height(height)
                .image_subresource(
                    vk::ImageSubresourceLayers::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .layer_count(1),
                )
                .image_offset(vk::Offset3D { x: dst_x, y: dst_y, z: 0 })
                .image_extent(vk::Extent3D { width, height, depth: 1 });
            d.cmd_copy_buffer_to_image(
                cbuf,
                staging.buffer,
                dst.image,
                vk::ImageLayout::GENERAL,
                &[copy],
            );
        });

        unsafe {
            self.device.destroy_buffer(staging.buffer, None);
            self.device.free_memory(staging.memory, None);
        }
        result
    }

    /// Import a client's dmabuf as a `VkImage` this device can read.
    ///
    /// **This is the path M3 exists to exercise.** On a limina guest the fd names a venus blob,
    /// so the import crosses from the client's virtio-gpu context into the compositor's, and the
    /// host resolves it in `vkr_create_device_memory` (`third_party/virglrenderer/src/venus/
    /// vkr_device_memory.c:319`) by looking the exporter's resource up in *this* context's table
    /// and aliasing its bytes. When the exporter's resource carries an IOSurface, the host takes
    /// a **borrowed `+1`** on it into `mem->imported_iosurface` (`:794`), released only by
    /// `vkr_device_memory_release` (`:981`). That reference is the host-side holder the whole
    /// `buffer-lifetime-matrix.md` is written about.
    ///
    /// The host has a **silent fallback ladder** underneath that (IOSurface → `map_ptr` →
    /// cross-context SHM, `:320-345`): if the IOSurface lookup fails, the import still succeeds,
    /// the pixels are still correct, and no `+1` exists. Correct pixels therefore do **not**
    /// prove the holder was armed — see `README.md` on gating M3a on the host's mechanism line
    /// as well as on the capture.
    ///
    /// Transcribed from synoik's `niri-vk/src/dmabuf.rs::ImportedImage::import`, with the usage
    /// changed from `SAMPLED` to `TRANSFER_SRC` (we copy rather than sample) and the acquire
    /// barrier's destination stage moved to match.
    pub fn import_dmabuf(&self, fd: std::os::fd::BorrowedFd<'_>, desc: &Imported) -> Result<ImportedImage> {
        use std::os::fd::IntoRawFd;

        // The explicit layout, not a list: an import must describe the memory it is given, and
        // a guessed pitch would shear the image one row at a time.
        let plane_layout = vk::SubresourceLayout {
            offset: desc.offset as u64,
            size: 0,
            row_pitch: desc.stride as u64,
            array_pitch: 0,
            depth_pitch: 0,
        };
        let mut mod_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
            .drm_format_modifier(desc.modifier)
            .plane_layouts(std::slice::from_ref(&plane_layout));
        let mut ext_info = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let image_ci = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(SCANOUT_FORMAT)
            .extent(vk::Extent3D { width: desc.width, height: desc.height, depth: 1 })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut ext_info)
            .push_next(&mut mod_info);
        let image = unsafe { self.device.create_image(&image_ci, None) }
            .context("vkCreateImage for the imported dmabuf")?;

        let req = unsafe { self.device.get_image_memory_requirements(image) };
        let mem_type = self.dmabuf_memory_type(fd, req)?;

        // vkAllocateMemory *consumes* the fd on success, so hand it a dup and let the caller keep
        // owning theirs. Without this the compositor's import would close a client's fd out from
        // under smithay's `Dmabuf`, and the second import of the same buffer would fail on EBADF.
        let raw_fd = fd
            .try_clone_to_owned()
            .context("dup the client's dmabuf fd for import")?
            .into_raw_fd();
        let mut import_info = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            .fd(raw_fd);
        // Dedicated, like the export side: without it venus suballocates guest-side and the host
        // import path — the thing under test — is never reached.
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let alloc_ci = vk::MemoryAllocateInfo::default()
            .allocation_size(req.size)
            .memory_type_index(mem_type)
            .push_next(&mut import_info)
            .push_next(&mut dedicated);
        let memory = match unsafe { self.device.allocate_memory(&alloc_ci, None) } {
            Ok(m) => m,
            Err(e) => {
                // Vulkan only takes the fd on success, so on failure it is still ours to close —
                // and a client that can make this fail (M3b path 3) would otherwise leak one fd
                // per attempt in the compositor.
                unsafe { libc::close(raw_fd) };
                unsafe { self.device.destroy_image(image, None) };
                return Err(e).context("vkAllocateMemory importing the client's dmabuf");
            }
        };
        unsafe { self.device.bind_image_memory(image, memory, 0) }
            .context("bind the imported dmabuf memory")?;

        // Acquire: the producer wrote these bytes outside this device, so ownership transfers
        // from the FOREIGN queue family when the extension is there and is a plain transition
        // when it is not. `UNDEFINED` as the old layout is safe *because the modifier is LINEAR*
        // — there is no tiling to preserve, so the bytes survive.
        let (src_qf, dst_qf) = if self.queue_family_foreign {
            (vk::QUEUE_FAMILY_FOREIGN_EXT, self.queue_family)
        } else {
            (vk::QUEUE_FAMILY_IGNORED, vk::QUEUE_FAMILY_IGNORED)
        };
        self.record(|cbuf| unsafe {
            let barrier = vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .src_queue_family_index(src_qf)
                .dst_queue_family_index(dst_qf)
                .image(image)
                .subresource_range(
                    vk::ImageSubresourceRange::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .level_count(1)
                        .layer_count(1),
                );
            self.device.cmd_pipeline_barrier(
                cbuf,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[barrier],
            );
        })
        .context("acquire the imported image")?;

        Ok(ImportedImage { image, memory, width: desc.width, height: desc.height })
    }

    /// The memory type an imported dmabuf can actually use: what the *image* accepts intersected
    /// with what the *fd* accepts. Taking either alone picks an unimportable type on some
    /// drivers, and the failure surfaces as an opaque `ERROR_INVALID_EXTERNAL_HANDLE`.
    fn dmabuf_memory_type(
        &self,
        fd: std::os::fd::BorrowedFd<'_>,
        req: vk::MemoryRequirements,
    ) -> Result<u32> {
        use std::os::fd::AsRawFd;
        let mut fd_props = vk::MemoryFdPropertiesKHR::default();
        // Borrows the fd, does not consume it (unlike the import that follows) — duping here
        // would leak one fd per import.
        unsafe {
            self.external_memory_fd.get_memory_fd_properties(
                vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
                fd.as_raw_fd(),
                &mut fd_props,
            )
        }
        .context("vkGetMemoryFdPropertiesKHR")?;

        let bits = req.memory_type_bits & fd_props.memory_type_bits;
        anyhow::ensure!(
            bits != 0,
            "no importable memory type for the client's dmabuf: image accepts {:#x}, \
             fd accepts {:#x}",
            req.memory_type_bits,
            fd_props.memory_type_bits,
        );
        Ok(bits.trailing_zeros())
    }

    /// Composite an imported client image into the scanout at `(dst_x, dst_y)`, GPU to GPU.
    ///
    /// The dmabuf counterpart of `upload`: no staging buffer and no host round-trip, because the
    /// client's pixels are already in a buffer this device can read. Source and destination are
    /// both `B8G8R8A8_UNORM` and both LINEAR, so a straight `vkCmdCopyImage` is correct — a blit
    /// would only add a filter we do not want and a format conversion we do not need.
    ///
    /// Takes the source as plain handles rather than an `&ImportedImage` so the caller can hold
    /// its import cache mutably while recording the copy.
    pub fn copy_image(
        &self,
        dst: &ScanoutImage,
        src: vk::Image,
        src_width: u32,
        src_height: u32,
        dst_x: i32,
        dst_y: i32,
    ) -> Result<()> {
        // Clip to the scanout: a client is free to be larger than the display, and an
        // out-of-bounds copy region is undefined behaviour rather than a clipped one.
        let width = src_width.min(dst.width.saturating_sub(dst_x.max(0) as u32));
        let height = src_height.min(dst.height.saturating_sub(dst_y.max(0) as u32));
        if width == 0 || height == 0 {
            return Ok(());
        }
        self.record(|cbuf| unsafe {
            let layers = vk::ImageSubresourceLayers::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .layer_count(1);
            let region = vk::ImageCopy::default()
                .src_subresource(layers)
                .src_offset(vk::Offset3D::default())
                .dst_subresource(layers)
                .dst_offset(vk::Offset3D { x: dst_x, y: dst_y, z: 0 })
                .extent(vk::Extent3D { width, height, depth: 1 });
            self.device.cmd_copy_image(
                cbuf,
                src,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                dst.image,
                vk::ImageLayout::GENERAL,
                &[region],
            );
        })
    }

    /// Release an imported image. The guest side must provably let go before any host-side
    /// retention number means anything — the same rule M1's churn loop lives under.
    pub fn destroy_imported(&self, img: &ImportedImage) {
        unsafe {
            self.device.destroy_image(img.image, None);
            self.device.free_memory(img.memory, None);
        }
    }

    /// A host-visible, coherent staging buffer. Allocated per upload and freed immediately:
    /// a pool would be the obvious optimisation, and is exactly the kind of caching whose
    /// lifetime bugs this vehicle exists to find in *other* code — so it stays out of here
    /// until a measurement asks for it.
    fn staging_buffer(&self, size: u64) -> Result<Staging> {
        let ci = vk::BufferCreateInfo::default()
            .size(size)
            .usage(vk::BufferUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let buffer = unsafe { self.device.create_buffer(&ci, None) }.context("staging buffer")?;
        let req = unsafe { self.device.get_buffer_memory_requirements(buffer) };
        let want = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
        let index = self
            .memory_types
            .iter()
            .enumerate()
            .position(|(i, t)| req.memory_type_bits & (1 << i) != 0 && t.property_flags.contains(want))
            .context("no host-visible coherent memory type")? as u32;
        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(req.size)
            .memory_type_index(index);
        let memory =
            unsafe { self.device.allocate_memory(&alloc, None) }.context("staging memory")?;
        unsafe { self.device.bind_buffer_memory(buffer, memory, 0) }.context("bind staging")?;
        Ok(Staging { buffer, memory })
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

/// A host-visible buffer used to stage `wl_shm` pixels on their way to the GPU.
struct Staging {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
}

/// The layout of a dmabuf being imported — everything `import_dmabuf` needs that the fd itself
/// does not carry. Deliberately plain numbers rather than a smithay `Dmabuf`, so `vk.rs` stays
/// independent of the Wayland layer and the client (which has no smithay) can describe its own
/// buffer with the same type.
pub struct Imported {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub offset: u32,
    pub modifier: u64,
}

/// A client's dmabuf, imported into *our* device. Holds no fd: `import_dmabuf` dups what it needs
/// and Vulkan owns that dup, so the only thing to release here is the Vulkan pair.
pub struct ImportedImage {
    pub image: vk::Image,
    pub memory: vk::DeviceMemory,
    pub width: u32,
    pub height: u32,
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
