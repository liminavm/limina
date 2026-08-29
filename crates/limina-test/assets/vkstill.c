// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva
//
// vkstill — a Wayland + Vulkan client that draws the SAME triangle every frame.
//
// It exists to give the L2 snapshot-restore landmark test a Vulkan surface it can actually
// check. The test compares a pre-suspend frame against a post-restore one cell by cell, so
// anything that animates has to be excluded from the comparison — and an excluded region is a
// region the test does not check. vkmark, the only Vulkan client in the guest image, animates
// every scene except `clear`, and `clear` is a flat fill: it catches a surface that came back
// blank, not one that came back subtly wrong.
//
// So: a live present loop (the client keeps submitting, the venus context stays exercised, the
// swapchain keeps rotating) drawing pixels that never change. The frame is deliberately
// detailed — a barycentric gradient under a high-frequency band pattern — so that a cell-mean
// comparison sees structure rather than a smooth ramp that corruption could survive.
//
// Nothing here is limina-specific; it is a plain WSI client. It is compiled IN THE GUEST at
// test time by `vkstill-build.sh` rather than shipped as a binary, so the test needs no
// addition to the guest image and no payload delivery.
//
// Exit codes: 0 never (it runs until killed), 1 on any setup failure with a message on stderr.

#define _POSIX_C_SOURCE 200809L
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <wayland-client.h>
#include "xdg-shell-client-protocol.h"

#define VK_USE_PLATFORM_WAYLAND_KHR
#include <vulkan/vulkan.h>

#include "vkstill-spv.h"

#define MAX_IMAGES 8

static void die(const char *what) {
    fprintf(stderr, "vkstill: %s\n", what);
    exit(1);
}

/// Enumerations are asked twice — count, then fill — and a fixed-size second call is allowed
/// to come back VK_INCOMPLETE, which means "there were more than you asked for", not an error.
/// Venus reports well over 32 surface formats, so treating INCOMPLETE as failure fails on the
/// one driver this program exists to exercise.
static void vk_check_enum(VkResult r, const char *what) {
    if (r != VK_SUCCESS && r != VK_INCOMPLETE) {
        fprintf(stderr, "vkstill: %s failed (VkResult %d)\n", what, (int)r);
        exit(1);
    }
}

static void vk_check(VkResult r, const char *what) {
    if (r != VK_SUCCESS) {
        fprintf(stderr, "vkstill: %s failed (VkResult %d)\n", what, (int)r);
        exit(1);
    }
}

/* ---- Wayland ------------------------------------------------------------- */

struct wl_state {
    struct wl_display *display;
    struct wl_compositor *compositor;
    struct xdg_wm_base *wm_base;
    struct wl_surface *surface;
    struct xdg_surface *xdg_surface;
    struct xdg_toplevel *toplevel;
    bool configured;
    bool closed;
    int32_t width, height;
};

static void wm_base_ping(void *data, struct xdg_wm_base *b, uint32_t serial) {
    (void)data;
    xdg_wm_base_pong(b, serial);
}
static const struct xdg_wm_base_listener wm_base_listener = { .ping = wm_base_ping };

static void registry_global(void *data, struct wl_registry *reg, uint32_t name,
                            const char *iface, uint32_t version) {
    struct wl_state *s = data;
    (void)version;
    if (!strcmp(iface, wl_compositor_interface.name)) {
        s->compositor = wl_registry_bind(reg, name, &wl_compositor_interface, 4);
    } else if (!strcmp(iface, xdg_wm_base_interface.name)) {
        s->wm_base = wl_registry_bind(reg, name, &xdg_wm_base_interface, 1);
        xdg_wm_base_add_listener(s->wm_base, &wm_base_listener, s);
    }
}
static void registry_global_remove(void *data, struct wl_registry *reg, uint32_t name) {
    (void)data; (void)reg; (void)name;
}
static const struct wl_registry_listener registry_listener = {
    .global = registry_global,
    .global_remove = registry_global_remove,
};

static void xdg_surface_configure(void *data, struct xdg_surface *xs, uint32_t serial) {
    struct wl_state *s = data;
    xdg_surface_ack_configure(xs, serial);
    s->configured = true;
}
static const struct xdg_surface_listener xdg_surface_listener = {
    .configure = xdg_surface_configure,
};

