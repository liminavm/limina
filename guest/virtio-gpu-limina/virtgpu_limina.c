// SPDX-License-Identifier: MIT
/*
 * limina's native-context extension of virtio-gpu.
 *
 * Behind VIRTIO_GPU_F_LIMINA_EXT the device offers two more shared-memory regions and a handful of
 * control commands. A guest context gets a doorbell page (stores to it reach the host renderer on
 * the vCPU thread, without a virtqueue) and arenas: host GPU memory either mapped into a window of
 * guest physical space, or, for a shared arena, exported as a host-visible blob GEM object that
 * other processes receive as a dma-buf and import into their own context. Userspace writes its commands into a ring inside an arena and rings the
 * doorbell only when the host says it is asleep; nothing here is on that path.
 *
 * This file is what the kernel adds: it creates contexts and arenas on the host for each open
 * file, and maps them into the file's process. Display and the upstream 3D paths are untouched.
 */

#include <linux/dma-fence.h>
#include <linux/file.h>
#include <linux/mm.h>
#include <linux/sizes.h>
#include <linux/slab.h>
#include <linux/sync_file.h>

#include <drm/drm_drv.h>
#include <drm/drm_file.h>
#include <drm/drm_gem.h>
#include <drm/drm_print.h>
#include <drm/virtgpu_drm.h>

#include "virtgpu_drv.h"
#include "virtgpu_limina_drm.h"

#define VIRTIO_GPU_SHM_ID_LIMINA_ARENAS		0x80
#define VIRTIO_GPU_SHM_ID_LIMINA_DOORBELLS	0x81
/* One 16 KiB slot per context, so 4 KiB and 16 KiB guests both map a page of their own. */
#define LIMINA_DOORBELL_STRIDE			(16 * 1024)
/* Fences a file may have waiting at once. Each holds a control-queue descriptor until the host
 * answers it, and the queue is shared by every client and the display: far above what a driver
 * keeps in flight, far below the queue's size.
 */
#define LIMINA_MAX_PENDING_FENCES		64

#define VIRTIO_GPU_CMD_LIMINA_CTX_CREATE	0x4c00
#define VIRTIO_GPU_CMD_LIMINA_CTX_DESTROY	0x4c01
#define VIRTIO_GPU_CMD_LIMINA_ARENA_CREATE	0x4c02
#define VIRTIO_GPU_CMD_LIMINA_ARENA_DESTROY	0x4c03
#define VIRTIO_GPU_CMD_LIMINA_RING_ATTACH	0x4c04
#define VIRTIO_GPU_CMD_LIMINA_ARENA_IMPORT	0x4c05
#define VIRTIO_GPU_CMD_LIMINA_SURFACE_CREATE	0x4c07
#define VIRTIO_GPU_CMD_LIMINA_SURFACE_IMPORT	0x4c08
#define VIRTIO_GPU_CMD_LIMINA_INFO		0x4c09
#define VIRTIO_GPU_RESP_OK_LIMINA_CTX		0x1180
#define VIRTIO_GPU_RESP_OK_LIMINA_ARENA		0x1181
#define VIRTIO_GPU_RESP_OK_LIMINA_SURFACE	0x1182
#define VIRTIO_GPU_RESP_OK_LIMINA_INFO		0x1183

/*
 * mmap offsets live far above the GEM fake-offset range, so drm_gem_mmap keeps everything below
 * LIMINA_MMAP_BASE. Arena n is at LIMINA_MMAP_ARENAS + n * LIMINA_ARENA_STRIDE; a mapping may
 * start at any page inside it.
 */
#define LIMINA_MMAP_BASE	(1ULL << 50)
#define LIMINA_MMAP_DOORBELL	LIMINA_MMAP_BASE
#define LIMINA_MMAP_ARENAS	(LIMINA_MMAP_BASE + (1ULL << 34))
#define LIMINA_ARENA_STRIDE	(1ULL << 34)
/* The host's shm_offset for an arena that is not in the window: a shared one. */
#define LIMINA_NOT_IN_WINDOW	(~0ULL)
/* ARENA_CREATE flag on the wire, as in the UAPI. */
#define LIMINA_ARENA_FLAG_BLOB	1

struct virtio_gpu_limina_ctx_create {
	struct virtio_gpu_ctrl_hdr hdr;
	__le32 flags;
	__le32 padding;
};

struct virtio_gpu_limina_arena_create {
	struct virtio_gpu_ctrl_hdr hdr;
	__le32 arena_id;
	__le32 flags;
	__le64 size;
};

struct virtio_gpu_limina_arena_import {
	struct virtio_gpu_ctrl_hdr hdr;
	__le32 arena_id;
	__le32 resource_id;
};

struct virtio_gpu_limina_surface_create {
	struct virtio_gpu_ctrl_hdr hdr;
	__le32 resource_id;
	__le32 width;
	__le32 height;
	__le32 drm_fourcc;
	__le32 stride;
	__le32 padding;
};

