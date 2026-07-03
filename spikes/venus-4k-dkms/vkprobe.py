#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

"""vkprobe — dependency-free Vulkan probe for stock-guest fallback tests.

Speaks to libvulkan.so.1 via ctypes (no vulkan-tools / gcc needed on the guest),
so the venus->lavapipe graceful-degradation test can run against a pristine stock
image. Prints structured lines the host harness asserts on:

    INSTANCE OK | INSTANCE ERR <VkResult>
    DEVICE <name>
    MAPPASS <count> <name> | MAPFAIL <VkResult> <name> | MAPSKIP <substring>

Usage: vkprobe.py [device-substring]
  - Creates an instance against whatever ICDs are visible (control with
    VK_DRIVER_FILES), enumerates devices, prints them.
  - If device-substring is given and matches, runs a host-visible allocate+map
    write/read stress on that device (odd sizes cycling every 4 KiB residue mod
    16 KiB — the blob-map alignment killer; see limina-blob-map-16k-alignment).
Always exits 0 unless the probe itself is broken; the OUTPUT is the oracle.
"""
import ctypes as C
import sys

VK_SUCCESS = 0


def u32(x):
    return C.c_uint32(x)


class InstanceCreateInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),
        ("pNext", C.c_void_p),
        ("flags", C.c_uint32),
        ("pApplicationInfo", C.c_void_p),
        ("enabledLayerCount", C.c_uint32),
        ("ppEnabledLayerNames", C.c_void_p),
        ("enabledExtensionCount", C.c_uint32),
        ("ppEnabledExtensionNames", C.c_void_p),
    ]


class DeviceQueueCreateInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),
        ("pNext", C.c_void_p),
        ("flags", C.c_uint32),
        ("queueFamilyIndex", C.c_uint32),
        ("queueCount", C.c_uint32),
        ("pQueuePriorities", C.POINTER(C.c_float)),
    ]


class DeviceCreateInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),
        ("pNext", C.c_void_p),
        ("flags", C.c_uint32),
        ("queueCreateInfoCount", C.c_uint32),
        ("pQueueCreateInfos", C.c_void_p),
        ("enabledLayerCount", C.c_uint32),
        ("ppEnabledLayerNames", C.c_void_p),
        ("enabledExtensionCount", C.c_uint32),
        ("ppEnabledExtensionNames", C.c_void_p),
        ("pEnabledFeatures", C.c_void_p),
    ]


class MemoryType(C.Structure):
    _fields_ = [("propertyFlags", C.c_uint32), ("heapIndex", C.c_uint32)]


class MemoryHeap(C.Structure):
    _fields_ = [("size", C.c_uint64), ("flags", C.c_uint32)]


class MemoryProperties(C.Structure):
    _fields_ = [
        ("memoryTypeCount", C.c_uint32),
        ("memoryTypes", MemoryType * 32),
        ("memoryHeapCount", C.c_uint32),
        ("memoryHeaps", MemoryHeap * 16),
    ]


class MemoryAllocateInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),
        ("pNext", C.c_void_p),
        ("allocationSize", C.c_uint64),
        ("memoryTypeIndex", C.c_uint32),
    ]


