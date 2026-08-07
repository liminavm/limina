#!/usr/bin/env python3
"""vkbadusage — issue spec-INVALID Vulkan calls and prove the host survives them.

The guest command stream is untrusted, and a guest app's own bug must not be able to take
down the VM. It repeatedly has: mesa's common Vulkan runtime is full of `assert()`s on
values a guest controls, our KosmicKrisp build compiles them in, and an assert in the
render server aborts the whole worker process. That is a guest-triggerable host DoS
reached by nothing more exotic than a buggy app.

This is the vehicle for that class. Each arm makes one call that a conforming app would
never make, and the PASS condition is simply that we are still here afterwards — the
worker is a live process and the guest got an error rather than a dead VM. A driver is
allowed to reject any of these; it is not allowed to abort.

Arms (add one per incident — see `vkr_dispatch_*` in the virglrenderer fork for the
matching host-side filters):

  zero-buffer   vkCreateBuffer with size = 0. Invalid per
                VUID-VkBufferCreateInfo-size-00912. Aborted the dogfood host on
                2026-08-07: kk_CreateBuffer -> vk_buffer_create -> vk_buffer_init ->
                `assert(pCreateInfo->size > 0)` -> __assert_rtn -> SIGABRT on the
                vkr-ring thread. Filtered in `vkr_dispatch_vkCreateBuffer`.

Usage: vkbadusage.py <arm> [device-substring]

Output:
    DEVICE <name> | NODEV <sub>
    BADUSAGE ARM <arm>
    BADUSAGE RESULT <arm> <VkResult>     the driver's answer; ANY value is a pass
    BADUSAGE PASS <arm>                  we reached the end without dying
    BADUSAGE FAIL <stage> <detail>       the probe itself broke (not a host bug)

Note the asymmetry that makes this test work: the probe cannot observe its own success
beyond "it kept running", because the failure mode is the HOST dying. The real oracle is
on the host side — the worker must be the same live pid afterwards. See
`crates/limina-test/tests/venus_bad_usage.rs`.
"""
import ctypes as C
import sys

VK_SUCCESS = 0
API_1_1 = (1 << 22) | (1 << 12)

P = C.c_void_p
H = C.c_uint64

ST_APPLICATION_INFO = 0
ST_INSTANCE_CREATE_INFO = 1
ST_DEVICE_QUEUE_CREATE_INFO = 2
ST_DEVICE_CREATE_INFO = 3
ST_BUFFER_CREATE_INFO = 12

BUFFER_USAGE_TRANSFER_SRC = 0x1


def fail(stage, detail=""):
    print(f"BADUSAGE FAIL {stage} {detail}".rstrip(), flush=True)
    sys.exit(1)


class ApplicationInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32), ("pNext", P), ("pApplicationName", C.c_char_p),
        ("applicationVersion", C.c_uint32), ("pEngineName", C.c_char_p),
        ("engineVersion", C.c_uint32), ("apiVersion", C.c_uint32),
    ]


class InstanceCreateInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32), ("pNext", P), ("flags", C.c_uint32),
        ("pApplicationInfo", P), ("enabledLayerCount", C.c_uint32),
        ("ppEnabledLayerNames", P), ("enabledExtensionCount", C.c_uint32),
        ("ppEnabledExtensionNames", P),
    ]


class DeviceQueueCreateInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32), ("pNext", P), ("flags", C.c_uint32),
        ("queueFamilyIndex", C.c_uint32), ("queueCount", C.c_uint32),
        ("pQueuePriorities", C.POINTER(C.c_float)),
    ]


class DeviceCreateInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32), ("pNext", P), ("flags", C.c_uint32),
        ("queueCreateInfoCount", C.c_uint32), ("pQueueCreateInfos", P),
        ("enabledLayerCount", C.c_uint32), ("ppEnabledLayerNames", P),
        ("enabledExtensionCount", C.c_uint32), ("ppEnabledExtensionNames", P),
        ("pEnabledFeatures", P),
    ]


class BufferCreateInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32), ("pNext", P), ("flags", C.c_uint32),
        ("size", C.c_uint64), ("usage", C.c_uint32), ("sharingMode", C.c_uint32),
        ("queueFamilyIndexCount", C.c_uint32), ("pQueueFamilyIndices", P),
    ]


