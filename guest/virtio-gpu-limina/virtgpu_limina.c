// SPDX-License-Identifier: MIT
/*
 * limina's native-context extension of virtio-gpu.
 *
 * Behind VIRTIO_GPU_F_LIMINA_EXT the device offers two more shared-memory regions and a handful of
 * control commands. A guest context gets a doorbell page (stores to it reach the host renderer on
 * the vCPU thread, without a virtqueue) and arenas (host GPU memory mapped into a window of guest
 * physical space). Userspace writes its commands into a ring inside an arena and rings the
 * doorbell only when the host says it is asleep; nothing here is on that path.
 *
 * This file is what the kernel adds: it creates contexts and arenas on the host for each open
 * file, and maps them into the file's process. Display and the upstream 3D paths are untouched.
 */

#include <linux/mm.h>
#include <linux/slab.h>

#include <drm/drm_drv.h>
#include <drm/drm_file.h>
#include <drm/drm_gem.h>
#include <drm/drm_print.h>

#include "virtgpu_drv.h"
#include "virtgpu_limina_drm.h"

#define VIRTIO_GPU_SHM_ID_LIMINA_ARENAS		0x80
#define VIRTIO_GPU_SHM_ID_LIMINA_DOORBELLS	0x81
/* One 16 KiB slot per context, so 4 KiB and 16 KiB guests both map a page of their own. */
#define LIMINA_DOORBELL_STRIDE			(16 * 1024)

#define VIRTIO_GPU_CMD_LIMINA_CTX_CREATE	0x4c00
#define VIRTIO_GPU_CMD_LIMINA_CTX_DESTROY	0x4c01
#define VIRTIO_GPU_CMD_LIMINA_ARENA_CREATE	0x4c02
#define VIRTIO_GPU_CMD_LIMINA_ARENA_DESTROY	0x4c03
#define VIRTIO_GPU_CMD_LIMINA_RING_ATTACH	0x4c04
#define VIRTIO_GPU_RESP_OK_LIMINA_CTX		0x1180
#define VIRTIO_GPU_RESP_OK_LIMINA_ARENA		0x1181

/*
 * mmap offsets live far above the GEM fake-offset range, so drm_gem_mmap keeps everything below
 * LIMINA_MMAP_BASE. Arena n is at LIMINA_MMAP_ARENAS + n * LIMINA_ARENA_STRIDE; a mapping may
 * start at any page inside it.
 */
#define LIMINA_MMAP_BASE	(1ULL << 50)
#define LIMINA_MMAP_DOORBELL	LIMINA_MMAP_BASE
#define LIMINA_MMAP_ARENAS	(LIMINA_MMAP_BASE + (1ULL << 34))
#define LIMINA_ARENA_STRIDE	(1ULL << 34)

struct virtio_gpu_limina_ctx_create {
	struct virtio_gpu_ctrl_hdr hdr;
	__le32 flags;
	__le32 padding;
};

struct virtio_gpu_limina_arena_create {
	struct virtio_gpu_ctrl_hdr hdr;
	__le32 arena_id;
	__le32 padding;
	__le64 size;
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
};

struct virtio_gpu_limina_file {
	struct mutex lock;
	bool created;
	u32 doorbell;
	struct list_head arenas;
};

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
	if (args->arena_id >= VIRTGPU_LIMINA_MAX_ARENAS || !args->size)
		return -EINVAL;
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
	cmd.size = cpu_to_le64(args->size);
	ret = limina_call(vgdev, &cmd, sizeof(cmd), &resp, sizeof(resp),
			  VIRTIO_GPU_RESP_OK_LIMINA_ARENA);
	if (ret)
		goto err;
	off = le64_to_cpu(resp.shm_offset);
	size = le64_to_cpu(resp.size);
	if (!PAGE_ALIGNED(off) || !PAGE_ALIGNED(size) || size > LIMINA_ARENA_STRIDE ||
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
	args->mmap_offset = LIMINA_MMAP_ARENAS + (u64)a->id * LIMINA_ARENA_STRIDE;
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
	if (!a || rel >= a->size || size > a->size - rel)
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
