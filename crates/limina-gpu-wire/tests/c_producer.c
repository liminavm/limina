/* The C producer half of tests/c_producer.rs: a separate process that maps the same file as the
 * Rust consumer and pushes through include/limina_gpu_ring.h.
 *
 * The file is the ring region followed by four u32 words: ready (set by us once the ring is
 * initialised), attached (set by the consumer), kicks (bumped by us each time publish says the
 * consumer is parked) and done (set by us after the last publish). */
#include <fcntl.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <unistd.h>

#include "limina_gpu_ring.h"

enum { W_READY, W_ATTACHED, W_KICKS, W_DONE };

static uint32_t *words;

static void
publish(struct lmn_ring *r)
{
   if (lmn_ring_publish(r))
      __atomic_fetch_add(&words[W_KICKS], 1, __ATOMIC_SEQ_CST);
}

static void
push(struct lmn_ring *r, uint16_t op, const struct lmn_iov *iov, unsigned n)
{
   for (;;) {
      enum lmn_push_result res = lmn_ring_pushv(r, op, iov, n);
      if (res == LMN_PUSH_OK)
         return;
      if (res != LMN_PUSH_FULL) {
         fprintf(stderr, "push: %d\n", res);
         exit(2);
      }
      publish(r);
      sched_yield();
   }
}

int
main(int argc, char **argv)
{
   if (argc != 4)
      return 64;
   uint32_t data_size = (uint32_t)strtoul(argv[2], NULL, 0);
   uint64_t count = strtoull(argv[3], NULL, 0);
   size_t region = LMN_RING_DATA_OFFSET + data_size;
   int fd = open(argv[1], O_RDWR);
   if (fd < 0)
      return 66;
   uint8_t *map = mmap(NULL, region + 16, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
   if (map == MAP_FAILED)
      return 71;
   words = (uint32_t *)(void *)(map + region);

   struct lmn_ring r;
   lmn_ring_init(&r, map, data_size);
   __atomic_store_n(&words[W_READY], 1, __ATOMIC_RELEASE);
   while (!__atomic_load_n(&words[W_ATTACHED], __ATOMIC_ACQUIRE))
      sched_yield();

   uint8_t name[64];
   for (uint64_t i = 0; i < count; i++) {
      struct lmn_fill_buffer fill = {
         .buffer = {.arena = 1, .offset = i},
         .length = 64,
         .value = (uint32_t)i,
      };
      struct lmn_iov f = {&fill, sizeof(fill)};
      push(&r, LMN_OP_FILL_BUFFER, &f, 1);

      struct lmn_commit commit = {.seqno = i};
      uint32_t extra = (uint32_t)(i % 37);
      for (uint32_t j = 0; j < extra; j++)
         name[j] = (uint8_t)(i + j);
      struct lmn_iov c[2] = {{&commit, sizeof(commit)}, {name, extra}};
      push(&r, LMN_OP_COMMIT, c, 2);

      /* Publish at irregular intervals, so the consumer sees batches of every size and parks
       * between some of them. */
      if (i % 7 == 0 || i % 11 == 0)
         publish(&r);
      if (i % 1000 == 0)
         usleep(200);
   }
   publish(&r);
   __atomic_store_n(&words[W_DONE], 1, __ATOMIC_RELEASE);
   return 0;
}
