/* virgl_resource_table race harness.
 *
 * Reproduces the limina-vmm crash of 2026-08-31 (SIGSEGV in
 * virgl_resource_forget_iosurface_cb): the global virgl_resource_table is walked from a venus
 * ring thread by virgl_resource_forget_iosurface(), while the virtio-gpu device thread creates
 * and removes entries in it. util_hash_table_remove() frees the node the walk is holding.
 *
 * Thread A is the device thread: virgl_resource_create_from_iov / virgl_resource_remove.
 * Thread B is the vkr ring thread: virgl_resource_forget_iosurface (one per vkDestroyImage).
 *
 * Built with -fsanitize=thread, so the verdict does not depend on catching a segfault: TSan
 * reports the first overlap. The harness never touches res->iosurface_id itself -- the field
 * write inside the callback is the point of the fix, not the race under test.
 */
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>

#include "virgl_resource.h"

#define N_RES 64
#define ITERS 20000

static atomic_bool stop;

static void
noop_unref(struct pipe_resource *pres, void *data)
{
   (void)pres;
   (void)data;
}

static void
noop_attach_iov(struct pipe_resource *pres, const struct iovec *iov, int iov_count, void *data)
{
   (void)pres; (void)iov; (void)iov_count; (void)data;
}

static void
noop_detach_iov(struct pipe_resource *pres, void *data)
{
   (void)pres; (void)data;
}

static void *
device_thread(void *arg)
{
   (void)arg;
   for (int i = 0; i < ITERS; i++) {
      uint32_t id = 1 + (uint32_t)(i % N_RES);
      struct virgl_resource *res = virgl_resource_create_from_iov(id, NULL, 0);
      if (res)
         res->iosurface_id = id;
      virgl_resource_remove(id);
   }
   atomic_store(&stop, true);
   return NULL;
}

static void *
ring_thread(void *arg)
{
   (void)arg;
   unsigned long walks = 0;
   while (!atomic_load(&stop)) {
      for (uint32_t id = 1; id <= N_RES; id++)
         virgl_resource_forget_iosurface(id);
      walks++;
   }
   printf("ring thread: %lu sweeps\n", walks);
   return NULL;
}

int
main(void)
{
   struct virgl_resource_pipe_callbacks cbs = {
      .data = NULL,
      .unref = noop_unref,
      .attach_iov = noop_attach_iov,
      .detach_iov = noop_detach_iov,
   };

   if (virgl_resource_table_init(&cbs)) {
      fprintf(stderr, "table init failed\n");
      return 1;
   }

   pthread_t dev, ring;
   pthread_create(&dev, NULL, device_thread, NULL);
   pthread_create(&ring, NULL, ring_thread, NULL);
   pthread_join(dev, NULL);
   pthread_join(ring, NULL);

   virgl_resource_table_cleanup();
   printf("done\n");
   return 0;
}
