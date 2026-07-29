// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

/* crossmark — cross-API graphics tier probe: the SAME scene rendered by a GL
 * backend (crossmark_gl.c, EGL + GLES 3.x) and a Vulkan backend
 * (crossmark_vk.c), so every guest tier gets a same-workload score:
 *
 *   GL     -> vrend            (virgl tier)
 *   GL     -> zink-on-venus    (enhanced GL tier)
 *   Vulkan -> venus            (enhanced Vulkan tier)
 *   GL     -> zink-on-KK       (host-native GL reference)
 *   Vulkan -> KK               (host-native Vulkan reference)
 *
 * Workload shapes (-S), same semantics in both backends:
 *   draws   - N flat triangles, one uniform/push-constant update per draw
 *             (command-stream throughput; the drawstorm shape)
 *   state   - N draws cycling 8 program/pipeline variants (bind + uniform +
 *             draw; where GL state -> pipeline translation hurts)
 *   upload  - per frame: stream 1 MiB of texels through a buffer into a
 *             512x512 texture (PBO / staging copy), then 100 quads sampling it
 *   desktop - a compositor-ish frame: 6 large + 40 small textured quads + clear
 *
 * This header holds everything both backends must agree on: CLI, timing, the
 * per-draw scene parameters (deterministic functions of frame/draw index so
 * the two backends render identical frames), and the readback pixel hash.
 */

#ifndef CROSSMARK_H
#define CROSSMARK_H

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#define CM_WIDTH 1024
#define CM_HEIGHT 1024
#define CM_NVARIANTS 8       /* program/pipeline variants for -S state */
#define CM_UPLOAD_TEX 512    /* streaming texture is 512x512 RGBA = 1 MiB */
#define CM_UPLOAD_DRAWS 100
#define CM_DESKTOP_LARGE 6
#define CM_DESKTOP_SMALL 40

enum cm_shape {
   CM_SHAPE_DRAWS,
   CM_SHAPE_STATE,
   CM_SHAPE_UPLOAD,
   CM_SHAPE_DESKTOP,
};

struct cm_opts {
   enum cm_shape shape;
   int ndraws; /* draws/state only; upload/desktop have fixed counts */
   int nframes;
   int warmup;
   int hash; /* readback + FNV hash after the last frame */
};

static inline const char *
cm_shape_name(enum cm_shape s)
{
   switch (s) {
   case CM_SHAPE_DRAWS:
      return "draws";
   case CM_SHAPE_STATE:
      return "state";
   case CM_SHAPE_UPLOAD:
      return "upload";
   default:
      return "desktop";
   }
}

static inline int
cm_parse_args(struct cm_opts *o, int argc, char **argv)
{
   o->shape = CM_SHAPE_DRAWS;
   o->ndraws = 1000;
   o->nframes = 300;
   o->warmup = 30;
   o->hash = 1;
   for (int i = 1; i < argc; i++) {
      if (!strcmp(argv[i], "-S") && i + 1 < argc) {
         const char *s = argv[++i];
         if (!strcmp(s, "draws"))
            o->shape = CM_SHAPE_DRAWS;
         else if (!strcmp(s, "state"))
            o->shape = CM_SHAPE_STATE;
         else if (!strcmp(s, "upload"))
            o->shape = CM_SHAPE_UPLOAD;
         else if (!strcmp(s, "desktop"))
            o->shape = CM_SHAPE_DESKTOP;
         else {
            fprintf(stderr, "unknown shape %s\n", s);
            return -1;
         }
      } else if (!strcmp(argv[i], "-n") && i + 1 < argc)
         o->ndraws = atoi(argv[++i]);
      else if (!strcmp(argv[i], "-f") && i + 1 < argc)
         o->nframes = atoi(argv[++i]);
      else if (!strcmp(argv[i], "-w") && i + 1 < argc)
         o->warmup = atoi(argv[++i]);
      else if (!strcmp(argv[i], "-H"))
         o->hash = 0;
      else {
         fprintf(stderr,
                 "usage: %s [-S draws|state|upload|desktop] [-n draws] "
                 "[-f frames] [-w warmup] [-H (skip hash)]\n",
                 argv[0]);
         return -1;
      }
   }
   if (o->shape == CM_SHAPE_UPLOAD)
      o->ndraws = CM_UPLOAD_DRAWS;
   else if (o->shape == CM_SHAPE_DESKTOP)
      o->ndraws = CM_DESKTOP_LARGE + CM_DESKTOP_SMALL;
   return 0;
}

static inline double
cm_now_ms(void)
{
   struct timespec ts;
   clock_gettime(CLOCK_MONOTONIC, &ts);
   return ts.tv_sec * 1e3 + ts.tv_nsec / 1e6;
}

