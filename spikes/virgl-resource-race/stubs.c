/* The few symbols virgl_resource.c pulls in that the harness does not exercise.
 * hash_func_u32/equal_func are copied from virgl_util.c so the harness does not have to drag
 * the logging and tracing machinery in behind it. */
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

#include "util/u_pointer.h"

uint32_t
hash_func_u32(const void *key)
{
   intptr_t ip = pointer_to_intptr(key);
   return (uint32_t)(ip & 0xffffffff);
}

bool
equal_func(const void *key1, const void *key2)
{
   return key1 == key2;
}

/* Paths the harness never reaches; present only so the link resolves. */
void
_debug_assert_fail(const char *expr, const char *file, unsigned line, const char *function)
{
   fprintf(stderr, "assert failed: %s at %s:%u in %s\n", expr, file, line, function);
   abort();
}

struct virgl_context *
virgl_context_lookup(uint32_t ctx_id)
{
   (void)ctx_id;
   return NULL;
}