struct virtio_gpu_limina_surface_import {
	struct virtio_gpu_ctrl_hdr hdr;
	__le32 resource_id;
	__le32 padding;
};

struct virtio_gpu_limina_resp_surface {
	struct virtio_gpu_ctrl_hdr hdr;
	__le32 width;
	__le32 height;
	__le32 drm_fourcc;
	__le32 stride;
};

struct virtio_gpu_limina_arena_destroy {
	struct virtio_gpu_ctrl_hdr hdr;
	__le32 arena_id;
	__le32 padding;
};

struct virtio_gpu_limina_ring_attach {
	struct virtio_gpu_ctrl_hdr hdr;
	__le32 arena_id;
	__le32 data_size;
	__le64 offset;
};

struct virtio_gpu_limina_resp_info {
	struct virtio_gpu_ctrl_hdr hdr;
	__le32 wire_version;
	__le32 flags;
};

struct virtio_gpu_limina_resp_ctx {
	struct virtio_gpu_ctrl_hdr hdr;
	__le32 doorbell;
	__le32 padding;
};

struct virtio_gpu_limina_resp_arena {
	struct virtio_gpu_ctrl_hdr hdr;
	__le64 shm_offset;
	__le64 size;
	__le64 gpu_base;
};

struct limina_arena {
	struct list_head node;
	u32 id;
	u64 shm_offset;
	u64 size;
	u64 gpu_base;
	/* Live mappings; the arena cannot be destroyed while any exists. */
	atomic_t mappings;
	/* Not in the arena window: mapped and shared through a GEM object (ARENA_EXPORT). */
	bool shared;
	bool exported;
};

struct virtio_gpu_limina_file {
	struct mutex lock;
	bool created;
	/* dma-fence context of this file's fences: they signal in order, and apart from every
	 * other context's. */
	u64 fence_ctx;
	u32 doorbell;
	struct list_head arenas;
	/* Fences not yet known signaled, at most LIMINA_MAX_PENDING_FENCES. */
	struct dma_fence *pending[LIMINA_MAX_PENDING_FENCES];
	unsigned int npending;
};

/* Takes a slot for `f` unless the file already has its fill of fences waiting. Call with
 * lf->lock held.
 */
static bool limina_fence_track(struct virtio_gpu_limina_file *lf, struct dma_fence *f)
{
	unsigned int i, kept = 0;

	for (i = 0; i < lf->npending; i++) {
		if (dma_fence_is_signaled(lf->pending[i]))
			dma_fence_put(lf->pending[i]);
		else
			lf->pending[kept++] = lf->pending[i];
	}
	lf->npending = kept;
	if (lf->npending == LIMINA_MAX_PENDING_FENCES)
		return false;
	lf->pending[lf->npending++] = dma_fence_get(f);
	return true;
}

/* Gives up the slot of a fence that was never sent. Call with lf->lock held. */
static void limina_fence_untrack(struct virtio_gpu_limina_file *lf, struct dma_fence *f)
{
	unsigned int i;

	for (i = 0; i < lf->npending; i++) {
		if (lf->pending[i] == f) {
			lf->pending[i] = lf->pending[--lf->npending];
			dma_fence_put(f);
			return;
		}
	}
}

void virtio_gpu_limina_init(struct virtio_gpu_device *vgdev)
{
	if (!virtio_has_feature(vgdev->vdev, VIRTIO_GPU_F_LIMINA_EXT))
		return;
	if (!virtio_get_shm_region(vgdev->vdev, &vgdev->limina_arenas,
				   VIRTIO_GPU_SHM_ID_LIMINA_ARENAS) ||
	    !virtio_get_shm_region(vgdev->vdev, &vgdev->limina_doorbells,
				   VIRTIO_GPU_SHM_ID_LIMINA_DOORBELLS)) {
		DRM_ERROR("limina extension negotiated without its shm regions\n");
		return;
	}
	vgdev->has_limina = true;
	DRM_INFO("limina extension: arenas 0x%llx+0x%llx, doorbells 0x%llx+0x%llx\n",
		 vgdev->limina_arenas.addr, vgdev->limina_arenas.len,
		 vgdev->limina_doorbells.addr, vgdev->limina_doorbells.len);
}

static int limina_errno(u32 type)
{
	switch (type) {
	case VIRTIO_GPU_RESP_ERR_OUT_OF_MEMORY:
		return -ENOMEM;
	case VIRTIO_GPU_RESP_ERR_INVALID_CONTEXT_ID:
		return -ENOENT;
	case VIRTIO_GPU_RESP_ERR_INVALID_PARAMETER:
	case VIRTIO_GPU_RESP_ERR_INVALID_RESOURCE_ID:
		return -EINVAL;
	default:
		return -EIO;
	}
}

