// The Objective-C runtime declarations Apple's metal-cpp headers include, for Linux. The functions
// are limina-metal's (shim/src/runtime.rs).
#pragma once
#include <stddef.h>

// metal-cpp knows arm64 only by Apple's name for it, and refuses to build without it
// (NSObject.hpp, doesRequireMsgSendStret). With it, every return comes back through plain
// objc_msgSend, as AAPCS64 has it.
#if defined(__aarch64__) && !defined(__arm64__)
#define __arm64__ 1
#endif

#ifdef __cplusplus
extern "C" {
#endif

typedef struct objc_class* Class;
// metal-cpp's NS::Object derives from it.
struct objc_object {
    Class isa;
};
typedef struct objc_object* id;
typedef const struct objc_selector* SEL;
typedef struct objc_protocol Protocol;
typedef void (*IMP)(void);

SEL sel_registerName(const char* name);
const char* sel_getName(SEL sel);
Class objc_lookUpClass(const char* name);
Class objc_getClass(const char* name);
Protocol* objc_getProtocol(const char* name);

#ifdef __cplusplus
}
#endif
