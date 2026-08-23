/* Emulate the guest-kernel one-liner without rebuilding the kernel.
 *
 * virtgpu_dma_buf_init_obj() never sets bo->blob_mem, so RESOURCE_INFO reports
 * 0 for a PRIME-imported dmabuf and Mesa consequently never emits SET_TYPE.
 * This shim reports blob_mem=VIRTGPU_BLOB_MEM_GUEST for exactly the handles
 * that came back from a PRIME_FD_TO_HANDLE import, which is what the fixed
 * kernel would do -- so the whole downstream chain (maybe_untyped -> the caps
 * gate -> SET_TYPE on the wire -> vrend typing the resource) can be validated
 * in minutes instead of hours.
 *
 * cc -shared -fPIC -O2 -o blobmem-shim.so blobmem-shim.c -ldl
 * LD_PRELOAD=./blobmem-shim.so gst-launch-1.0 ...
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

/* Every term unsigned long: (3 << 30) overflows a signed int and sign-extends
 * when compared against the unsigned long request. */
#define _IOC_(d, t, nr, size) (((unsigned long)(d) << 30) | \
                               ((unsigned long)(size) << 16) | \
                               ((unsigned long)(t) << 8) | (unsigned long)(nr))
#define PRIME_FD_TO_HANDLE  _IOC_(3, 'd', 0x2e, 12)
#define VIRTGPU_RES_INFO    _IOC_(3, 'd', 0x40 + 0x05, 16)
#define BLOB_MEM_GUEST      1

struct prime_handle { uint32_t handle; uint32_t flags; int32_t fd; };
struct res_info { uint32_t bo_handle, res_handle, size, blob_mem; };

/* Imported handles are few (one per pool buffer); a flat set is plenty. */
static uint32_t imported[4096];
static unsigned n_imported;

static int (*real_ioctl)(int, unsigned long, ...);

static int quiet(void) { return getenv("BLOBMEM_SHIM_QUIET") != NULL; }

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
	if (ret)
		return ret;

	if (request == PRIME_FD_TO_HANDLE && arg) {
		struct prime_handle *p = arg;
		if (n_imported < sizeof(imported) / sizeof(imported[0]))
			imported[n_imported++] = p->handle;
		if (!quiet())
			fprintf(stderr, "[SHIM] imported dmabuf fd %d -> handle %u\n",
			        p->fd, p->handle);
	} else if (request == VIRTGPU_RES_INFO && arg) {
		struct res_info *r = arg;
		for (unsigned i = 0; i < n_imported; i++) {
			if (imported[i] != r->bo_handle)
				continue;
			if (!quiet())
				fprintf(stderr, "[SHIM] RESOURCE_INFO handle %u res %u: "
				        "blob_mem %u -> %u\n",
				        r->bo_handle, r->res_handle, r->blob_mem,
				        BLOB_MEM_GUEST);
			r->blob_mem = BLOB_MEM_GUEST;
			break;
		}
	}

	return ret;
}