/* Sends `cmd` and waits for a response of type `ok` into `resp`. */
static int limina_call(struct virtio_gpu_device *vgdev, void *cmd, int cmd_size,
		       void *resp, int resp_size, u32 ok)
{
	struct virtio_gpu_limina_call call = {
		.cmd = cmd,
		.cmd_size = cmd_size,
		.resp = resp,
		.resp_size = resp_size,
	};
	u32 type;
	int ret;

	ret = virtio_gpu_cmd_limina(vgdev, &call);
	if (ret)
		return ret;
	type = le32_to_cpu(((struct virtio_gpu_ctrl_hdr *)resp)->type);
	return type == ok ? 0 : limina_errno(type);
}

static struct virtio_gpu_limina_file *limina_file(struct virtio_gpu_device *vgdev,
						  struct drm_file *file)
{
	struct virtio_gpu_fpriv *vfpriv = file->driver_priv;
	struct virtio_gpu_limina_file *lf;

	if (!vgdev->has_limina || !vfpriv)
		return NULL;
	mutex_lock(&vfpriv->context_lock);
	lf = vfpriv->limina;
	if (!lf) {
		lf = kzalloc(sizeof(*lf), GFP_KERNEL);
		if (lf) {
			mutex_init(&lf->lock);
			INIT_LIST_HEAD(&lf->arenas);
			lf->fence_ctx = dma_fence_context_alloc(1);
			vfpriv->limina = lf;
		}
	}
	mutex_unlock(&vfpriv->context_lock);
	return lf;
}

static struct limina_arena *limina_find(struct virtio_gpu_limina_file *lf, u32 id)
{
	struct limina_arena *a;

	list_for_each_entry(a, &lf->arenas, node)
		if (a->id == id)
			return a;
	return NULL;
}

int virtio_gpu_limina_info_ioctl(struct drm_device *dev, void *data, struct drm_file *file)
{
	struct virtio_gpu_device *vgdev = dev->dev_private;
	struct drm_virtgpu_limina_info *args = data;
	struct virtio_gpu_ctrl_hdr cmd = {};
	struct virtio_gpu_limina_resp_info resp = {};
	int ret;

	if (!vgdev->has_limina)
		return -ENODEV;
	cmd.type = cpu_to_le32(VIRTIO_GPU_CMD_LIMINA_INFO);
	ret = limina_call(vgdev, &cmd, sizeof(cmd), &resp, sizeof(resp),
			  VIRTIO_GPU_RESP_OK_LIMINA_INFO);
	if (ret)
		return ret;
	args->wire_version = le32_to_cpu(resp.wire_version);
	args->flags = le32_to_cpu(resp.flags);
	return 0;
}

int virtio_gpu_limina_ctx_create_ioctl(struct drm_device *dev, void *data,
				       struct drm_file *file)
{
	struct virtio_gpu_device *vgdev = dev->dev_private;
	struct virtio_gpu_fpriv *vfpriv = file->driver_priv;
	struct drm_virtgpu_limina_ctx_create *args = data;
	struct virtio_gpu_limina_ctx_create cmd = {};
	struct virtio_gpu_limina_resp_ctx resp = {};
	struct virtio_gpu_limina_file *lf;
	u32 doorbell;
	int ret;

	if (!vgdev->has_limina)
		return -ENODEV;
	if (args->flags)
		return -EINVAL;
	lf = limina_file(vgdev, file);
	if (!lf)
		return -ENOMEM;

	mutex_lock(&lf->lock);
	ret = -EEXIST;
	if (lf->created)
		goto out;
	cmd.hdr.type = cpu_to_le32(VIRTIO_GPU_CMD_LIMINA_CTX_CREATE);
	cmd.hdr.ctx_id = cpu_to_le32(vfpriv->ctx_id);
	ret = limina_call(vgdev, &cmd, sizeof(cmd), &resp, sizeof(resp),
			  VIRTIO_GPU_RESP_OK_LIMINA_CTX);
	if (ret)
		goto out;
	doorbell = le32_to_cpu(resp.doorbell);
	if ((u64)(doorbell + 1) * LIMINA_DOORBELL_STRIDE > vgdev->limina_doorbells.len) {
		ret = -EIO;
		goto out;
	}
	lf->doorbell = doorbell;
	lf->created = true;
	args->doorbell_offset = LIMINA_MMAP_DOORBELL;
out:
	mutex_unlock(&lf->lock);
	return ret;
}

int virtio_gpu_limina_arena_create_ioctl(struct drm_device *dev, void *data,
					 struct drm_file *file)
{
	struct virtio_gpu_device *vgdev = dev->dev_private;
	struct virtio_gpu_fpriv *vfpriv = file->driver_priv;
	struct drm_virtgpu_limina_arena_create *args = data;
	struct virtio_gpu_limina_arena_create cmd = {};
	struct virtio_gpu_limina_resp_arena resp = {};
	struct virtio_gpu_limina_file *lf;
	struct limina_arena *a;
	u64 off, size;
	int ret;

