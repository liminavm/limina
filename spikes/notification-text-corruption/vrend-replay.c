// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva
//
// Replay a captured vrend command stream through libvirglrenderer on the HOST, with no VM.
//
// WHY THIS EXISTS. glyphmimic reproduced gnome-shell's failing draw field-for-field -- same
// target, same attachments, same bindings, same counts -- and never lost a pixel, in a session
// that was damaging 16 of 16 real cards. So the trigger is not in the shape of the draw, and
// hand-writing GL that "looks like" the shell's is exhausted. This feeds vrend the shell's ACTUAL
// bytes instead: the exact command batches it decoded, in order, against resources created with
// the exact arguments it saw, holding the exact transfer contents it was given.
//
// What that buys, if it reproduces: a deterministic, VM-free vehicle where Apple's Metal capture
// works (it segfaults on the VM's command stream) and a bisect costs seconds instead of a boot.
//
// READ THE VERDICT CAREFULLY. A replay POSITIVE is gold. A replay NEGATIVE is weak: the stream
// does not carry live state (host GL objects created before the capture window, the second
// KosmicKrisp device the worker hosts for venus, real timing), so a clean replay routes to the
// two-device hypothesis rather than to "the capture is unfaithful". Do not read it as exoneration.
//
//   vrend-replay <dump> [--ctx N] [--loops N] [--nodraw] [--readback RES]
//
//   --ctx N       which virgl context to replay (default: the one with the most records)
//   --loops N     replay the captured stream N times (default 1). A single pass proves little
//                 about an intermittent fault.
//   --nodraw      POSITIVE CONTROL: drop the glyph DRAW_VBO from every batch. Readback must then
//                 report text-lost, or the oracle cannot say "lost" and no clean verdict means
//                 anything.
//   --readback R  resource to score (default: the colour attachment of the last glyph draw).
//
// Run it under the same KosmicKrisp/zink env as the worker -- mimic-host.sh carries it.
#include "virglrenderer.h"
#include "virgl_hw.h"

#include <errno.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/uio.h>

#define TRACE_MAGIC 0x4c4d5654u

enum { T_SUBMIT = 1, T_CMD = 2, T_DRAW_FB = 3, T_TRANSFER = 4, T_FENCE = 5, T_RETIRE = 6,
       T_PAD = 7, T_XFERDATA = 9 };
enum { RES_CREATE = 0, RES_BLOB = 1, RES_UNREF = 2 };

#define VIRGL_CCMD_CREATE_OBJECT        1
#define VIRGL_CCMD_SET_FRAMEBUFFER_STATE 5
#define VIRGL_CCMD_SET_VERTEX_BUFFERS    6
#define VIRGL_CCMD_DRAW_VBO              8
#define VIRGL_CCMD_COPY_TRANSFER3D       45
/* dword index of the SOURCE resource handle inside a COPY_TRANSFER3D payload */
#define VIRGL_COPY_TRANSFER3D_SRC_RES_HANDLE 12
/* dword index of the source OFFSET, which is the offset the payload record carries */
#define VIRGL_COPY_TRANSFER3D_SRC_RES_OFFSET 13
/* dword index of the DESTINATION handle, which is what the payload record is keyed by */
#define VIRGL_COPY_TRANSFER3D_DST_RES_HANDLE 1
#define VIRGL_OBJECT_SURFACE             8

struct rec_hdr {
   uint32_t total_len;
   uint8_t  type, cmd;
   uint16_t ctx_id;
   uint64_t seq, mono_ns;
   uint32_t payload_len, aux_count;
};

struct res_ev {
   uint64_t seq;
   uint32_t kind, handle, target, format, bind;
   uint32_t width, height, depth, array_size, last_level, nr_samples, flags;
};

/* One host-side backing store per replayed resource. The capture records transfer CONTENTS, so
 * the replayer never reconstructs blob or iov machinery: it hands every resource a plain malloc'd
 * iov and memcpys the recorded bytes in before the batch that reads them. */
struct backing {
   uint32_t handle;
   uint8_t *mem;
   size_t   size;
   struct iovec iov;
   bool     live;
};

