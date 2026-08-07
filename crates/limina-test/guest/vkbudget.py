#!/usr/bin/env python3
"""Drive the host GPU-memory budget from inside the guest.

Two roles, chosen by the mode argument:

  hog   — allocate VkDeviceMemory until the host cap stops us. This process is
          EXPECTED TO DIE: past the cap the host kills the venus context, and mesa
          aborts the guest process when the ring goes fatal. A clean exit, an error
          return, and an abort are all consistent with the cap having fired — which
          one you get depends on where the ring death lands, so none of them is the
          oracle. The host's worker log is (see vkr_budget.h).

  probe — allocate ONE chunk and free it. Run after `hog` as the recovery check: a
          fresh context must still work, which is only true if the host credited the
          dead context's charges back. A ledger that only counts up would pass a
          refusal test and still break every guest that merely churns memory.

Deliberately NOT checked here: the specific VkResult. venus submits allocations
asynchronously (vn_device_memory_alloc_simple) and returns VK_SUCCESS without
waiting, so the host's result code never reaches this process. Asserting on it
would be asserting on a value the transport discards.

Usage: vkbudget.py <device-name-substring> hog|probe <chunk-mib> [max-chunks]

Output (the oracle for the guest half — always exits 0 unless the probe breaks):

    DEVICE <name> | NODEV <substring> | INSTANCE ERR <r>
    ALLOCATED <n>                   chunks the guest believes it got
    ALLOC ERR <n> <VkResult>        an allocation returned non-success
    PROBE OK | PROBE FAIL <VkResult>
    BUDGET DONE <mode> | BUDGET FAIL <stage>
"""
import ctypes as C
import sys

VK_SUCCESS = 0
API_1_1 = (1 << 22) | (1 << 12)

P = C.c_void_p
H = C.c_uint64


def vp(x):
    return C.cast(C.byref(x), C.c_void_p)


class ApplicationInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),
        ("pNext", P),
        ("pApplicationName", C.c_char_p),
        ("applicationVersion", C.c_uint32),
        ("pEngineName", C.c_char_p),
        ("engineVersion", C.c_uint32),
        ("apiVersion", C.c_uint32),
    ]


class InstanceCreateInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),
        ("pNext", P),
        ("flags", C.c_uint32),
        ("pApplicationInfo", P),
        ("enabledLayerCount", C.c_uint32),
        ("ppEnabledLayerNames", P),
        ("enabledExtensionCount", C.c_uint32),
        ("ppEnabledExtensionNames", P),
    ]


class DeviceQueueCreateInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),
        ("pNext", P),
        ("flags", C.c_uint32),
        ("queueFamilyIndex", C.c_uint32),
        ("queueCount", C.c_uint32),
        ("pQueuePriorities", C.POINTER(C.c_float)),
    ]


class DeviceCreateInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),
        ("pNext", P),
        ("flags", C.c_uint32),
        ("queueCreateInfoCount", C.c_uint32),
        ("pQueueCreateInfos", P),
        ("enabledLayerCount", C.c_uint32),
        ("ppEnabledLayerNames", P),
        ("enabledExtensionCount", C.c_uint32),
        ("ppEnabledExtensionNames", P),
        ("pEnabledFeatures", P),
    ]


class MemoryAllocateInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),
        ("pNext", P),
        ("allocationSize", C.c_uint64),
        ("memoryTypeIndex", C.c_uint32),
    ]


def load_vk():
    vk = C.CDLL("libvulkan.so.1")
    vk.vkCreateInstance.argtypes = [P, P, P]
    vk.vkEnumeratePhysicalDevices.argtypes = [P, P, P]
    vk.vkGetPhysicalDeviceProperties.argtypes = [P, P]
    vk.vkCreateDevice.argtypes = [P, P, P, P]
    vk.vkDestroyDevice.argtypes = [P, P]
    vk.vkAllocateMemory.argtypes = [P, P, P, P]
    vk.vkFreeMemory.argtypes = [P, H, P]
    for f in (vk.vkCreateInstance, vk.vkEnumeratePhysicalDevices, vk.vkCreateDevice,
              vk.vkAllocateMemory):
        f.restype = C.c_int
    return vk