	if (!vgdev->has_limina)
		return -ENODEV;
	if (args->arena_id >= VIRTGPU_LIMINA_MAX_ARENAS || !args->size ||
	    args->flags & ~VIRTGPU_LIMINA_ARENA_SHARED)
		return -EINVAL;
	/* A shared arena is a host-visible blob. */
	if ((args->flags & VIRTGPU_LIMINA_ARENA_SHARED) &&
	    !(vgdev->has_resource_blob && vgdev->has_host_visible))
		return -EOPNOTSUPP;
	lf = limina_file(vgdev, file);
	if (!lf)
		return -ENOMEM;
	a = kzalloc(sizeof(*a), GFP_KERNEL);
	if (!a)
		return -ENOMEM;

	mutex_lock(&lf->lock);
	ret = -ENOENT;
	if (!lf->created)
		goto err;
	ret = -EEXIST;
	if (limina_find(lf, args->arena_id))
		goto err;
	cmd.hdr.type = cpu_to_le32(VIRTIO_GPU_CMD_LIMINA_ARENA_CREATE);
	cmd.hdr.ctx_id = cpu_to_le32(vfpriv->ctx_id);
	cmd.arena_id = cpu_to_le32(args->arena_id);
	if (args->flags & VIRTGPU_LIMINA_ARENA_SHARED)
		cmd.flags = cpu_to_le32(LIMINA_ARENA_FLAG_BLOB);
	cmd.size = cpu_to_le64(args->size);
	ret = limina_call(vgdev, &cmd, sizeof(cmd), &resp, sizeof(resp),
			  VIRTIO_GPU_RESP_OK_LIMINA_ARENA);
	if (ret)
		goto err;
	off = le64_to_cpu(resp.shm_offset);
	size = le64_to_cpu(resp.size);
	a->shared = args->flags & VIRTGPU_LIMINA_ARENA_SHARED;
	if (a->shared ? off != LIMINA_NOT_IN_WINDOW || !PAGE_ALIGNED(size) :
	    !PAGE_ALIGNED(off) || !PAGE_ALIGNED(size) || size > LIMINA_ARENA_STRIDE ||
	    off > vgdev->limina_arenas.len || size > vgdev->limina_arenas.len - off) {
		DRM_ERROR("limina: host placed arena %u out of its window\n", args->arena_id);
		ret = -EIO;
		goto err;
	}
	a->id = args->arena_id;
	a->shm_offset = off;
	a->size = size;
	a->gpu_base = le64_to_cpu(resp.gpu_base);
	atomic_set(&a->mappings, 0);
	list_add(&a->node, &lf->arenas);
	args->size = size;
	args->gpu_base = a->gpu_base;
	args->mmap_offset = a->shared ? 0 : LIMINA_MMAP_ARENAS + (u64)a->id * LIMINA_ARENA_STRIDE;
	mutex_unlock(&lf->lock);
	return 0;
err:
	mutex_unlock(&lf->lock);
	kfree(a);
	return ret;
}

int virtio_gpu_limina_arena_destroy_ioctl(struct drm_device *dev, void *data,
					  struct drm_file *file)
{
	struct virtio_gpu_device *vgdev = dev->dev_private;
	struct virtio_gpu_fpriv *vfpriv = file->driver_priv;
	struct drm_virtgpu_limina_arena_destroy *args = data;
	struct virtio_gpu_limina_arena_destroy cmd = {};
	struct virtio_gpu_ctrl_hdr resp = {};
	struct virtio_gpu_limina_file *lf;
	struct limina_arena *a;
	int ret;

	if (!vgdev->has_limina)
		return -ENODEV;
	lf = limina_file(vgdev, file);
	if (!lf)
		return -ENOMEM;

	mutex_lock(&lf->lock);
	ret = -ENOENT;
	a = limina_find(lf, args->arena_id);
	if (!a)
		goto out;
	/*
	 * The host reuses the arena's guest-physical range; a mapping left behind would reach
	 * whatever lands there next.
	 */
	ret = -EBUSY;
	if (atomic_read(&a->mappings))
		goto out;
	cmd.hdr.type = cpu_to_le32(VIRTIO_GPU_CMD_LIMINA_ARENA_DESTROY);
	cmd.hdr.ctx_id = cpu_to_le32(vfpriv->ctx_id);
	cmd.arena_id = cpu_to_le32(a->id);
	ret = limina_call(vgdev, &cmd, sizeof(cmd), &resp, sizeof(resp),
			  VIRTIO_GPU_RESP_OK_NODATA);
	list_del(&a->node);
	kfree(a);
out:
	mutex_unlock(&lf->lock);
	return ret;
}

int virtio_gpu_limina_ring_attach_ioctl(struct drm_device *dev, void *data,
					struct drm_file *file)
{
	struct virtio_gpu_device *vgdev = dev->dev_private;
	struct virtio_gpu_fpriv *vfpriv = file->driver_priv;
	struct drm_virtgpu_limina_ring_attach *args = data;
	struct virtio_gpu_limina_ring_attach cmd = {};
	struct virtio_gpu_ctrl_hdr resp = {};
	struct virtio_gpu_limina_file *lf;
	int ret;

