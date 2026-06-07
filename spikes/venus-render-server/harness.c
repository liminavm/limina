// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Standalone reproduction of the limina 1.3.0 venus context-create failure.
//
// Mirrors exactly what libkrun's rutabaga does on the GPU worker thread:
//   virgl_renderer_init(cookie, VENUS|NO_VIRGL|RENDER_SERVER, callbacks_v3)
//   virgl_renderer_context_create_with_flags(ctx_id=1, capset_id=4 /*venus*/, ...)
//
// The point is to reproduce `proxy: failed to pre-initialize context` /
// `server: socket disconnected` / CtxCreate ENOMEM WITHOUT booting a VM, so we
// can bisect which step fails and run it under lldb. virglrenderer logs to
// stderr by default, so proxy_log/render_log lines show up directly.
//
// Build + run: see run.sh

#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include "virglrenderer.h"

static void cb_write_fence(void *cookie, uint32_t fence) {
   (void)cookie;
   fprintf(stderr, "[harness] write_fence %u\n", fence);
}

static void cb_write_context_fence(void *cookie, uint32_t ctx_id, uint32_t ring_idx,
                                   uint64_t fence_id) {
   (void)cookie;
   fprintf(stderr, "[harness] write_context_fence ctx=%u ring=%u id=%llu\n", ctx_id,
           ring_idx, (unsigned long long)fence_id);
}

static int cb_get_server_fd(void *cookie, uint32_t version) {
   (void)cookie;
   (void)version;
   // SAME_PROCESS thread mode ignores this; return -1 like rutabaga does.
   return -1;
}

int main(void) {
   struct virgl_renderer_callbacks cbs;
   memset(&cbs, 0, sizeof(cbs));
   cbs.version = 3;
   cbs.write_fence = cb_write_fence;
   cbs.write_context_fence = cb_write_context_fence;
   cbs.get_server_fd = cb_get_server_fd;

   int cookie = 0;
   int flags = VIRGL_RENDERER_VENUS | VIRGL_RENDERER_NO_VIRGL | VIRGL_RENDERER_RENDER_SERVER;
   fprintf(stderr, "[harness] virgl_renderer_init flags=0x%x\n", flags);
   int ret = virgl_renderer_init(&cookie, flags, &cbs);
   fprintf(stderr, "[harness] virgl_renderer_init -> %d\n", ret);
   if (ret) {
      fprintf(stderr, "[harness] init failed, abort\n");
      return 1;
   }

   // Probe the venus capset like the guest does (this part worked already).
   uint32_t cap_ver = 0, cap_size = 0;
   virgl_renderer_get_cap_set(4 /*venus*/, &cap_ver, &cap_size);
   fprintf(stderr, "[harness] venus capset: version=%u max_size=%u\n", cap_ver, cap_size);

   // The failing call: create a venus (capset_id=4) context.
   const char *name = "gpu_renderer";
   uint32_t ctx_flags = 4; // CAPSET_ID = 4 (venus), masked by 0xff
   fprintf(stderr, "[harness] virgl_renderer_context_create_with_flags ctx=1 flags=%u\n",
           ctx_flags);
   ret = virgl_renderer_context_create_with_flags(1, ctx_flags, (uint32_t)strlen(name), name);
   fprintf(stderr, "[harness] context_create_with_flags -> %d (%s)\n", ret,
           ret == 0 ? "OK" : strerror(-ret));
   if (ret) {
      fprintf(stderr, "[harness] FAILED to create venus context\n");
      return 2;
   }

   fprintf(stderr, "[harness] SUCCESS: venus context created\n");
   virgl_renderer_context_destroy(1);
   return 0;
}