def make_instance(vk):
    app = ApplicationInfo()
    app.sType = 0
    app.apiVersion = API_1_1
    ici = InstanceCreateInfo()
    ici.sType = 1
    ici.pApplicationInfo = vp(app)
    inst = C.c_void_p()
    return vk.vkCreateInstance(C.byref(ici), None, C.byref(inst)), inst


def pick_device(vk, inst, want):
    n = C.c_uint32(0)
    vk.vkEnumeratePhysicalDevices(inst, C.byref(n), None)
    devs = (C.c_void_p * max(n.value, 1))()
    vk.vkEnumeratePhysicalDevices(inst, C.byref(n), devs)
    for i in range(n.value):
        props = C.create_string_buffer(4096)
        vk.vkGetPhysicalDeviceProperties(devs[i], props)
        name = props.raw[20 : 20 + 256].split(b"\0")[0].decode()
        print(f"DEVICE {name}", flush=True)
        if want in name:
            return devs[i]
    return None


def make_device(vk, phys):
    prio = C.c_float(1.0)
    qci = DeviceQueueCreateInfo()
    qci.sType = 2
    qci.queueFamilyIndex = 0
    qci.queueCount = 1
    qci.pQueuePriorities = C.pointer(prio)
    dci = DeviceCreateInfo()
    dci.sType = 3
    dci.queueCreateInfoCount = 1
    dci.pQueueCreateInfos = vp(qci)
    dev = C.c_void_p()
    r = vk.vkCreateDevice(phys, C.byref(dci), None, C.byref(dev))
    if r != VK_SUCCESS:
        print(f"BUDGET FAIL vkCreateDevice-{r}", flush=True)
        return None
    return dev


def alloc(vk, dev, size):
    ai = MemoryAllocateInfo()
    ai.sType = 5
    ai.allocationSize = size
    ai.memoryTypeIndex = 0
    mem = C.c_uint64()
    return vk.vkAllocateMemory(dev, C.byref(ai), None, C.byref(mem)), mem.value


def main():
    want = sys.argv[1] if len(sys.argv) > 1 else "Venus"
    mode = sys.argv[2] if len(sys.argv) > 2 else "hog"
    chunk = int(sys.argv[3] if len(sys.argv) > 3 else 64) * 1024 * 1024
    max_chunks = int(sys.argv[4] if len(sys.argv) > 4 else 256)

    vk = load_vk()
    r, inst = make_instance(vk)
    if r != VK_SUCCESS:
        print(f"INSTANCE ERR {r}", flush=True)
        print("BUDGET FAIL instance", flush=True)
        return
    phys = pick_device(vk, inst, want)
    if not phys:
        print(f"NODEV {want}", flush=True)
        print("BUDGET FAIL nodev", flush=True)
        return
    dev = make_device(vk, phys)
    if not dev:
        return

    if mode == "probe":
        r, mem = alloc(vk, dev, chunk)
        if r != VK_SUCCESS:
            print(f"PROBE FAIL {r}", flush=True)
            print("BUDGET FAIL probe", flush=True)
            return
        vk.vkFreeMemory(dev, mem, None)
        print("PROBE OK", flush=True)
        print("BUDGET DONE probe", flush=True)
        return

    # hog: allocate until the host stops us. Nothing is freed on the way — the point is
    # to hold the memory. Whether this loop finishes, errors, or the process dies
    # mid-flight is all the same to the test; the host log is the oracle.
    got = 0
    for i in range(max_chunks):
        r, _mem = alloc(vk, dev, chunk)
        if r != VK_SUCCESS:
            print(f"ALLOC ERR {i} {r}", flush=True)
            break
        got += 1
    print(f"ALLOCATED {got}", flush=True)
    print("BUDGET DONE hog", flush=True)


if __name__ == "__main__":
    main()