	if (!vgdev->has_limina)
		return -ENODEV;
	lf = limina_file(vgdev, file);
	if (!lf)
		return -ENOMEM;

	mutex_lock(&lf->lock);
	ret = -ENOENT;
	if (!limina_find(lf, args->arena_id))
		goto out;
	cmd.hdr.type = cpu_to_le32(VIRTIO_GPU_CMD_LIMINA_RING_ATTACH);
	cmd.hdr.ctx_id = cpu_to_le32(vfpriv->ctx_id);
	cmd.arena_id = cpu_to_le32(args->arena_id);
	cmd.data_size = cpu_to_le32(args->data_size);
	cmd.offset = cpu_to_le64(args->offset);
	ret = limina_call(vgdev, &cmd, sizeof(cmd), &resp, sizeof(resp),
			  VIRTIO_GPU_RESP_OK_NODATA);
out:
	mutex_unlock(&lf->lock);
	return ret;
}

int virtio_gpu_limina_arena_export_ioctl(struct drm_device *dev, void *data,
					 struct drm_file *file)
{
	struct virtio_gpu_device *vgdev = dev->dev_private;
	struct virtio_gpu_fpriv *vfpriv = file->driver_priv;
	struct drm_virtgpu_limina_arena_export *args = data;
	struct virtio_gpu_object_params params = {};
	struct virtio_gpu_limina_file *lf;
	struct virtio_gpu_object *bo;
	struct drm_gem_object *obj;
	struct limina_arena *a;
	u32 handle;
	int ret;

	if (!vgdev->has_limina)
		return -ENODEV;
	lf = limina_file(vgdev, file);
	if (!lf)
		return -ENOMEM;

	mutex_lock(&lf->lock);
	ret = -ENOENT;
	a = limina_find(lf, args->arena_id);
	if (!a)
		goto out;
	ret = -EINVAL;
	if (!a->shared || a->exported)
		goto out;
	/*
	 * The host blob resource of this context whose blob_id is the arena: the host maps the
	 * arena's pages for it, and keeps the arena alive while it exists. This is what
	 * RESOURCE_CREATE_BLOB does, without creating a virtio-gpu 3D context for the file.
	 */
	params.size = a->size;
	params.blob = true;
	params.ctx_id = vfpriv->ctx_id;
	params.blob_mem = VIRTGPU_BLOB_MEM_HOST3D;
	params.blob_flags = VIRTGPU_BLOB_FLAG_USE_MAPPABLE | VIRTGPU_BLOB_FLAG_USE_SHAREABLE;
	params.blob_id = a->id;
	ret = virtio_gpu_vram_create(vgdev, &params, &bo);
	if (ret < 0)
		goto out;
	bo->host3d_blob = true;
	bo->blob_mem = params.blob_mem;
	bo->blob_flags = params.blob_flags;
	obj = &bo->base.base;
	/*
	 * Neither creation nor MAP_BLOB waits for the host; a refused export fails here. Both are
	 * only queued: without the kick they wait for whatever traffic comes along next, and a
	 * quiet desktop has none for up to a minute.
	 */
	virtio_gpu_notify(vgdev);
	wait_event(vgdev->resp_wq, to_virtio_gpu_vram(bo)->map_state != STATE_INITIALIZING);
	if (to_virtio_gpu_vram(bo)->map_state != STATE_OK) {
		drm_gem_object_put(obj);
		ret = -EIO;
		goto out;
	}
	ret = drm_gem_handle_create(file, obj, &handle);
	/* The handle owns the object now, or nothing does and this frees it. */
	drm_gem_object_put(obj);
	if (ret)
		goto out;
	a->exported = true;
	args->handle = handle;
out:
	mutex_unlock(&lf->lock);
	return ret;
}

int virtio_gpu_limina_arena_import_ioctl(struct drm_device *dev, void *data,
					 struct drm_file *file)
{
	struct virtio_gpu_device *vgdev = dev->dev_private;
	struct virtio_gpu_fpriv *vfpriv = file->driver_priv;
	struct drm_virtgpu_limina_arena_import *args = data;
	struct virtio_gpu_limina_arena_import cmd = {};
	struct virtio_gpu_limina_resp_arena resp = {};
	struct virtio_gpu_limina_file *lf;
	struct drm_gem_object *obj;
	struct limina_arena *a;
	u32 res;
	int ret;

	if (!vgdev->has_limina)
		return -ENODEV;
	if (args->arena_id >= VIRTGPU_LIMINA_MAX_ARENAS)
		return -EINVAL;
	obj = drm_gem_object_lookup(file, args->handle);
	if (!obj)
		return -ENOENT;
	res = gem_to_virtio_gpu_obj(obj)->hw_res_handle;
	drm_gem_object_put(obj);
	lf = limina_file(vgdev, file);
	if (!lf)
		return -ENOMEM;
	a = kzalloc(sizeof(*a), GFP_KERNEL);
	if (!a)
		return -ENOMEM;

