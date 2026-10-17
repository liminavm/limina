// What metal-cpp takes from CoreFoundation, for Linux.
#pragma once
// CoreFoundation.h brings these in on Darwin, and metal-cpp relies on it (INFINITY).
#include <math.h>
#include <stdint.h>

typedef double CFTimeInterval;
typedef const void* CFTypeRef;
typedef const struct __CFString* CFStringRef;
// Named only by metal-cpp's deprecated NS::MakeConstantString, which MLX does not call.
#define __CFStringMakeConstantString(cStr) ((CFStringRef)(cStr))