/* Per-draw scene parameters. params[0..3] = rect (xy offset in NDC, zw
 * scale), params[4..7] = tint. Both backends feed these to the identical
 * shader interface, so frames are bit-comparable. */
static inline void
cm_draw_params(enum cm_shape shape, int frame, int i, float p[8])
{
   switch (shape) {
   case CM_SHAPE_DRAWS:
   case CM_SHAPE_STATE:
      p[0] = -0.9f + 1.8f * (float)(i & 1023) / 1024.0f;
      p[1] = -0.9f + 1.8f * (float)((i >> 4) & 1023) / 1024.0f;
      p[2] = 0.02f;
      p[3] = 0.02f;
      p[4] = (float)(i % 7) / 7.0f;
      p[5] = (float)(i % 5) / 5.0f;
      p[6] = 1.0f - (float)(i % 3) / 3.0f;
      p[7] = 1.0f;
      break;
   case CM_SHAPE_UPLOAD: {
      int gx = i % 10, gy = i / 10;
      p[0] = -1.0f + 0.2f * gx;
      p[1] = -1.0f + 0.2f * gy;
      p[2] = 0.19f;
      p[3] = 0.19f;
      p[4] = p[5] = p[6] = p[7] = 1.0f;
      break;
   }
   case CM_SHAPE_DESKTOP:
      if (i < CM_DESKTOP_LARGE) {
         /* large overlapping windows */
         p[0] = -0.95f + 0.18f * i;
         p[1] = -0.9f + 0.12f * i;
         p[2] = 1.1f;
         p[3] = 1.3f;
         p[4] = p[5] = p[6] = p[7] = 1.0f;
      } else {
         /* small widgets along the top */
         int k = i - CM_DESKTOP_LARGE;
         p[0] = -0.98f + 0.049f * k;
         p[1] = 0.85f;
         p[2] = 0.04f;
         p[3] = 0.1f;
         p[4] = 0.8f;
         p[5] = 0.9f;
         p[6] = 1.0f;
         p[7] = 1.0f;
      }
      break;
   }
}

/* Which texture a draw samples (tex shapes only): desktop large quads
 * alternate the two static textures, small widgets share texture 1; upload
 * samples the streaming texture (index 2). */
static inline int
cm_draw_texture(enum cm_shape shape, int i)
{
   if (shape == CM_SHAPE_UPLOAD)
      return 2;
   return i < CM_DESKTOP_LARGE ? (i & 1) : 1;
}

/* Deterministic texel patterns (static textures + the per-frame stream). */
static inline void
cm_fill_texture(uint8_t *rgba, int w, int h, int which)
{
   for (int y = 0; y < h; y++)
      for (int x = 0; x < w; x++) {
         uint8_t *p = rgba + 4 * (y * w + x);
         p[0] = (uint8_t)(x * (which + 1));
         p[1] = (uint8_t)(y * (which + 2));
         p[2] = (uint8_t)((x ^ y) + which * 37);
         p[3] = 255;
      }
}

static inline void
cm_fill_stream(uint8_t *rgba, int frame)
{
   /* cheap per-frame variation; the memcpy/dma is what we measure */
   uint32_t seed = 0x9e3779b9u * (uint32_t)(frame + 1);
   uint32_t *px = (uint32_t *)rgba;
   for (int i = 0; i < CM_UPLOAD_TEX * CM_UPLOAD_TEX; i++)
      px[i] = seed + (uint32_t)i * 2654435761u;
}

static inline uint64_t
cm_hash_pixels(const uint8_t *rgba, size_t n)
{
   uint64_t h = 1469598103934665603ull;
   for (size_t i = 0; i < n; i++) {
      h ^= rgba[i];
      h *= 1099511628211ull;
   }
   return h;
}

struct cm_times {
   double draw, flush, sync, total;
   int frames;
};

static inline void
cm_report(const char *api, const struct cm_opts *o, const char *device,
          const struct cm_times *t, uint64_t hash)
{
   double per = t->frames ? 1.0 / t->frames : 0;
   printf("crossmark api=%s shape=%s n=%d frames=%d device=\"%s\"\n", api,
          cm_shape_name(o->shape), o->ndraws, t->frames, device);
   printf("per-frame ms: draw=%.3f flush=%.3f sync=%.3f total=%.3f (%.1f fps)\n",
          t->draw * per, t->flush * per, t->sync * per, t->total * per,
          t->frames / (t->total / 1e3));
   if (o->hash)
      printf("pixel-hash: %016llx\n", (unsigned long long)hash);
}

#endif