	mutex_lock(&lf->lock);
	ret = -ENOENT;
	if (!lf->created)
		goto err;
	ret = -EEXIST;
	if (limina_find(lf, args->arena_id))
		goto err;
	cmd.hdr.type = cpu_to_le32(VIRTIO_GPU_CMD_LIMINA_ARENA_IMPORT);
	cmd.hdr.ctx_id = cpu_to_le32(vfpriv->ctx_id);
	cmd.arena_id = cpu_to_le32(args->arena_id);
	cmd.resource_id = cpu_to_le32(res);
	ret = limina_call(vgdev, &cmd, sizeof(cmd), &resp, sizeof(resp),
			  VIRTIO_GPU_RESP_OK_LIMINA_ARENA);
	if (ret)
		goto err;
	a->id = args->arena_id;
	a->shm_offset = LIMINA_NOT_IN_WINDOW;
	a->size = le64_to_cpu(resp.size);
	a->gpu_base = le64_to_cpu(resp.gpu_base);
	a->shared = true;
	/* Imported memory is shared through the exporter's object, not a second one. */
	a->exported = true;
	atomic_set(&a->mappings, 0);
	list_add(&a->node, &lf->arenas);
	args->size = a->size;
	args->gpu_base = a->gpu_base;
	mutex_unlock(&lf->lock);
	return 0;
err:
	mutex_unlock(&lf->lock);
	kfree(a);
	return ret;
}

int virtio_gpu_limina_surface_create_ioctl(struct drm_device *dev, void *data,
					   struct drm_file *file)
{
	struct virtio_gpu_device *vgdev = dev->dev_private;
	struct virtio_gpu_fpriv *vfpriv = file->driver_priv;
	struct drm_virtgpu_limina_surface_create *args = data;
	struct virtio_gpu_limina_surface_create cmd = {};
	struct virtio_gpu_object_params params = {};
	struct virtio_gpu_ctrl_hdr resp = {};
	struct virtio_gpu_limina_file *lf;
	struct virtio_gpu_object *bo;
	struct drm_gem_object *obj;
	bool created;
	u32 handle;
	int ret;

	if (!vgdev->has_limina)
		return -ENODEV;
	/* The host checks the format and that the pitch is one it can lay rows out at. */
	if (!args->width || !args->height || args->stride / 4 < args->width ||
	    (u64)args->stride * args->height > SZ_1G)
		return -EINVAL;
	lf = limina_file(vgdev, file);
	if (!lf)
		return -ENOMEM;
	mutex_lock(&lf->lock);
	created = lf->created;
	mutex_unlock(&lf->lock);
	if (!created)
		return -ENOENT;

	/* A blob of host3d memory, for RESOURCE_INFO; shareable, never mappable. */
	params.size = (u64)args->stride * args->height;
	params.blob = true;
	params.ctx_id = vfpriv->ctx_id;
	params.blob_mem = VIRTGPU_BLOB_MEM_HOST3D;
	params.blob_flags = VIRTGPU_BLOB_FLAG_USE_SHAREABLE;
	ret = virtio_gpu_vram_init(vgdev, &params, &bo);
	if (ret)
		return ret;
	bo->host3d_blob = true;
	bo->blob_mem = params.blob_mem;
	bo->blob_flags = params.blob_flags;
	obj = &bo->base.base;

	cmd.hdr.type = cpu_to_le32(VIRTIO_GPU_CMD_LIMINA_SURFACE_CREATE);
	cmd.hdr.ctx_id = cpu_to_le32(vfpriv->ctx_id);
	cmd.resource_id = cpu_to_le32(bo->hw_res_handle);
	cmd.width = cpu_to_le32(args->width);
	cmd.height = cpu_to_le32(args->height);
	cmd.drm_fourcc = cpu_to_le32(args->drm_fourcc);
	cmd.stride = cpu_to_le32(args->stride);
	ret = limina_call(vgdev, &cmd, sizeof(cmd), &resp, sizeof(resp),
			  VIRTIO_GPU_RESP_OK_NODATA);
	/* Made or refused, the unref that freeing the object sends is what returns its id. */
	bo->created = true;
	if (ret) {
		drm_gem_object_put(obj);
		return ret;
	}
	ret = drm_gem_handle_create(file, obj, &handle);
	/* The handle owns the object now, or nothing does and this frees it. */
	drm_gem_object_put(obj);
	if (ret)
		return ret;
	args->handle = handle;
	args->res_handle = bo->hw_res_handle;
	return 0;
}

