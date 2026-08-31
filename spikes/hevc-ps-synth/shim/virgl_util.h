/* Test shim: the real header drags in meson-generated config we do not need here. */
#ifndef SHIM_VIRGL_UTIL_H
#define SHIM_VIRGL_UTIL_H
#include <stdio.h>
#define virgl_error(...) fprintf(stderr, __VA_ARGS__)
#endif
