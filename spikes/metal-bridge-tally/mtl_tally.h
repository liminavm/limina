/* SPDX-License-Identifier: MIT
 * Copyright © 2026 Gustavo Noronha Silva
 *
 * limina measurement spike (spikes/metal-bridge-tally): count every Metal bridge call, the
 * bytes it would put on a guest->host wire, and the time spent inside it. Armed by
 * LIMINA_KK_BRIDGE_TALLY=<seconds>; unset, each scope costs one load and one branch.
 * Generated into the tree by gen_tally.py -- never commit this to limina-kk. */
#pragma once

#include <stdint.h>
#include <string.h>

#ifdef __cplusplus
extern "C" {
#endif

enum mtl_tally_class {
   MTL_TALLY_ASYNC,
   MTL_TALLY_CREATE,
   MTL_TALLY_SYNC,
   MTL_TALLY_LOCAL,
   MTL_TALLY_DIAG,
   MTL_TALLY_NCLASS,
};

struct mtl_tally_scope {
   int idx; /* -1 when unarmed */
   uint64_t t0;
};

extern int mtl_tally_armed;
struct mtl_tally_scope mtl_tally_enter(int idx, uint64_t wire, uint64_t reply);
void mtl_tally_exit(struct mtl_tally_scope *s);

#define MTL_TALLY_SCOPE(idx, wire, reply)                                        \
   struct mtl_tally_scope _mtl_tally                                           \
      __attribute__((cleanup(mtl_tally_exit), unused)) =                       \
         __builtin_expect(mtl_tally_armed != 0, 0)                             \
            ? mtl_tally_enter((idx), (uint64_t)(wire), (uint64_t)(reply))      \
            : (struct mtl_tally_scope){-1, 0}

#ifdef __cplusplus
}
#endif