static void toplevel_configure(void *data, struct xdg_toplevel *t, int32_t w, int32_t h,
                               struct wl_array *states) {
    struct wl_state *s = data;
    (void)t; (void)states;
    // A zero size means "you choose"; keep whatever we already had.
    if (w > 0 && h > 0) { s->width = w; s->height = h; }
}
static void toplevel_close(void *data, struct xdg_toplevel *t) {
    struct wl_state *s = data;
    (void)t;
    s->closed = true;
}
static const struct xdg_toplevel_listener toplevel_listener = {
    .configure = toplevel_configure,
    .close = toplevel_close,
};

/* ---- Vulkan -------------------------------------------------------------- */

struct vk_state {
    VkInstance instance;
    VkPhysicalDevice phys;
    uint32_t queue_family;
    VkDevice device;
    VkQueue queue;
    VkSurfaceKHR surface;
    VkFormat format;
    VkExtent2D extent;
    VkSwapchainKHR swapchain;
    uint32_t image_count;
    VkImageView views[MAX_IMAGES];
    VkFramebuffer fbs[MAX_IMAGES];
    VkRenderPass pass;
    VkPipelineLayout layout;
    VkPipeline pipeline;
    VkCommandPool pool;
    VkCommandBuffer cmds[MAX_IMAGES];
    VkSemaphore acquired, rendered;
    VkFence in_flight;
};

static VkShaderModule make_shader(VkDevice dev, const uint32_t *code, size_t bytes) {
    VkShaderModuleCreateInfo ci = {
        .sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
        .codeSize = bytes,
        .pCode = code,
    };
    VkShaderModule m;
    vk_check(vkCreateShaderModule(dev, &ci, NULL, &m), "vkCreateShaderModule");
    return m;
}

static void create_swapchain(struct vk_state *vk) {
    VkSurfaceCapabilitiesKHR caps;
    vk_check(vkGetPhysicalDeviceSurfaceCapabilitiesKHR(vk->phys, vk->surface, &caps),
             "vkGetPhysicalDeviceSurfaceCapabilitiesKHR");
    if (caps.currentExtent.width != 0xffffffffu)
        vk->extent = caps.currentExtent;
    if (vk->extent.width == 0 || vk->extent.height == 0)
        vk->extent = (VkExtent2D){ 800, 600 };

    uint32_t want = caps.minImageCount + 1;
    if (caps.maxImageCount && want > caps.maxImageCount) want = caps.maxImageCount;
    if (want > MAX_IMAGES) want = MAX_IMAGES;

    VkSwapchainCreateInfoKHR ci = {
        .sType = VK_STRUCTURE_TYPE_SWAPCHAIN_CREATE_INFO_KHR,
        .surface = vk->surface,
        .minImageCount = want,
        .imageFormat = vk->format,
        .imageColorSpace = VK_COLOR_SPACE_SRGB_NONLINEAR_KHR,
        .imageExtent = vk->extent,
        .imageArrayLayers = 1,
        .imageUsage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT,
        .imageSharingMode = VK_SHARING_MODE_EXCLUSIVE,
        .preTransform = caps.currentTransform,
        .compositeAlpha = VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR,
        // FIFO is always supported and paces us to the compositor: a still picture
        // should not spin the GPU as fast as it will go.
        .presentMode = VK_PRESENT_MODE_FIFO_KHR,
        .clipped = VK_TRUE,
    };
    vk_check(vkCreateSwapchainKHR(vk->device, &ci, NULL, &vk->swapchain), "vkCreateSwapchainKHR");

    VkImage images[MAX_IMAGES];
    vk->image_count = MAX_IMAGES;
    vk_check_enum(vkGetSwapchainImagesKHR(vk->device, vk->swapchain, &vk->image_count, images),
             "vkGetSwapchainImagesKHR");

    for (uint32_t i = 0; i < vk->image_count; i++) {
        VkImageViewCreateInfo vci = {
            .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
            .image = images[i],
            .viewType = VK_IMAGE_VIEW_TYPE_2D,
            .format = vk->format,
            .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 },
        };
        vk_check(vkCreateImageView(vk->device, &vci, NULL, &vk->views[i]), "vkCreateImageView");
        VkFramebufferCreateInfo fci = {
            .sType = VK_STRUCTURE_TYPE_FRAMEBUFFER_CREATE_INFO,
            .renderPass = vk->pass,
            .attachmentCount = 1,
            .pAttachments = &vk->views[i],
            .width = vk->extent.width,
            .height = vk->extent.height,
            .layers = 1,
        };
        vk_check(vkCreateFramebuffer(vk->device, &fci, NULL, &vk->fbs[i]), "vkCreateFramebuffer");
    }
}