/* An array of POINTERS, never of structs. vrend stores the `struct iovec *` it is handed and
 * dereferences it for the resource's whole life, so a backing's iovec must never move. Holding
 * the backings by value in a realloc'd array left every previously attached resource pointing
 * into freed memory the moment the array grew past its capacity -- which is why a run would
 * sometimes sail through and sometimes report `src iov_len=0`, poison the context on one failed
 * copy, and then fail every submit and readback after it in silence. */
static struct backing **backings;
static uint32_t backing_n, backing_cap;

static struct backing *backing_find(uint32_t handle)
{
   for (uint32_t i = 0; i < backing_n; i++)
      if (backings[i]->live && backings[i]->handle == handle)
         return backings[i];
   return NULL;
}

static struct backing *backing_add(uint32_t handle, size_t size)
{
   if (backing_n == backing_cap) {
      backing_cap = backing_cap ? backing_cap * 2 : 256;
      backings = realloc(backings, backing_cap * sizeof *backings);
      if (!backings) { fprintf(stderr, "OOM\n"); exit(2); }
   }
   struct backing *b = calloc(1, sizeof *b);
   if (!b) { fprintf(stderr, "OOM\n"); exit(2); }
   backings[backing_n++] = b;
   b->handle = handle;
   b->size = size ? size : 4096;
   b->mem = calloc(1, b->size);
   if (!b->mem) { fprintf(stderr, "OOM\n"); exit(2); }
   b->iov.iov_base = b->mem;
   b->iov.iov_len = b->size;
   b->live = true;
   return b;
}

static void write_fence(void *cookie, uint32_t fence) { (void)cookie; (void)fence; }

static struct virgl_renderer_callbacks cbs = {
   .version = VIRGL_RENDERER_CALLBACKS_VERSION,
   .write_fence = write_fence,
};

/* A generous over-estimate of a resource's byte size. The capture records the create arguments
 * but not the host's chosen layout, and an iov that is too small makes vrend reject transfers
 * with ILLEGAL_RESOURCE -- which would read as a replay fault rather than a sizing mistake. */
static size_t res_bytes(const struct res_ev *r)
{
   uint64_t w = r->width ? r->width : 1;
   uint64_t h = r->height ? r->height : 1;
   uint64_t d = r->depth ? r->depth : 1;
   uint64_t a = r->array_size ? r->array_size : 1;
   if (r->kind == RES_BLOB)
      return (size_t)(((uint64_t)r->height << 32) | r->width);   /* size was split across two words */
   /* target 0 is PIPE_BUFFER: width is already bytes. Otherwise assume at most 16 bytes/texel
    * and add the mip tail. */
   if (r->target == 0)
      return (size_t)w;
   return (size_t)(w * h * d * a * 16u + 65536u);
}

/* Walk the stream once to find the colour resource behind the LAST glyph-pipeline draw. Two
 * reasons this has to be a pre-pass rather than a running guess: the shell abandons a card's
 * resources, so by the end of the stream the interesting one has been unref'd and cannot be read
 * back unless the replay is told in advance to keep it alive; and "which draw was last" is not
 * knowable until the file is exhausted. */
static uint32_t scan_last_glyph_colour(const uint8_t *blob, size_t flen, size_t off, int ctx)
{
   uint32_t surf_res[65536];
   uint32_t fb_cbuf0 = 0, found = 0;
   bool glyph_shaped = false;
   memset(surf_res, 0, sizeof surf_res);

   for (size_t p = off; p + sizeof(struct rec_hdr) <= flen; ) {
      struct rec_hdr h;
      memcpy(&h, blob + p, sizeof h);
      if (h.total_len < sizeof h || p + h.total_len > flen) break;
      if (h.ctx_id != (uint16_t)ctx) { p += h.total_len; continue; }
      const uint8_t *pay = blob + p + sizeof h + (size_t)h.aux_count * 4;
      const uint32_t *dw = (const uint32_t *)pay;
      size_t ndw = h.payload_len / 4;

      if (h.type == T_CMD && ndw >= 2) {
         if (h.cmd == VIRGL_CCMD_CREATE_OBJECT && ndw >= 3 &&
             ((dw[0] >> 8) & 0xff) == VIRGL_OBJECT_SURFACE && dw[1] < 65536)
            surf_res[dw[1]] = dw[2];
         else if (h.cmd == VIRGL_CCMD_SET_FRAMEBUFFER_STATE && ndw >= 4)
            fb_cbuf0 = dw[3];
         else if (h.cmd == VIRGL_CCMD_SET_VERTEX_BUFFERS) {
            size_t n = (ndw - 1) / 3;
            glyph_shaped = false;
            if (n == 3)
               for (size_t k = 0; k < n; k++)
                  if (dw[1 + 3 * k] == 0) glyph_shaped = true;
         } else if (h.cmd == VIRGL_CCMD_DRAW_VBO && glyph_shaped && fb_cbuf0 < 65536 &&
                    surf_res[fb_cbuf0])
            found = surf_res[fb_cbuf0];
      }
      p += h.total_len;
   }
   return found;
}