int virtio_gpu_limina_surface_import_ioctl(struct drm_device *dev, void *data,
					   struct drm_file *file)
{
	struct virtio_gpu_device *vgdev = dev->dev_private;
	struct virtio_gpu_fpriv *vfpriv = file->driver_priv;
	struct drm_virtgpu_limina_surface_import *args = data;
	struct virtio_gpu_limina_surface_import cmd = {};
	struct virtio_gpu_limina_resp_surface resp = {};
	struct virtio_gpu_limina_file *lf;
	struct drm_gem_object *obj;
	u32 res_handle;
	bool created;
	int ret;

	if (!vgdev->has_limina)
		return -ENODEV;
	lf = limina_file(vgdev, file);
	if (!lf)
		return -ENOMEM;
	mutex_lock(&lf->lock);
	created = lf->created;
	mutex_unlock(&lf->lock);
	if (!created)
		return -ENOENT;
	/* Looked up in this file's table, so a context can only ask for what it was given. */
	obj = drm_gem_object_lookup(file, args->handle);
	if (!obj)
		return -ENOENT;
	res_handle = gem_to_virtio_gpu_obj(obj)->hw_res_handle;
	drm_gem_object_put(obj);

	cmd.hdr.type = cpu_to_le32(VIRTIO_GPU_CMD_LIMINA_SURFACE_IMPORT);
	cmd.hdr.ctx_id = cpu_to_le32(vfpriv->ctx_id);
	cmd.resource_id = cpu_to_le32(res_handle);
	ret = limina_call(vgdev, &cmd, sizeof(cmd), &resp, sizeof(resp),
			  VIRTIO_GPU_RESP_OK_LIMINA_SURFACE);
	if (ret)
		return ret;
	args->width = le32_to_cpu(resp.width);
	args->height = le32_to_cpu(resp.height);
	args->drm_fourcc = le32_to_cpu(resp.drm_fourcc);
	args->stride = le32_to_cpu(resp.stride);
	args->res_handle = res_handle;
	return 0;
}

int virtio_gpu_limina_fence_ioctl(struct drm_device *dev, void *data, struct drm_file *file)
{
	struct virtio_gpu_device *vgdev = dev->dev_private;
	struct virtio_gpu_fpriv *vfpriv = file->driver_priv;
	struct drm_virtgpu_limina_fence *args = data;
	struct virtio_gpu_object_array *objs = NULL;
	struct virtio_gpu_limina_file *lf;
	struct virtio_gpu_fence *fence;
	struct sync_file *sync_file;
	int fd = -1;
	bool created, tracked;
	int ret;

	if (!vgdev->has_limina)
		return -ENODEV;
	if (args->flags & ~VIRTGPU_LIMINA_FENCE_FD || args->pad)
		return -EINVAL;
	lf = limina_file(vgdev, file);
	if (!lf)
		return -ENOMEM;
	mutex_lock(&lf->lock);
	created = lf->created;
	mutex_unlock(&lf->lock);
	if (!created)
		return -ENOENT;

	if (args->num_handles) {
		u32 *handles = vmemdup_array_user(u64_to_user_ptr(args->handles),
						  args->num_handles, sizeof(u32));

		if (IS_ERR(handles))
			return PTR_ERR(handles);
		objs = virtio_gpu_array_from_handles(file, handles, args->num_handles);
		kvfree(handles);
		if (!objs)
			return -ENOENT;
		ret = virtio_gpu_array_lock_resv(objs);
		if (ret) {
			virtio_gpu_array_put_free(objs);
			return ret;
		}
	}
	fence = virtio_gpu_fence_alloc(vgdev, lf->fence_ctx, 0);
	if (!fence) {
		ret = -ENOMEM;
		goto err_objs;
	}
	/* One client asking for fences that never come would otherwise hold every descriptor of the
	 * control queue, and every other command, the display's too, would wait behind them.
	 */
	mutex_lock(&lf->lock);
	tracked = limina_fence_track(lf, &fence->f);
	mutex_unlock(&lf->lock);
	if (!tracked) {
		ret = -EBUSY;
		goto err_fence;
	}
	if (args->flags & VIRTGPU_LIMINA_FENCE_FD) {
		fd = get_unused_fd_flags(O_CLOEXEC);
		if (fd < 0) {
			ret = fd;
			goto err_untrack;
		}
	}
	/* From here the command owns `objs`, and the reservations get unlocked with the fence in. */
	ret = virtio_gpu_cmd_limina_fence(vgdev, vfpriv->ctx_id, args->seqno, objs, fence);
	if (ret) {
		mutex_lock(&lf->lock);
		limina_fence_untrack(lf, &fence->f);
		mutex_unlock(&lf->lock);
		goto err_fd;
	}
	if (fd >= 0) {
		sync_file = sync_file_create(&fence->f);
		if (!sync_file) {
			ret = -ENOMEM;
			goto err_fd;
		}
		fd_install(fd, sync_file->file);
		args->fence_fd = fd;
	}
	dma_fence_put(&fence->f);
	return 0;

err_fd:
	if (fd >= 0)
		put_unused_fd(fd);
	dma_fence_put(&fence->f);
	return ret;
err_untrack:
	mutex_lock(&lf->lock);
	limina_fence_untrack(lf, &fence->f);
	mutex_unlock(&lf->lock);
err_fence:
	dma_fence_put(&fence->f);
err_objs:
	if (objs) {
		virtio_gpu_array_unlock_resv(objs);
		virtio_gpu_array_put_free(objs);
	}
	return ret;
}