static void destroy_swapchain(struct vk_state *vk) {
    for (uint32_t i = 0; i < vk->image_count; i++) {
        vkDestroyFramebuffer(vk->device, vk->fbs[i], NULL);
        vkDestroyImageView(vk->device, vk->views[i], NULL);
    }
    vkDestroySwapchainKHR(vk->device, vk->swapchain, NULL);
    vk->swapchain = VK_NULL_HANDLE;
}

/// Record the one draw. Viewport and scissor are dynamic, so a resize only rebuilds the
/// swapchain and these buffers — never the pipeline.
static void record(struct vk_state *vk, uint32_t i) {
    VkCommandBufferBeginInfo bi = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO };
    vk_check(vkBeginCommandBuffer(vk->cmds[i], &bi), "vkBeginCommandBuffer");
    VkClearValue clear = { .color = { .float32 = { 0.06f, 0.06f, 0.10f, 1.0f } } };
    VkRenderPassBeginInfo rp = {
        .sType = VK_STRUCTURE_TYPE_RENDER_PASS_BEGIN_INFO,
        .renderPass = vk->pass,
        .framebuffer = vk->fbs[i],
        .renderArea = { { 0, 0 }, vk->extent },
        .clearValueCount = 1,
        .pClearValues = &clear,
    };
    vkCmdBeginRenderPass(vk->cmds[i], &rp, VK_SUBPASS_CONTENTS_INLINE);
    VkViewport vp = { 0, 0, (float)vk->extent.width, (float)vk->extent.height, 0.0f, 1.0f };
    VkRect2D sc = { { 0, 0 }, vk->extent };
    vkCmdSetViewport(vk->cmds[i], 0, 1, &vp);
    vkCmdSetScissor(vk->cmds[i], 0, 1, &sc);
    vkCmdBindPipeline(vk->cmds[i], VK_PIPELINE_BIND_POINT_GRAPHICS, vk->pipeline);
    vkCmdDraw(vk->cmds[i], 3, 1, 0, 0);
    vkCmdEndRenderPass(vk->cmds[i]);
    vk_check(vkEndCommandBuffer(vk->cmds[i]), "vkEndCommandBuffer");
}

