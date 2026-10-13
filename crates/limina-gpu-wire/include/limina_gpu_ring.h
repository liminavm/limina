/* SPDX-License-Identifier: MIT
 *
 * The guest end of limina's command ring, in C: a line-for-line port of `Producer` in
 * crates/limina-gpu-wire/src/ring.rs, which is the definition. tests/c_producer.rs drives this
 * file against the Rust consumer, parking protocol included.
 *
 * One producer per ring; the caller serialises pushes. Nothing pushed is visible to the host until
 * lmn_ring_publish(), which returns whether the host has parked and needs a doorbell.
 */
#ifndef LIMINA_GPU_RING_H
#define LIMINA_GPU_RING_H

#include <stdbool.h>
#include <stdint.h>
#include <string.h>

#include "limina_gpu_wire.h"

struct lmn_ring {
   uint8_t *ctrl;
   uint8_t *data;
   uint64_t size;
   uint64_t tail;
   uint64_t head_seen;
};

struct lmn_iov {
   const void *base;
   uint32_t len;
};

enum lmn_push_result {
   LMN_PUSH_OK = 0,
   /* Not enough free space now: publish, kick if needed, wait for the host's head, retry. */
   LMN_PUSH_FULL = 1,
   /* Larger than half the data area, so it can never fit. */
   LMN_PUSH_TOO_LARGE = 2,
};

static inline uint64_t *
lmn_ring_u64(const struct lmn_ring *r, uint32_t off)
{
   return (uint64_t *)(void *)(r->ctrl + off);
}

static inline uint32_t *
lmn_ring_u32(const struct lmn_ring *r, uint32_t off)
{
   return (uint32_t *)(void *)(r->ctrl + off);
}

static inline bool
lmn_ring_valid_data_size(uint32_t data_size)
{
   return data_size >= LMN_RING_MIN_DATA_SIZE && (data_size & (data_size - 1)) == 0;
}

/* `region` is LMN_RING_DATA_OFFSET + data_size bytes, 128-aligned, owned by this producer. */
static inline void
lmn_ring_init(struct lmn_ring *r, void *region, uint32_t data_size)
{
   r->ctrl = (uint8_t *)region;
   r->data = r->ctrl + LMN_RING_DATA_OFFSET;
   r->size = data_size;
   r->tail = 0;
   r->head_seen = 0;
   memset(r->ctrl, 0, LMN_RING_DATA_OFFSET);
   *lmn_ring_u32(r, LMN_RING_OFF_MAGIC) = LMN_RING_MAGIC;
   *lmn_ring_u32(r, LMN_RING_OFF_VERSION) = LMN_WIRE_VERSION;
}

static inline uint32_t
lmn_ring_encoded_len(uint32_t payload_len)
{
   uint32_t len = (uint32_t)sizeof(struct lmn_cmd_header) + payload_len;
   return (len + LMN_RING_CMD_ALIGN - 1) & ~(LMN_RING_CMD_ALIGN - 1);
}

static inline uint8_t *
lmn_ring_reserve(struct lmn_ring *r, uint64_t len, enum lmn_push_result *res)
{
   if (len > r->size / 2) {
      *res = LMN_PUSH_TOO_LARGE;
      return NULL;
   }
   uint64_t pos = r->tail & (r->size - 1);
   uint64_t to_end = r->size - pos;
   uint64_t need = len > to_end ? to_end + len : len;
   if (r->tail + need - r->head_seen > r->size) {
      r->head_seen = __atomic_load_n(lmn_ring_u64(r, LMN_RING_OFF_HEAD), __ATOMIC_ACQUIRE);
      if (r->tail + need - r->head_seen > r->size) {
         *res = LMN_PUSH_FULL;
         return NULL;
      }
   }
   if (len > to_end) {
      struct lmn_cmd_header pad = {
         .op = LMN_OP_PAD,
         .flags = 0,
         .len = (uint32_t)to_end,
      };
      memcpy(r->data + pos, &pad, sizeof(pad));
      r->tail += to_end;
   }
   *res = LMN_PUSH_OK;
   return r->data + (r->tail & (r->size - 1));
}

/* Appends a command: `op`, then the concatenation of `iov[0..n]` (the payload struct first). */
static inline enum lmn_push_result
lmn_ring_pushv(struct lmn_ring *r, uint16_t op, const struct lmn_iov *iov, unsigned n)
{
   uint64_t payload = 0;
   for (unsigned i = 0; i < n; i++)
      payload += iov[i].len;
   if (payload > UINT32_MAX / 2)
      return LMN_PUSH_TOO_LARGE;
   uint32_t len = lmn_ring_encoded_len((uint32_t)payload);
   enum lmn_push_result res;
   uint8_t *p = lmn_ring_reserve(r, len, &res);
   if (!p)
      return res;
   struct lmn_cmd_header h = {.op = op, .flags = 0, .len = len};
   memcpy(p, &h, sizeof(h));
   uint8_t *q = p + sizeof(h);
   for (unsigned i = 0; i < n; i++) {
      memcpy(q, iov[i].base, iov[i].len);
      q += iov[i].len;
   }
   memset(q, 0, (size_t)(p + len - q));
   r->tail += len;
   return LMN_PUSH_OK;
}

static inline enum lmn_push_result
lmn_ring_push(struct lmn_ring *r, uint16_t op, const void *payload, uint32_t len)
{
   struct lmn_iov iov = {payload, len};
   return lmn_ring_pushv(r, op, &iov, 1);
}

/* Makes everything pushed visible; returns whether the host is parked and needs a doorbell. */
static inline bool
lmn_ring_publish(struct lmn_ring *r)
{
   __atomic_store_n(lmn_ring_u64(r, LMN_RING_OFF_TAIL), r->tail, __ATOMIC_RELEASE);
   __atomic_thread_fence(__ATOMIC_SEQ_CST);
   return __atomic_load_n(lmn_ring_u32(r, LMN_RING_OFF_STATE), __ATOMIC_RELAXED) ==
          LMN_RING_STATE_PARKED;
}

/* Bytes pushed so far, published or not. */
static inline uint64_t
lmn_ring_position(const struct lmn_ring *r)
{
   return r->tail;
}

/* Highest seqno the GPU has finished. */
static inline uint64_t
lmn_ring_completed(const struct lmn_ring *r)
{
   return __atomic_load_n(lmn_ring_u64(r, LMN_RING_OFF_COMPLETED), __ATOMIC_ACQUIRE);
}

/* Zero, or the LMN_ERROR_* that lost the context, with its ring position in `*at`. */
static inline uint32_t
lmn_ring_error(const struct lmn_ring *r, uint64_t *at)
{
   uint32_t e = __atomic_load_n(lmn_ring_u32(r, LMN_RING_OFF_ERROR), __ATOMIC_ACQUIRE);
   if (e && at)
      *at = __atomic_load_n(lmn_ring_u64(r, LMN_RING_OFF_ERROR_AT), __ATOMIC_ACQUIRE);
   return e;
}

/* Whether a reply the host fills has been marked done. */
static inline bool
lmn_reply_done(const struct lmn_reply *reply)
{
   return __atomic_load_n(&reply->done, __ATOMIC_ACQUIRE) == LMN_REPLY_DONE;
}

#endif /* LIMINA_GPU_RING_H */