/* Score at the resource's UNREF, not after the whole stream.
 *
 * The obvious alternative -- suppress the unref so the resource survives to the end -- is not
 * neutral: virgl handles are REUSED, so keeping one alive makes a later create collide and fail,
 * and everything downstream of that resource silently stops rendering. That is how the first
 * scoring attempt read "no ink" from a card the bug is known to spare. The unref is the last
 * moment the resource is both complete and still alive, so read there and let the stream run on
 * exactly as captured. */
static uint32_t readback_res;
static int scored, scored_ink;
static int want_ctx = -1;


/* The payload records name the COPY_TRANSFER3D's DESTINATION, and the copy path never reads the
 * destination's iov -- it reads the SOURCE's. vrend captures the bytes inside
 * vrend_renderer_transfer_write_iov, which the copy reaches as
 * transfer_write_iov(dst_res, src_res->iov, ...), so the payload is keyed by dst while the
 * offset and the bytes both belong to src. Seeding the destination therefore puts every
 * copy-fed upload in a buffer nothing reads and leaves the real source at zeros, which is
 * indistinguishable from a draw whose vertices are legitimately blank.
 *
 * A record carries no source handle, but it does not need one: command records are emitted
 * AFTER their dispatch, so the next CMD in the same context is the command that produced this
 * payload. Reading the source handle out of it needs no recapture. */
static int copy_src_of_next_cmd(const uint8_t *blob, size_t flen, size_t p,
                                int want_ctx, uint32_t want_dst, uint64_t want_off,
                                uint32_t *src_handle, uint32_t *mismatches)
{
   for (; p + sizeof(struct rec_hdr) <= flen; ) {
      struct rec_hdr h;
      memcpy(&h, blob + p, sizeof h);
      if (h.total_len < sizeof h || p + h.total_len > flen) return 0;
      if (h.ctx_id == (uint16_t)want_ctx && h.type == T_CMD) {
         if (h.cmd != VIRGL_CCMD_COPY_TRANSFER3D) return 0;
         const uint32_t *dw = (const uint32_t *)(blob + p + sizeof h + (size_t)h.aux_count * 4);
         if (h.payload_len / 4 <= VIRGL_COPY_TRANSFER3D_SRC_RES_OFFSET) return 0;
         /* Prove this copy is the producer rather than assuming it. The record is keyed by the
          * copy's destination and carries the copy's source offset, so both must agree. Without
          * the check an API-path transfer -- which has no following command of its own -- would
          * silently adopt the next unrelated copy and land its bytes in a stranger's buffer, a
          * drift that grows over a run and would be easy to mistake for the fault under study. */
         if (dw[VIRGL_COPY_TRANSFER3D_DST_RES_HANDLE] != want_dst
             || dw[VIRGL_COPY_TRANSFER3D_SRC_RES_OFFSET] != (uint32_t)want_off) {
            (*mismatches)++;
            return 0;
         }
         *src_handle = dw[VIRGL_COPY_TRANSFER3D_SRC_RES_HANDLE];
         return 1;
      }
      p += h.total_len;
   }
   return 0;
}

