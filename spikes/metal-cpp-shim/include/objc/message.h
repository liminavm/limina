// objc_msgSend, for Linux (shim/src/runtime.rs). Callers cast it to the method's real type, as on
// Darwin. On arm64 metal-cpp never takes the _stret or _fpret variants.
#pragma once
#include <objc/runtime.h>

#ifdef __cplusplus
extern "C" {
#endif

void objc_msgSend(void);

#ifdef __cplusplus
}
#endif
