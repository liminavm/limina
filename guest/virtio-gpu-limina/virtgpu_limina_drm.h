/* SPDX-License-Identifier: MIT */
/*
 * limina's extension of the virtio-gpu DRM UAPI: a guest context of the Metal-level transport, its
 * arenas and its doorbell. Only present when the device negotiated VIRTIO_GPU_F_LIMINA_EXT; on
 * any other virtio-gpu these ioctls fail (ENODEV here, EINVAL on an upstream driver).
 *
 * One extension context per open file. Its doorbell and arenas are mapped with mmap() on the same
 * file at the offsets these ioctls return.
 */
#ifndef VIRTGPU_LIMINA_DRM_H
#define VIRTGPU_LIMINA_DRM_H

#include <drm/drm.h>

#if defined(__cplusplus)
extern "C" {
#endif

#define DRM_VIRTGPU_LIMINA_CTX_CREATE    0x20
#define DRM_VIRTGPU_LIMINA_ARENA_CREATE  0x21
#define DRM_VIRTGPU_LIMINA_ARENA_DESTROY 0x22
#define DRM_VIRTGPU_LIMINA_RING_ATTACH   0x23
#define DRM_VIRTGPU_LIMINA_ARENA_EXPORT  0x24
#define DRM_VIRTGPU_LIMINA_ARENA_IMPORT  0x25
#define DRM_VIRTGPU_LIMINA_FENCE         0x26
#define DRM_VIRTGPU_LIMINA_SURFACE_CREATE 0x27
#define DRM_VIRTGPU_LIMINA_SURFACE_IMPORT 0x28
#define DRM_VIRTGPU_LIMINA_INFO          0x29

/* Arena ids are below this. */
#define VIRTGPU_LIMINA_MAX_ARENAS 4096

struct drm_virtgpu_limina_ctx_create {
	__u32 flags;		/* in: 0 */
	__u32 pad;
	__u64 doorbell_offset;	/* out: mmap offset of the doorbell page */
};

/*
 * A shared arena can be exported (ARENA_EXPORT) as a GEM object, which is how it is mapped and how
 * it reaches other processes, as a dma-buf. It has no arena mmap offset.
 */
#define VIRTGPU_LIMINA_ARENA_SHARED 1

struct drm_virtgpu_limina_arena_create {
	__u32 arena_id;		/* in: chosen by userspace, < VIRTGPU_LIMINA_MAX_ARENAS */
	__u32 flags;		/* in: VIRTGPU_LIMINA_ARENA_* */
	__u64 size;		/* in: wanted; out: allocated (rounded up by the host) */
	__u64 gpu_base;		/* out: GPU address of the arena's first byte */
	__u64 mmap_offset;	/* out: mmap offset of the arena; offset + n maps from byte n, page-aligned;
				 * 0 for a shared arena */
};

/* Fails with EBUSY while any mapping of the arena exists. */
struct drm_virtgpu_limina_arena_destroy {
	__u32 arena_id;
	__u32 pad;
};

/*
 * Wraps a shared arena in a GEM object (a host-visible blob resource): map it with
 * DRM_IOCTL_VIRTGPU_MAP, share it with PRIME. One export per arena. The memory lives while the
 * arena, the object or any import of it does.
 */
struct drm_virtgpu_limina_arena_export {
	__u32 arena_id;
	__u32 handle;		/* out: GEM handle */
};

/*
 * Makes arena `arena_id` of this context the memory of a GEM object another context exported
 * (typically received as a dma-buf and turned into a handle with PRIME). The arena is shared: map
 * it through the handle. Destroy it with ARENA_DESTROY like any arena.
 */
struct drm_virtgpu_limina_arena_import {
	__u32 arena_id;
	__u32 handle;		/* in: GEM handle of the exported object */
	__u64 size;		/* out */
	__u64 gpu_base;		/* out: the same address as in the exporting context */
};

/*
 * A fence that signals once the u64 at offset `completed_at` of the ring's arena reaches `seqno`
 * (or the context is gone): a queue's completed value, or with `completed_at` 0 the ring's own.
 * A nonzero `completed_at` must be 8-aligned and inside the ring's arena, and a fence whose
 * value is already reached signals at once. A context can fence at most 63 distinct nonzero
 * `completed_at`s (ENOSPC after). It is added, as a write, to the reservation of each
 * GEM object in `handles` (implicit sync for whoever reads them as dma-bufs), and with
 * VIRTGPU_LIMINA_FENCE_FD it is returned as a sync_file too. Fences on one `completed_at` signal
 * in the order they were made.
 */
#define VIRTGPU_LIMINA_FENCE_FD 1

struct drm_virtgpu_limina_fence {
	__u64 seqno;
	__u64 handles;		/* in: pointer to num_handles __u32 GEM handles */
	__u32 num_handles;
	__u32 flags;		/* VIRTGPU_LIMINA_FENCE_* */
	__s32 fence_fd;		/* out, with VIRTGPU_LIMINA_FENCE_FD */
	__u32 completed_at;
};

/*
 * A GEM object whose storage is a surface the host makes: `width` x `height` of DRM format
 * `drm_fourcc` (ARGB/XRGB/ABGR/XBGR8888), rows `stride` bytes apart. This context renders into it
 * through a texture made over surface `res_handle` (the ring's CREATE_TEXTURE_FROM_SURFACE); any
 * other context, of any kind, reaches it as an ordinary resource, typically received as a dma-buf.
 * It cannot be mapped.
 */
struct drm_virtgpu_limina_surface_create {
	__u32 width;
	__u32 height;
	__u32 drm_fourcc;
	__u32 stride;
	__u32 handle;		/* out: GEM handle */
	__u32 res_handle;	/* out: the resource, which names the surface on the ring */
};

/*
 * The surface GEM object `handle` is presented from, whichever driver made it (typically received
 * as a dma-buf from another process), given to this context: a texture made over surface
 * `res_handle` on the ring reads and writes its pixels. Answered with its shape; -EINVAL for an
 * object with no surface the host can share.
 */
struct drm_virtgpu_limina_surface_import {
	__u32 handle;
	__u32 width;		/* out */
	__u32 height;		/* out */
	__u32 drm_fourcc;	/* out */
	__u32 stride;		/* out */
	__u32 res_handle;	/* out */
};

/*
 * What the host speaks, without creating a context: a driver declines a host whose protocol it
 * does not speak before it costs the host anything. ENODEV without the extension.
 */
struct drm_virtgpu_limina_info {
	__u32 wire_version;	/* out: the protocol a context's ring speaks */
	__u32 flags;		/* out: none defined yet */
};

/* Starts the host consuming the command ring userspace initialised in an arena. */
struct drm_virtgpu_limina_ring_attach {
	__u32 arena_id;
	__u32 data_size;
	__u64 offset;
};

#define DRM_IOCTL_VIRTGPU_LIMINA_CTX_CREATE \
	DRM_IOWR(DRM_COMMAND_BASE + DRM_VIRTGPU_LIMINA_CTX_CREATE, \
		 struct drm_virtgpu_limina_ctx_create)
#define DRM_IOCTL_VIRTGPU_LIMINA_ARENA_CREATE \
	DRM_IOWR(DRM_COMMAND_BASE + DRM_VIRTGPU_LIMINA_ARENA_CREATE, \
		 struct drm_virtgpu_limina_arena_create)
#define DRM_IOCTL_VIRTGPU_LIMINA_ARENA_DESTROY \
	DRM_IOW(DRM_COMMAND_BASE + DRM_VIRTGPU_LIMINA_ARENA_DESTROY, \
		struct drm_virtgpu_limina_arena_destroy)
#define DRM_IOCTL_VIRTGPU_LIMINA_RING_ATTACH \
	DRM_IOW(DRM_COMMAND_BASE + DRM_VIRTGPU_LIMINA_RING_ATTACH, \
		struct drm_virtgpu_limina_ring_attach)
#define DRM_IOCTL_VIRTGPU_LIMINA_ARENA_EXPORT \
	DRM_IOWR(DRM_COMMAND_BASE + DRM_VIRTGPU_LIMINA_ARENA_EXPORT, \
		 struct drm_virtgpu_limina_arena_export)
#define DRM_IOCTL_VIRTGPU_LIMINA_ARENA_IMPORT \
	DRM_IOWR(DRM_COMMAND_BASE + DRM_VIRTGPU_LIMINA_ARENA_IMPORT, \
		 struct drm_virtgpu_limina_arena_import)
#define DRM_IOCTL_VIRTGPU_LIMINA_FENCE \
	DRM_IOWR(DRM_COMMAND_BASE + DRM_VIRTGPU_LIMINA_FENCE, \
		 struct drm_virtgpu_limina_fence)
#define DRM_IOCTL_VIRTGPU_LIMINA_SURFACE_CREATE \
	DRM_IOWR(DRM_COMMAND_BASE + DRM_VIRTGPU_LIMINA_SURFACE_CREATE, \
		 struct drm_virtgpu_limina_surface_create)
#define DRM_IOCTL_VIRTGPU_LIMINA_SURFACE_IMPORT \
	DRM_IOWR(DRM_COMMAND_BASE + DRM_VIRTGPU_LIMINA_SURFACE_IMPORT, \
		 struct drm_virtgpu_limina_surface_import)
#define DRM_IOCTL_VIRTGPU_LIMINA_INFO \
	DRM_IOR(DRM_COMMAND_BASE + DRM_VIRTGPU_LIMINA_INFO, \
		struct drm_virtgpu_limina_info)

#if defined(__cplusplus)
}
#endif

#endif