static void score_resource(const struct res_ev *ev)
{
   uint32_t w = ev->width ? ev->width : 1, h = ev->height ? ev->height : 1;
   size_t need = (size_t)w * h * 4;
   uint8_t *px = calloc(1, need);
   struct iovec riov = { .iov_base = px, .iov_len = need };
   struct virgl_box box = { .x = 0, .y = 0, .z = 0, .w = w, .h = h, .d = 1 };

   /* force_ctx_0 is load-bearing: without it the readback segfaults. It is also why scoring is
    * only trustworthy in a FULL --sweep. It switches the renderer away from the replayed context
    * and the stream never switches back, so batches after the first score are damaged; a full
    * sweep scores from the very start and reads each resource before that matters, while a sparse
    * one (--sweep-w, or a single --readback) submits hundreds of batches after the switch and
    * reports 373 submit errors. Narrowing the readbacks needs this fixed first. */
   virgl_renderer_force_ctx_0();
   int rr = virgl_renderer_transfer_read_iov(ev->handle, (uint32_t)want_ctx, 0, w * 4, 0,
                                             &box, 0, &riov, 1);
   if (rr) {
      fprintf(stderr, "readback of res %u failed: %d\n", ev->handle, rr);
      free(px);
      return;
   }

   size_t ink = 0;
   for (size_t i = 0; i < need; i += 4)
      if (px[i] | px[i + 1] | px[i + 2] | px[i + 3]) ink++;

   printf("VERDICT: resource %u %ux%u, %zu of %zu pixels have ink -- %s\n",
          ev->handle, w, h, ink, need / 4, ink ? "TEXT PRESENT" : "TEXT LOST");

   /* An ink COUNT cannot say which offscreen is the header and which is the body, and that
    * mapping is what any verdict about "the title is lost" rests on. Dump the pixels and look. */
   const char *dir = getenv("REPLAY_DUMP_DIR");
   const char *only = getenv("REPLAY_DUMP_W");
   if (dir && (!only || w == (uint32_t)atoi(only))) {
      char fn[512];
      snprintf(fn, sizeof fn, "%s/res%u_%ux%u.rgba", dir, ev->handle, w, h);
      FILE *f = fopen(fn, "wb");
      if (f) { fwrite(px, 1, need, f); fclose(f); }
   }
   scored = 1;
   scored_ink = ink != 0;
   free(px);
}

