/* What does virtio-gpu's RESOURCE_INFO report for a PRIME-imported udmabuf?
 *
 * Mesa gates the SET_TYPE command (which is what makes the host give the
 * blob a real texture) on info.blob_mem being nonzero.  This probe walks the
 * exact path glupload's DirectDmabuf uploader takes and prints the answer.
 *
 * cc -o resinfo-probe resinfo-probe.c -ldrm -I/usr/include/libdrm
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/memfd.h>
#include <linux/udmabuf.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <unistd.h>
#include <xf86drm.h>
#include <drm/virtgpu_drm.h>

int main(void)
{
	int udma = open("/dev/udmabuf", O_RDWR);
	if (udma < 0) { perror("open /dev/udmabuf"); return 1; }

	size_t size = 64 * 64 * 4;
	int mfd = memfd_create("probe", MFD_ALLOW_SEALING);
	if (mfd < 0) { perror("memfd_create"); return 1; }
	if (ftruncate(mfd, size)) { perror("ftruncate"); return 1; }
	if (fcntl(mfd, F_ADD_SEALS, F_SEAL_SHRINK)) { perror("F_SEAL_SHRINK"); return 1; }

	struct udmabuf_create create = { .memfd = mfd, .flags = UDMABUF_FLAGS_CLOEXEC,
	                                 .offset = 0, .size = size };
	int dbuf = ioctl(udma, UDMABUF_CREATE, &create);
	if (dbuf < 0) { perror("UDMABUF_CREATE"); return 1; }
	printf("udmabuf fd=%d size=%zu\n", dbuf, size);

	int drm = open("/dev/dri/renderD128", O_RDWR);
	if (drm < 0) { perror("open renderD128"); return 1; }

	uint32_t handle = 0;
	if (drmPrimeFDToHandle(drm, dbuf, &handle)) { perror("PRIME_FD_TO_HANDLE"); return 1; }
	printf("imported: gem handle=%u\n", handle);

	struct drm_virtgpu_resource_info info;
	memset(&info, 0, sizeof(info));
	info.bo_handle = handle;
	if (drmIoctl(drm, DRM_IOCTL_VIRTGPU_RESOURCE_INFO, &info)) {
		perror("VIRTGPU_RESOURCE_INFO"); return 1;
	}
	printf("RESOURCE_INFO: res_handle=%u size=%u blob_mem=%u\n",
	       info.res_handle, info.size, info.blob_mem);
	printf("mesa would set maybe_untyped=%s -> SET_TYPE %s emitted\n",
	       info.blob_mem ? "true" : "false",
	       info.blob_mem ? "IS" : "is NOT");
	return 0;
}