int main(void) {
    struct wl_state wl = { .width = 800, .height = 600 };
    struct vk_state vk = { 0 };

    wl.display = wl_display_connect(NULL);
    if (!wl.display) die("cannot connect to the Wayland display (is WAYLAND_DISPLAY set?)");
    struct wl_registry *reg = wl_display_get_registry(wl.display);
    wl_registry_add_listener(reg, &registry_listener, &wl);
    wl_display_roundtrip(wl.display);
    if (!wl.compositor || !wl.wm_base) die("compositor does not offer wl_compositor + xdg_wm_base");

    wl.surface = wl_compositor_create_surface(wl.compositor);
    wl.xdg_surface = xdg_wm_base_get_xdg_surface(wl.wm_base, wl.surface);
    xdg_surface_add_listener(wl.xdg_surface, &xdg_surface_listener, &wl);
    wl.toplevel = xdg_surface_get_toplevel(wl.xdg_surface);
    xdg_toplevel_add_listener(wl.toplevel, &toplevel_listener, &wl);
    xdg_toplevel_set_title(wl.toplevel, "vkstill");
    xdg_toplevel_set_app_id(wl.toplevel, "eti.noronha.limina.vkstill");
    wl_surface_commit(wl.surface);
    while (!wl.configured && wl_display_dispatch(wl.display) != -1) { }

    const char *exts[] = { VK_KHR_SURFACE_EXTENSION_NAME, VK_KHR_WAYLAND_SURFACE_EXTENSION_NAME };
    VkApplicationInfo app = {
        .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
        .pApplicationName = "vkstill",
        .apiVersion = VK_API_VERSION_1_1,
    };
    VkInstanceCreateInfo ici = {
        .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
        .pApplicationInfo = &app,
        .enabledExtensionCount = 2,
        .ppEnabledExtensionNames = exts,
    };
    vk_check(vkCreateInstance(&ici, NULL, &vk.instance), "vkCreateInstance");

    VkWaylandSurfaceCreateInfoKHR sci = {
        .sType = VK_STRUCTURE_TYPE_WAYLAND_SURFACE_CREATE_INFO_KHR,
        .display = wl.display,
        .surface = wl.surface,
    };
    vk_check(vkCreateWaylandSurfaceKHR(vk.instance, &sci, NULL, &vk.surface),
             "vkCreateWaylandSurfaceKHR");

    uint32_t n = 0;
    vk_check_enum(vkEnumeratePhysicalDevices(vk.instance, &n, NULL), "vkEnumeratePhysicalDevices");
    if (!n) die("no Vulkan physical device (venus not enumerating?)");
    VkPhysicalDevice devs[8];
    if (n > 8) n = 8;
    vk_check_enum(vkEnumeratePhysicalDevices(vk.instance, &n, devs), "vkEnumeratePhysicalDevices");

    bool found = false;
    for (uint32_t d = 0; d < n && !found; d++) {
        uint32_t qn = 0;
        vkGetPhysicalDeviceQueueFamilyProperties(devs[d], &qn, NULL);
        VkQueueFamilyProperties qs[16];
        if (qn > 16) qn = 16;
        vkGetPhysicalDeviceQueueFamilyProperties(devs[d], &qn, qs);
        for (uint32_t q = 0; q < qn; q++) {
            VkBool32 present = VK_FALSE;
            vkGetPhysicalDeviceSurfaceSupportKHR(devs[d], q, vk.surface, &present);
            if ((qs[q].queueFlags & VK_QUEUE_GRAPHICS_BIT) && present) {
                vk.phys = devs[d];
                vk.queue_family = q;
                found = true;
                break;
            }
        }
    }
    if (!found) die("no graphics queue family can present to this surface");

    VkPhysicalDeviceProperties props;
    vkGetPhysicalDeviceProperties(vk.phys, &props);
    // Named on stdout so the test (and a human) can see WHICH driver drew this.
    printf("vkstill: device %s\n", props.deviceName);
    fflush(stdout);

    float prio = 1.0f;
    VkDeviceQueueCreateInfo qci = {
        .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
        .queueFamilyIndex = vk.queue_family,
        .queueCount = 1,
        .pQueuePriorities = &prio,
    };
    const char *dev_exts[] = { VK_KHR_SWAPCHAIN_EXTENSION_NAME };
    VkDeviceCreateInfo dci = {
        .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
        .queueCreateInfoCount = 1,
        .pQueueCreateInfos = &qci,
        .enabledExtensionCount = 1,
        .ppEnabledExtensionNames = dev_exts,
    };
    vk_check(vkCreateDevice(vk.phys, &dci, NULL, &vk.device), "vkCreateDevice");
    vkGetDeviceQueue(vk.device, vk.queue_family, 0, &vk.queue);

    uint32_t fn = 0;
    vk_check_enum(vkGetPhysicalDeviceSurfaceFormatsKHR(vk.phys, vk.surface, &fn, NULL),
             "vkGetPhysicalDeviceSurfaceFormatsKHR");
    VkSurfaceFormatKHR formats[64];
    if (fn > 64) fn = 64;
    vk_check_enum(vkGetPhysicalDeviceSurfaceFormatsKHR(vk.phys, vk.surface, &fn, formats),
             "vkGetPhysicalDeviceSurfaceFormatsKHR");
    vk.format = formats[0].format;
    for (uint32_t i = 0; i < fn; i++) {
        if (formats[i].format == VK_FORMAT_B8G8R8A8_UNORM ||
            formats[i].format == VK_FORMAT_R8G8B8A8_UNORM) {
            vk.format = formats[i].format;
            break;
        }
    }

    VkAttachmentDescription att = {
        .format = vk.format,
        .samples = VK_SAMPLE_COUNT_1_BIT,
        .loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR,
        .storeOp = VK_ATTACHMENT_STORE_OP_STORE,
        .stencilLoadOp = VK_ATTACHMENT_LOAD_OP_DONT_CARE,
        .stencilStoreOp = VK_ATTACHMENT_STORE_OP_DONT_CARE,
        .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
        .finalLayout = VK_IMAGE_LAYOUT_PRESENT_SRC_KHR,
    };
    VkAttachmentReference ref = { 0, VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL };
    VkSubpassDescription sub = {
        .pipelineBindPoint = VK_PIPELINE_BIND_POINT_GRAPHICS,
        .colorAttachmentCount = 1,
        .pColorAttachments = &ref,
    };
    VkSubpassDependency dep = {
        .srcSubpass = VK_SUBPASS_EXTERNAL,
        .srcStageMask = VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT,
        .dstStageMask = VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT,
        .dstAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
    };
    VkRenderPassCreateInfo rpci = {
        .sType = VK_STRUCTURE_TYPE_RENDER_PASS_CREATE_INFO,
        .attachmentCount = 1,
        .pAttachments = &att,
        .subpassCount = 1,
        .pSubpasses = &sub,
        .dependencyCount = 1,
        .pDependencies = &dep,
    };
    vk_check(vkCreateRenderPass(vk.device, &rpci, NULL, &vk.pass), "vkCreateRenderPass");

    VkShaderModule vs = make_shader(vk.device, VERT_SPV, sizeof VERT_SPV);
    VkShaderModule fs = make_shader(vk.device, FRAG_SPV, sizeof FRAG_SPV);
    VkPipelineShaderStageCreateInfo stages[2] = {
        { .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
          .stage = VK_SHADER_STAGE_VERTEX_BIT, .module = vs, .pName = "main" },
        { .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
          .stage = VK_SHADER_STAGE_FRAGMENT_BIT, .module = fs, .pName = "main" },
    };
    VkPipelineVertexInputStateCreateInfo vi = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO,
    };
    VkPipelineInputAssemblyStateCreateInfo ia = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO,
        .topology = VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST,
    };
    VkPipelineViewportStateCreateInfo vpst = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO,
        .viewportCount = 1,
        .scissorCount = 1,
    };
    VkPipelineRasterizationStateCreateInfo rs = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO,
        .polygonMode = VK_POLYGON_MODE_FILL,
        .cullMode = VK_CULL_MODE_NONE,
        .frontFace = VK_FRONT_FACE_COUNTER_CLOCKWISE,
        .lineWidth = 1.0f,
    };
    VkPipelineMultisampleStateCreateInfo ms = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_MULTISAMPLE_STATE_CREATE_INFO,
        .rasterizationSamples = VK_SAMPLE_COUNT_1_BIT,
    };
    VkPipelineColorBlendAttachmentState cba = { .colorWriteMask = 0xf };
    VkPipelineColorBlendStateCreateInfo cb = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_COLOR_BLEND_STATE_CREATE_INFO,
        .attachmentCount = 1,
        .pAttachments = &cba,
    };
    VkDynamicState dyn[] = { VK_DYNAMIC_STATE_VIEWPORT, VK_DYNAMIC_STATE_SCISSOR };
    VkPipelineDynamicStateCreateInfo ds = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_DYNAMIC_STATE_CREATE_INFO,
        .dynamicStateCount = 2,
        .pDynamicStates = dyn,
    };
    VkPipelineLayoutCreateInfo plci = { .sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO };
    vk_check(vkCreatePipelineLayout(vk.device, &plci, NULL, &vk.layout), "vkCreatePipelineLayout");
    VkGraphicsPipelineCreateInfo gpci = {
        .sType = VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO,
        .stageCount = 2,
        .pStages = stages,
        .pVertexInputState = &vi,
        .pInputAssemblyState = &ia,
        .pViewportState = &vpst,
        .pRasterizationState = &rs,
        .pMultisampleState = &ms,
        .pColorBlendState = &cb,
        .pDynamicState = &ds,
        .layout = vk.layout,
        .renderPass = vk.pass,
    };
    vk_check(vkCreateGraphicsPipelines(vk.device, VK_NULL_HANDLE, 1, &gpci, NULL, &vk.pipeline),
             "vkCreateGraphicsPipelines");
    vkDestroyShaderModule(vk.device, vs, NULL);
    vkDestroyShaderModule(vk.device, fs, NULL);

    VkCommandPoolCreateInfo cpci = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
        .flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT,
        .queueFamilyIndex = vk.queue_family,
    };
    vk_check(vkCreateCommandPool(vk.device, &cpci, NULL, &vk.pool), "vkCreateCommandPool");

    VkSemaphoreCreateInfo semci = { .sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO };
    vk_check(vkCreateSemaphore(vk.device, &semci, NULL, &vk.acquired), "vkCreateSemaphore");
    vk_check(vkCreateSemaphore(vk.device, &semci, NULL, &vk.rendered), "vkCreateSemaphore");
    VkFenceCreateInfo fci = {
        .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO,
        .flags = VK_FENCE_CREATE_SIGNALED_BIT,
    };
    vk_check(vkCreateFence(vk.device, &fci, NULL, &vk.in_flight), "vkCreateFence");

    create_swapchain(&vk);
    VkCommandBufferAllocateInfo cbai = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
        .commandPool = vk.pool,
        .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
        .commandBufferCount = vk.image_count,
    };
    vk_check(vkAllocateCommandBuffers(vk.device, &cbai, vk.cmds), "vkAllocateCommandBuffers");
    for (uint32_t i = 0; i < vk.image_count; i++) record(&vk, i);

    while (!wl.closed) {
        if (wl_display_dispatch_pending(wl.display) == -1) break;
        wl_display_flush(wl.display);

        vkWaitForFences(vk.device, 1, &vk.in_flight, VK_TRUE, UINT64_MAX);
        uint32_t idx = 0;
        VkResult r = vkAcquireNextImageKHR(vk.device, vk.swapchain, UINT64_MAX, vk.acquired,
                                           VK_NULL_HANDLE, &idx);
        if (r == VK_ERROR_OUT_OF_DATE_KHR || r == VK_SUBOPTIMAL_KHR) {
            vkDeviceWaitIdle(vk.device);
            destroy_swapchain(&vk);
            create_swapchain(&vk);
            for (uint32_t i = 0; i < vk.image_count; i++) record(&vk, i);
            continue;
        }
        vk_check(r, "vkAcquireNextImageKHR");
        vkResetFences(vk.device, 1, &vk.in_flight);

        VkPipelineStageFlags wait = VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT;
        VkSubmitInfo si = {
            .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
            .waitSemaphoreCount = 1,
            .pWaitSemaphores = &vk.acquired,
            .pWaitDstStageMask = &wait,
            .commandBufferCount = 1,
            .pCommandBuffers = &vk.cmds[idx],
            .signalSemaphoreCount = 1,
            .pSignalSemaphores = &vk.rendered,
        };
        vk_check(vkQueueSubmit(vk.queue, 1, &si, vk.in_flight), "vkQueueSubmit");

        VkPresentInfoKHR pi = {
            .sType = VK_STRUCTURE_TYPE_PRESENT_INFO_KHR,
            .waitSemaphoreCount = 1,
            .pWaitSemaphores = &vk.rendered,
            .swapchainCount = 1,
            .pSwapchains = &vk.swapchain,
            .pImageIndices = &idx,
        };
        r = vkQueuePresentKHR(vk.queue, &pi);
        if (r == VK_ERROR_OUT_OF_DATE_KHR || r == VK_SUBOPTIMAL_KHR) {
            vkDeviceWaitIdle(vk.device);
            destroy_swapchain(&vk);
            create_swapchain(&vk);
            for (uint32_t i = 0; i < vk.image_count; i++) record(&vk, i);
        } else {
            vk_check(r, "vkQueuePresentKHR");
        }
    }
    return 0;
}