def main():
    want = sys.argv[1] if len(sys.argv) > 1 else None
    vk = C.CDLL("libvulkan.so.1")

    # Explicit signatures: without argtypes, ctypes passes python ints (e.g. a
    # VkPhysicalDevice pulled out of an array) as 32-bit C ints — truncating the
    # 64-bit handle and crashing inside the loader.
    P = C.c_void_p
    vk.vkCreateInstance.argtypes = [P, P, P]
    vk.vkEnumeratePhysicalDevices.argtypes = [P, P, P]
    vk.vkGetPhysicalDeviceProperties.argtypes = [P, P]
    vk.vkGetPhysicalDeviceMemoryProperties.argtypes = [P, P]
    vk.vkCreateDevice.argtypes = [P, P, P, P]
    vk.vkAllocateMemory.argtypes = [P, P, P, P]
    vk.vkMapMemory.argtypes = [P, P, C.c_uint64, C.c_uint64, C.c_uint32, P]
    vk.vkUnmapMemory.argtypes = [P, P]
    for f in (
        vk.vkCreateInstance,
        vk.vkEnumeratePhysicalDevices,
        vk.vkCreateDevice,
        vk.vkAllocateMemory,
        vk.vkMapMemory,
    ):
        f.restype = C.c_int

    ici = InstanceCreateInfo()
    ici.sType = 1  # VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO
    inst = C.c_void_p()
    r = vk.vkCreateInstance(C.byref(ici), None, C.byref(inst))
    if r != VK_SUCCESS:
        print(f"INSTANCE ERR {r}", flush=True)
        return
    print("INSTANCE OK", flush=True)

    n = C.c_uint32(0)
    vk.vkEnumeratePhysicalDevices(inst, C.byref(n), None)
    devs = (C.c_void_p * max(n.value, 1))()
    vk.vkEnumeratePhysicalDevices(inst, C.byref(n), devs)

    chosen = None
    chosen_name = None
    for i in range(n.value):
        # VkPhysicalDeviceProperties: deviceName (char[256]) sits at offset 20,
        # after apiVersion/driverVersion/vendorID/deviceID/deviceType. Oversized
        # buffer covers limits/sparse.
        props = C.create_string_buffer(4096)
        vk.vkGetPhysicalDeviceProperties(devs[i], props)
        name = props.raw[20 : 20 + 256].split(b"\0")[0].decode()
        print(f"DEVICE {name}", flush=True)
        if want and chosen is None and want in name:
            chosen = devs[i]
            chosen_name = name

    if want is None:
        return
    if chosen is None:
        print(f"MAPSKIP {want}", flush=True)
        return

    prio = C.c_float(1.0)
    qci = DeviceQueueCreateInfo()
    qci.sType = 2  # DEVICE_QUEUE_CREATE_INFO
    qci.queueFamilyIndex = 0
    qci.queueCount = 1
    qci.pQueuePriorities = C.pointer(prio)
    dci = DeviceCreateInfo()
    dci.sType = 3  # DEVICE_CREATE_INFO
    dci.queueCreateInfoCount = 1
    dci.pQueueCreateInfos = C.cast(C.byref(qci), C.c_void_p)
    dev = C.c_void_p()
    r = vk.vkCreateDevice(chosen, C.byref(dci), None, C.byref(dev))
    if r != VK_SUCCESS:
        print(f"MAPFAIL {r} {chosen_name} (vkCreateDevice)", flush=True)
        return

    mp = MemoryProperties()
    vk.vkGetPhysicalDeviceMemoryProperties(chosen, C.byref(mp))
    HOST_VISIBLE, HOST_COHERENT = 0x2, 0x4
    mtype = next(
        (
            i
            for i in range(mp.memoryTypeCount)
            if mp.memoryTypes[i].propertyFlags & HOST_VISIBLE
            and mp.memoryTypes[i].propertyFlags & HOST_COHERENT
        ),
        None,
    )
    if mtype is None:
        print(f"MAPFAIL -1 {chosen_name} (no host-visible type)", flush=True)
        return

    vk.vkMapMemory.restype = C.c_int
    total = 0
    live = []
    # Odd sizes cycling every 4 KiB residue mod 16 KiB; keep a live set so the
    # window packs, free half each round to force hole reuse.
    for rnd in range(3):
        live = live[len(live) // 2 :]
        while len(live) < 16:
            size = 0x1000 * (1 + (total % 33))
            ai = MemoryAllocateInfo()
            ai.sType = 5  # MEMORY_ALLOCATE_INFO
            ai.allocationSize = size
            ai.memoryTypeIndex = mtype
            mem = C.c_void_p()
            r = vk.vkAllocateMemory(dev, C.byref(ai), None, C.byref(mem))
            if r != VK_SUCCESS:
                print(f"MAPFAIL {r} {chosen_name} (alloc #{total})", flush=True)
                return
            ptr = C.c_void_p()
            r = vk.vkMapMemory(dev, mem, C.c_uint64(0), C.c_uint64(size), 0, C.byref(ptr))
            if r != VK_SUCCESS:
                print(f"MAPFAIL {r} {chosen_name} (map #{total})", flush=True)
                return
            buf = (C.c_ubyte * size).from_address(ptr.value)
            buf[0], buf[size - 1] = 0xA5, 0x5A
            if buf[0] != 0xA5 or buf[size - 1] != 0x5A:
                print(f"MAPFAIL -2 {chosen_name} (readback #{total})", flush=True)
                return
            vk.vkUnmapMemory(dev, mem)
            live.append(mem)
            total += 1
    print(f"MAPPASS {total} {chosen_name}", flush=True)


if __name__ == "__main__":
    main()