int main(int argc, char **argv)
{
   const char *path = NULL;
   int loops = 1;
   bool nodraw = false;
   bool sweep = false;
   uint64_t draws_from = 0;
   uint32_t sweep_w = 0;

   bool no_unref = getenv("REPLAY_NO_UNREF") != NULL;
   uint32_t watch = getenv("REPLAY_WATCH") ? (uint32_t)atoi(getenv("REPLAY_WATCH")) : 0;

   for (int i = 1; i < argc; i++) {
      if (!strcmp(argv[i], "--ctx") && i + 1 < argc) want_ctx = atoi(argv[++i]);
      else if (!strcmp(argv[i], "--sweep")) sweep = true;
      else if (!strcmp(argv[i], "--sweep-w") && i + 1 < argc)
         sweep_w = (uint32_t)atoi(argv[++i]);
      else if (!strcmp(argv[i], "--draws-from") && i + 1 < argc)
         draws_from = strtoull(argv[++i], NULL, 10);
      else if (!strcmp(argv[i], "--loops") && i + 1 < argc) loops = atoi(argv[++i]);
      else if (!strcmp(argv[i], "--nodraw")) nodraw = true;
      else if (!strcmp(argv[i], "--readback") && i + 1 < argc) readback_res = (uint32_t)atoi(argv[++i]);
      else if (argv[i][0] != '-') path = argv[i];
   }
   if (!path) { fprintf(stderr, "usage: vrend-replay <dump> [--ctx N] [--loops N] [--nodraw]\n"); return 2; }

   FILE *f = fopen(path, "rb");
   if (!f) { perror(path); return 2; }
   fseek(f, 0, SEEK_END);
   long flen = ftell(f);
   fseek(f, 0, SEEK_SET);
   uint8_t *blob = malloc((size_t)flen);
   if (!blob || fread(blob, 1, (size_t)flen, f) != (size_t)flen) { fprintf(stderr, "short read\n"); return 2; }
   fclose(f);

   uint32_t head[16];
   memcpy(head, blob, sizeof head);
   if (head[0] != TRACE_MAGIC) { fprintf(stderr, "not a vrend trace dump\n"); return 2; }
   if (head[1] < 2) {
      fprintf(stderr, "trace is version %u: it has no resource log, so it cannot be replayed.\n"
                      "Recapture with a tracer built from this tree.\n", head[1]);
      return 2;
   }
   if (head[13]) fprintf(stderr, "WARNING: resource log overflowed; this trace is NOT replayable\n");

   uint32_t res_n = head[12];
   struct res_ev *res = (struct res_ev *)(blob + 64);
   size_t off = 64 + (size_t)res_n * sizeof(struct res_ev);
   printf("replay: %u resource events, ring %u MB, %u evicted\n", res_n, head[2], head[6]);
   if (head[6]) fprintf(stderr, "WARNING: %u records evicted; the window does not reach the start\n", head[6]);

   /* Pick the busiest context if not told. */
   if (want_ctx < 0) {
      uint32_t count[65536];
      memset(count, 0, sizeof count);
      for (size_t p = off; p + sizeof(struct rec_hdr) <= (size_t)flen; ) {
         struct rec_hdr h;
         memcpy(&h, blob + p, sizeof h);
         if (h.total_len < sizeof h || p + h.total_len > (size_t)flen) break;
         if (h.type == T_CMD) count[h.ctx_id]++;
         p += h.total_len;
      }
      uint32_t best = 0;
      for (uint32_t i = 0; i < 65536; i++) if (count[i] > count[best]) best = i;
      want_ctx = (int)best;
      printf("replay: no --ctx given, picking ctx %d (%u commands)\n", want_ctx, count[best]);
   }

   int flags = VIRGL_RENDERER_USE_EGL | VIRGL_RENDERER_USE_SURFACELESS | VIRGL_RENDERER_USE_GLES;
   /* The cookie must be non-NULL: virglrenderer rejects the vrend path outright with "invalid
    * renderer vrend callbacks" when it is null, whatever the callbacks contain. It is opaque to
    * the library and only handed back to our callbacks, so any live address will do. */
   static int cookie;
   int ret = virgl_renderer_init(&cookie, flags, &cbs);
   if (ret) { fprintf(stderr, "virgl_renderer_init failed: %d\n", ret); return 2; }
   printf("replay: virglrenderer initialised (flags 0x%x)\n", flags);

   const char *name = "limina-replay";
   ret = virgl_renderer_context_create((uint32_t)want_ctx, (uint32_t)strlen(name), name);
   if (ret) { fprintf(stderr, "context_create failed: %d\n", ret); return 2; }

   if (!readback_res) {
      readback_res = scan_last_glyph_colour(blob, (size_t)flen, off, want_ctx);
      if (!readback_res) {
         fprintf(stderr, "no glyph-pipeline draw found in ctx %d -- nothing to score\n", want_ctx);
         return 2;
      }
      printf("replay: scoring resource %u (colour attachment of the last glyph draw)\n", readback_res);
   }

   for (int loop = 0; loop < loops; loop++) {
      uint32_t next_res = 0;
      /* Batch assembly: CMD records between two SUBMITs form one submit_cmd call. */
      uint32_t *batch = NULL;
      size_t batch_dw = 0, batch_cap = 0;
      uint32_t submits = 0, cmds = 0, xfers = 0, dropped = 0, copy_fed = 0, copy_bad = 0;
      uint32_t made = 0, failed = 0, unrefs = 0;
      bool batch_watch = false;

      for (size_t p = off; p + sizeof(struct rec_hdr) <= (size_t)flen; ) {
         struct rec_hdr h;
         memcpy(&h, blob + p, sizeof h);
         if (h.total_len < sizeof h || p + h.total_len > (size_t)flen) break;
         const uint32_t *aux = (const uint32_t *)(blob + p + sizeof h);
         const uint8_t *pay = blob + p + sizeof h + (size_t)h.aux_count * 4;

         if (h.ctx_id != (uint16_t)want_ctx) { p += h.total_len; continue; }

         switch (h.type) {
         case T_SUBMIT:
            if (watch && batch_watch)
               fprintf(stderr, "[watch] submitting batch with a TRANSFER3D for res=%u at seq %llu\n",
                       watch, (unsigned long long)h.seq);
            batch_watch = false;
            if (batch_dw) {
               if (virgl_renderer_submit_cmd(batch, want_ctx, (int)batch_dw))
                  dropped++;
               submits++;
               batch_dw = 0;
            }
            break;
         case T_CMD: {
            size_t dw = h.payload_len / 4;
            if (nodraw && h.cmd == VIRGL_CCMD_DRAW_VBO) break;   /* the positive control */
            /* --draws-from is the bounded form of --nodraw, and it asks the one question the
             * replay was built for: is the fault ACCUMULATED? Every resource, transfer and state
             * command still runs, so a later card is set up exactly as captured -- only the
             * earlier cards' rasterisation is removed. If that card then renders its header, the
             * damage is carried by work done before it rather than by its own stream. */
            if (draws_from && h.cmd == VIRGL_CCMD_DRAW_VBO && h.seq < draws_from) break;
            if (batch_dw + dw > batch_cap) {
               batch_cap = (batch_dw + dw) * 2;
               batch = realloc(batch, batch_cap * 4);
               if (!batch) { fprintf(stderr, "OOM\n"); return 2; }
            }
            memcpy(batch + batch_dw, pay, dw * 4);
            /* CCMD 43 is TRANSFER3D; its payload dword 1 is the resource handle. */
            if (watch && h.cmd == 43 && dw > 1 && ((const uint32_t *)pay)[1] == watch)
               batch_watch = true;
            batch_dw += dw;
            cmds++;
            break;
         }
         case T_XFERDATA: {
            /* Land the recorded bytes in the resource's backing store BEFORE the batch that
             * reads them -- which is why this is applied at its recorded position in the stream
             * and not hoisted. */
            uint32_t handle = h.aux_count > 0 ? aux[0] : 0;
            uint64_t xoff = h.aux_count > 2 ? ((uint64_t)aux[2] << 32 | aux[1]) : 0;
            uint32_t src = 0;
            if (copy_src_of_next_cmd(blob, (size_t)flen, p + h.total_len, want_ctx,
                                     handle, xoff, &src, &copy_bad)) {
               handle = src;                 /* see copy_src_of_next_cmd */
               copy_fed++;
            }
            struct backing *b = backing_find(handle);
            if (b && xoff + h.payload_len <= b->size) {
               memcpy(b->mem + xoff, pay, h.payload_len);
               xfers++;
            }
            break;
         }
         default:
            break;
         }

         /* Resource events are applied AFTER the record, not before it. A SUBMIT record marks
          * the START of a batch in the capture, so the commands that follow it belong to it --
          * which means the buffered batch above must be handed to vrend before any create or
          * UNREF carrying this record's sequence is applied. Doing it the other way round runs
          * a batch after the unref of a resource it references, and vrend rejects the whole
          * batch with "Illegal resource" -- a defect of the replay that reads exactly like a
          * capture too incomplete to replay. */
         while (next_res < res_n && res[next_res].seq <= h.seq) {
            struct res_ev *r = &res[next_res++];
            if (r->kind == RES_UNREF) {
               /* --sweep scores EVERY colour offscreen at its unref. One run, and it answers the
                * question a single-resource verdict cannot: whether the replay renders at all.
                * A verdict of "lost" is only worth reading once some sibling comes back "present". */
               if (sweep && r->handle != readback_res) {
                  const struct res_ev *born = NULL;
                  for (uint32_t k = 0; k < res_n; k++)
                     if (res[k].handle == r->handle && res[k].kind == RES_CREATE
                         && res[k].seq < r->seq)
                        born = &res[k];
                  /* target 2 is a 2D texture; format 20 is D24S8, which carries no ink. */
                  /* --sweep-w narrows the readbacks to one width. Needed for the llvmpipe arm:
                   * reading every offscreen back trips an assert inside llvmpipe on resources
                   * that have nothing to do with the cards, and aborts the run before it reaches
                   * them. Scoring only the width under study keeps the A/B possible. */
                  if (born && born->target == 2 && born->format != 20 && born->width > 8
                      && (!sweep_w || born->width == sweep_w))
                     score_resource(born);
               }
               if (r->handle == readback_res) {
                  const struct res_ev *born = NULL;
                  for (uint32_t k = 0; k < res_n; k++)
                     if (res[k].handle == readback_res && res[k].kind != RES_UNREF
                         && res[k].seq < r->seq)
                        born = &res[k];
                  if (born) score_resource(born);
               }
               if (no_unref) continue;
               struct backing *b = backing_find(r->handle);
               if (b) { virgl_renderer_resource_unref(r->handle); b->live = false; free(b->mem); b->mem = NULL; unrefs++; }
               continue;
            }
            struct backing *b = backing_add(r->handle, res_bytes(r));
            if (r->kind == RES_BLOB) {
               /* A guest-memory blob is just shared pages; the command stream reads it through
                * its iov exactly like any other resource, so a plain resource with a backing
                * store is a faithful stand-in and needs no get_blob plumbing. */
               struct virgl_renderer_resource_create_args a = {
                  .handle = r->handle, .target = 0, .format = 64 /* R8_UNORM */,
                  .bind = 0x10 /* VIRGL_BIND_VERTEX_BUFFER */, .width = (uint32_t)b->size,
                  .height = 1, .depth = 1, .array_size = 1, .nr_samples = 0, .last_level = 0, .flags = 0,
               };
               int cr = virgl_renderer_resource_create(&a, NULL, 0);
               if (cr) {
                  if (failed < 5)
                     fprintf(stderr, "create BLOB res=%u size=%zu failed: %d\n", r->handle, b->size, cr);
                  b->live = false; failed++; continue;
               }
               made++;
            } else {
               struct virgl_renderer_resource_create_args a = {
                  .handle = r->handle, .target = r->target, .format = r->format, .bind = r->bind,
                  .width = r->width, .height = r->height, .depth = r->depth,
                  .array_size = r->array_size, .nr_samples = r->nr_samples,
                  .last_level = r->last_level, .flags = r->flags,
               };
               int cr = virgl_renderer_resource_create(&a, NULL, 0);
               if (cr) {
                  if (failed < 5)
                     fprintf(stderr, "create res=%u target=%u fmt=%u bind=0x%x %ux%ux%u array=%u "
                             "levels=%u samples=%u iov=%zu failed: %d\n",
                             r->handle, r->target, r->format, r->bind, r->width, r->height,
                             r->depth, r->array_size, r->last_level, r->nr_samples, b->size, cr);
                  b->live = false; failed++; continue;
               }
               made++;
            }
            if (watch && r->handle == watch) {
               /* set_priv/get_priv round-trip is a registration probe: both go through
                * virgl_resource_lookup, so a NULL read back means the handle is not registered
                * even though create returned success. */
               virgl_renderer_resource_set_priv(r->handle, (void *)(uintptr_t)0xf00d);
               void *pv = virgl_renderer_resource_get_priv(r->handle);
               fprintf(stderr, "[watch] create res=%u at res-seq %llu, applied at record seq %llu, "
                       "registered=%s\n",
                       r->handle, (unsigned long long)r->seq, (unsigned long long)h.seq,
                       pv ? "yes" : "NO");
            }
            /* The iov MUST arrive here and not through create. create stores it on the VIRGL
             * resource and stops -- its own signature marks the parameter UNUSED -- while vrend's
             * resource, the one every transfer actually reads, is fed only by this call. Worse,
             * the two are mutually exclusive: virgl_resource_attach_iov refuses with EINVAL when
             * an iov is already set, so passing it to create silently BLOCKS the attach that
             * reaches vrend. The resource then creates, registers and attaches cleanly, and every
             * TRANSFER3D touching it fails check_transfer_iovec -- reported as the very same
             * "Illegal resource" as a handle the context has never heard of. */
            virgl_renderer_resource_attach_iov((int)r->handle, &b->iov, 1);
            virgl_renderer_ctx_attach_resource(want_ctx, (int)r->handle);
         }

         p += h.total_len;
      }

      if (batch_dw && virgl_renderer_submit_cmd(batch, want_ctx, (int)batch_dw))
         dropped++;
      free(batch);
      printf("loop %d: %u resources created (%u failed), %u unrefs, %u submits, %u commands, "
             "%u transfers applied (%u redirected to a copy source, %u unmatched), "
             "%u submit errors\n",
             loop, made, failed, unrefs, submits, cmds, xfers, copy_fed, copy_bad, dropped);
   }

   if (!scored)
      fprintf(stderr, "scored resource %u was never unref'd in the trace; nothing read back\n",
              readback_res);
   if (nodraw && scored_ink)
      printf("  !! POSITIVE CONTROL FAILED: draws were dropped and pixels still arrived; the "
             "oracle is not measuring what it claims\n");
   return scored_ink ? 0 : 1;
}