class PhysicalDeviceProperties(C.Structure):
    _fields_ = [
        ("apiVersion", C.c_uint32), ("driverVersion", C.c_uint32),
        ("vendorID", C.c_uint32), ("deviceID", C.c_uint32), ("deviceType", C.c_uint32),
        ("deviceName", C.c_char * 256), ("pipelineCacheUUID", C.c_uint8 * 16),
        ("limits", C.c_uint8 * 504), ("sparseProperties", C.c_uint8 * 20),
    ]


def load_vk():
    vk = C.CDLL("libvulkan.so.1")
    for name, args in [
        ("vkCreateInstance", [P, P, P]),
        ("vkEnumeratePhysicalDevices", [P, P, P]),
        ("vkGetPhysicalDeviceProperties", [P, P]),
        ("vkCreateDevice", [P, P, P, P]),
        ("vkCreateBuffer", [P, P, P, P]),
        ("vkDestroyBuffer", [P, H, P]),
    ]:
        fn = getattr(vk, name)
        fn.argtypes = args
        fn.restype = C.c_int32
    vk.vkGetPhysicalDeviceProperties.restype = None
    return vk


def open_device(vk, want):
    app = ApplicationInfo(sType=ST_APPLICATION_INFO, pApplicationName=b"vkbadusage",
                          pEngineName=b"vkbadusage", apiVersion=API_1_1)
    ici = InstanceCreateInfo(sType=ST_INSTANCE_CREATE_INFO,
                             pApplicationInfo=C.cast(C.byref(app), P))
    inst = P()
    if vk.vkCreateInstance(C.byref(ici), None, C.byref(inst)) != VK_SUCCESS:
        fail("vkCreateInstance")

    count = C.c_uint32(0)
    vk.vkEnumeratePhysicalDevices(inst, C.byref(count), None)
    if count.value == 0:
        fail("vkEnumeratePhysicalDevices", "no devices")
    devs = (P * count.value)()
    vk.vkEnumeratePhysicalDevices(inst, C.byref(count), devs)

    chosen, name = None, None
    for i in range(count.value):
        props = PhysicalDeviceProperties()
        vk.vkGetPhysicalDeviceProperties(devs[i], C.byref(props))
        dn = props.deviceName.decode(errors="replace")
        if want.lower() in dn.lower():
            chosen, name = devs[i], dn
            break
    if chosen is None:
        print(f"NODEV {want}", flush=True)
        sys.exit(0)
    print(f"DEVICE {name}", flush=True)

    prio = (C.c_float * 1)(1.0)
    q = DeviceQueueCreateInfo(sType=ST_DEVICE_QUEUE_CREATE_INFO, queueFamilyIndex=0,
                              queueCount=1, pQueuePriorities=prio)
    dci = DeviceCreateInfo(sType=ST_DEVICE_CREATE_INFO, queueCreateInfoCount=1,
                           pQueueCreateInfos=C.cast(C.byref(q), P))
    dev = P()
    if vk.vkCreateDevice(chosen, C.byref(dci), None, C.byref(dev)) != VK_SUCCESS:
        fail("vkCreateDevice")
    return dev


def arm_zero_buffer(vk, dev):
    """vkCreateBuffer with size = 0 — the 2026-08-07 dogfood abort.

    A conforming driver rejects this. An asserting one dies, and takes the VM with it.
    Whatever comes back, we only care that we get to print the next line.
    """
    ci = BufferCreateInfo(sType=ST_BUFFER_CREATE_INFO, size=0,
                          usage=BUFFER_USAGE_TRANSFER_SRC, sharingMode=0)
    buf = H(0)
    rc = vk.vkCreateBuffer(dev, C.byref(ci), None, C.byref(buf))
    print(f"BADUSAGE RESULT zero-buffer {rc}", flush=True)
    if rc == VK_SUCCESS and buf.value:
        # It accepted it. Not our problem to judge, but don't leak the handle.
        vk.vkDestroyBuffer(dev, buf, None)


ARMS = {"zero-buffer": arm_zero_buffer}


def main():
    arm = sys.argv[1] if len(sys.argv) > 1 else "zero-buffer"
    want = sys.argv[2] if len(sys.argv) > 2 else "Venus"
    if arm not in ARMS:
        fail("usage", f"unknown arm {arm}")

    vk = load_vk()
    dev = open_device(vk, want)
    print(f"BADUSAGE ARM {arm}", flush=True)
    ARMS[arm](vk, dev)
    print(f"BADUSAGE PASS {arm}", flush=True)


if __name__ == "__main__":
    main()
