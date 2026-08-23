/* Observe-only tracer for the virtio-gpu import path in the guest.
 *
 * Logs the two ioctls that decide whether a PRIME-imported dmabuf becomes a
 * typed host texture: the import itself, and the RESOURCE_INFO Mesa reads to
 * set `maybe_untyped`. Unlike blobmem-shim.c it changes nothing -- use it to
 * see what the guest really does, then reason about which gate closed.
 *
 * ONE RESOURCE_INFO for TWO PRIME imports is the multi-planar signature: the
 * planes share a dmabuf, so only the first allocates the virgl_hw_res.
 *
 * Whether SET_TYPE actually went out is read on the HOST, in the worker log --
 * not here. An earlier version of this file decoded the EXECBUFFER command
 * stream looking for VIRGL_CCMD_PIPE_RESOURCE_SET_TYPE and stayed silent even
 * for imports the host demonstrably typed, so it was removed: a lying oracle
 * costs more than a missing one.
 *
 * cc -shared -fPIC -O2 -o virtgpu-trace.so virtgpu-trace.c -ldl
 * LD_PRELOAD=./virtgpu-trace.so gst-launch-1.0 ...
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/ioctl.h>

/* Match on type+nr only: the ioctl structs grow over time, so the encoded size
 * is not a stable part of the request number. */
#define IOC_TYPE(r) (((r) >> 8) & 0xff)
#define IOC_NR(r)   ((r) & 0xff)
#define IS_DRM(r)   (IOC_TYPE(r) == 'd')
#define NR_PRIME_FD_TO_HANDLE 0x2e
#define NR_VIRTGPU_RES_INFO   (0x40 + 0x05)

struct prime_handle { uint32_t handle; uint32_t flags; int32_t fd; };
struct res_info { uint32_t bo_handle, res_handle, size, blob_mem; };

static int (*real_ioctl)(int, unsigned long, ...);

int ioctl(int fd, unsigned long request, ...)
{
	va_list ap;
	void *arg;

	va_start(ap, request);
	arg = va_arg(ap, void *);
	va_end(ap);

	if (!real_ioctl)
		real_ioctl = dlsym(RTLD_NEXT, "ioctl");

	int ret = real_ioctl(fd, request, arg);
	if (ret || !IS_DRM(request) || !arg)
		return ret;

	if (IOC_NR(request) == NR_PRIME_FD_TO_HANDLE) {
		struct prime_handle *p = arg;
		fprintf(stderr, "[TRACE] PRIME_FD_TO_HANDLE fd %d -> handle %u\n",
		        p->fd, p->handle);
	} else if (IOC_NR(request) == NR_VIRTGPU_RES_INFO) {
		struct res_info *r = arg;
		fprintf(stderr, "[TRACE] RESOURCE_INFO handle %u -> res %u size %u "
		        "blob_mem %u\n", r->bo_handle, r->res_handle, r->size,
		        r->blob_mem);
	}

	return ret;
}