void virtio_gpu_limina_postclose(struct virtio_gpu_device *vgdev,
				 struct virtio_gpu_fpriv *vfpriv)
{
	struct virtio_gpu_limina_file *lf = vfpriv->limina;
	struct virtio_gpu_limina_ctx_create cmd = {};
	struct virtio_gpu_ctrl_hdr resp = {};
	struct limina_arena *a, *tmp;

	if (!lf)
		return;
	/* No mapping outlives the file, so every arena is free to go; the host drops them with the
	 * context.
	 */
	if (lf->created) {
		cmd.hdr.type = cpu_to_le32(VIRTIO_GPU_CMD_LIMINA_CTX_DESTROY);
		cmd.hdr.ctx_id = cpu_to_le32(vfpriv->ctx_id);
		limina_call(vgdev, &cmd, sizeof(cmd.hdr), &resp, sizeof(resp),
			    VIRTIO_GPU_RESP_OK_NODATA);
	}
	list_for_each_entry_safe(a, tmp, &lf->arenas, node) {
		list_del(&a->node);
		kfree(a);
	}
	while (lf->npending)
		dma_fence_put(lf->pending[--lf->npending]);
	mutex_destroy(&lf->lock);
	kfree(lf);
	vfpriv->limina = NULL;
}

static void limina_vm_open(struct vm_area_struct *vma)
{
	struct limina_arena *a = vma->vm_private_data;

	atomic_inc(&a->mappings);
}

static void limina_vm_close(struct vm_area_struct *vma)
{
	struct limina_arena *a = vma->vm_private_data;

	atomic_dec(&a->mappings);
}

static const struct vm_operations_struct limina_arena_vm_ops = {
	.open = limina_vm_open,
	.close = limina_vm_close,
};

int virtio_gpu_limina_mmap(struct file *filp, struct vm_area_struct *vma)
{
	struct drm_file *file = filp->private_data;
	struct drm_device *dev = file->minor->dev;
	struct virtio_gpu_device *vgdev = dev->dev_private;
	struct virtio_gpu_fpriv *vfpriv = file->driver_priv;
	u64 off = (u64)vma->vm_pgoff << PAGE_SHIFT;
	unsigned long size = vma->vm_end - vma->vm_start;
	struct virtio_gpu_limina_file *lf;
	struct limina_arena *a;
	phys_addr_t phys;
	u64 id, rel;
	int ret;

	if (off < LIMINA_MMAP_BASE)
		return drm_gem_mmap(filp, vma);
	if (drm_dev_is_unplugged(dev))
		return -ENODEV;
	if (!vgdev->has_limina || !vfpriv || !vfpriv->limina)
		return -EINVAL;
	lf = vfpriv->limina;

	mutex_lock(&lf->lock);
	ret = -EINVAL;
	if (off == LIMINA_MMAP_DOORBELL) {
		if (!lf->created || size != PAGE_SIZE)
			goto out;
		phys = vgdev->limina_doorbells.addr + (u64)lf->doorbell * LIMINA_DOORBELL_STRIDE;
		vm_flags_set(vma, VM_IO | VM_PFNMAP | VM_DONTEXPAND | VM_DONTDUMP | VM_DONTCOPY);
		vma->vm_page_prot = pgprot_noncached(vma->vm_page_prot);
		ret = io_remap_pfn_range(vma, vma->vm_start, phys >> PAGE_SHIFT, size,
					 vma->vm_page_prot);
		goto out;
	}
	if (off < LIMINA_MMAP_ARENAS)
		goto out;
	id = (off - LIMINA_MMAP_ARENAS) / LIMINA_ARENA_STRIDE;
	/* Userspace sub-allocates arenas, so a mapping may start anywhere inside one. */
	rel = (off - LIMINA_MMAP_ARENAS) % LIMINA_ARENA_STRIDE;
	a = id < VIRTGPU_LIMINA_MAX_ARENAS ? limina_find(lf, id) : NULL;
	if (!a || a->shared || rel >= a->size || size > a->size - rel)
		goto out;
	phys = vgdev->limina_arenas.addr + a->shm_offset + rel;
	vm_flags_set(vma, VM_PFNMAP | VM_DONTEXPAND | VM_DONTDUMP | VM_DONTCOPY);
	vma->vm_ops = &limina_arena_vm_ops;
	vma->vm_private_data = a;
	ret = remap_pfn_range(vma, vma->vm_start, phys >> PAGE_SHIFT, size, vma->vm_page_prot);
	if (!ret)
		atomic_inc(&a->mappings);
out:
	mutex_unlock(&lf->lock);
	return ret;
}
