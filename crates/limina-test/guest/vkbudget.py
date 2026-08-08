#!/usr/bin/env python3
"""Drive the host GPU-memory budget from inside the guest.

Three roles, chosen by the mode argument:

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

  budget — ask VK_EXT_memory_budget what the host says is left, allocate, and ask
          again. This is the ONE feedback channel that survives the transport (see
          below): the budget query is not an allocation, and
          vn_GetPhysicalDeviceMemoryProperties2 issues a real synchronous vn_call_
          round-trip whenever the budget struct is chained.

Deliberately NOT checked here: the specific VkResult of an allocation. venus submits
allocations asynchronously (vn_device_memory_alloc_simple) and returns VK_SUCCESS
without waiting, so the host's result code never reaches this process. Asserting on
it would be asserting on a value the transport discards. The budget query is exempt
precisely because it is not an allocation — that asymmetry is the whole point of the
`budget` mode.

REQUIRES `VN_DEBUG=mem_budget` IN THIS PROCESS'S ENVIRONMENT. venus advertises the
extension only under that flag (vn_physical_device.c: `.EXT_memory_budget =
VN_DEBUG(MEM_BUDGET)`), and it is read once per process at instance creation, so it
must be set before python starts — exporting it later does nothing. The caller sets
it explicitly rather than relying on the /etc/environment.d drop-in, which a
non-login ssh shell does not source.

Usage: vkbudget.py <device-name-substring> hog|probe|budget <chunk-mib> [max-chunks]

Output (the oracle for the guest half — always exits 0 unless the probe breaks):

    DEVICE <name> | NODEV <substring> | INSTANCE ERR <r>
    ALLOCATED <n>                   chunks the guest believes it got
    ALLOC ERR <n> <VkResult>        an allocation returned non-success
    PROBE OK | PROBE FAIL <VkResult>
    BUDGET NOEXT                    venus did not advertise VK_EXT_memory_budget
    BUDGET HEAP <i> <when> flags=<h> size=<b> budget=<b> usage=<b>
    BUDGET TARGET <heap-index>      the heap memory type 0 allocates from
    BUDGET DONE <mode> | BUDGET FAIL <stage>
"""
import ctypes as C
import sys

VK_SUCCESS = 0
API_1_1 = (1 << 22) | (1 << 12)

# vulkan_core.h — verified against the header in the guest mesa checkout, not recalled.
ST_PHYSICAL_DEVICE_MEMORY_PROPERTIES_2 = 1000059006
ST_PHYSICAL_DEVICE_MEMORY_BUDGET_PROPERTIES_EXT = 1000237000
MAX_MEMORY_TYPES = 32
MAX_MEMORY_HEAPS = 16
MEMORY_HEAP_DEVICE_LOCAL_BIT = 0x1
EXT_MEMORY_BUDGET = b"VK_EXT_memory_budget"

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


# Let ctypes compute the offsets rather than indexing into a raw buffer: these are nested
# fixed-size arrays whose padding is easy to get subtly wrong, and a wrong offset here
# reads a plausible number from the wrong field.
class MemoryType(C.Structure):
    _fields_ = [("propertyFlags", C.c_uint32), ("heapIndex", C.c_uint32)]


class MemoryHeap(C.Structure):
    _fields_ = [("size", C.c_uint64), ("flags", C.c_uint32)]


class PhysicalDeviceMemoryProperties(C.Structure):
    _fields_ = [
        ("memoryTypeCount", C.c_uint32),
        ("memoryTypes", MemoryType * MAX_MEMORY_TYPES),
        ("memoryHeapCount", C.c_uint32),
        ("memoryHeaps", MemoryHeap * MAX_MEMORY_HEAPS),
    ]


class PhysicalDeviceMemoryProperties2(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),
        ("pNext", P),
        ("memoryProperties", PhysicalDeviceMemoryProperties),
    ]


class MemoryBudgetPropertiesEXT(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),
        ("pNext", P),
        ("heapBudget", C.c_uint64 * MAX_MEMORY_HEAPS),
        ("heapUsage", C.c_uint64 * MAX_MEMORY_HEAPS),
    ]


class ExtensionProperties(C.Structure):
    _fields_ = [("extensionName", C.c_char * 256), ("specVersion", C.c_uint32)]


def load_vk():
    vk = C.CDLL("libvulkan.so.1")
    vk.vkCreateInstance.argtypes = [P, P, P]
    vk.vkEnumeratePhysicalDevices.argtypes = [P, P, P]
    vk.vkGetPhysicalDeviceProperties.argtypes = [P, P]
    vk.vkCreateDevice.argtypes = [P, P, P, P]
    vk.vkDestroyDevice.argtypes = [P, P]
    vk.vkAllocateMemory.argtypes = [P, P, P, P]
    vk.vkFreeMemory.argtypes = [P, H, P]
    vk.vkGetPhysicalDeviceMemoryProperties2.argtypes = [P, P]
    vk.vkEnumerateDeviceExtensionProperties.argtypes = [P, C.c_char_p, P, P]
    for f in (vk.vkCreateInstance, vk.vkEnumeratePhysicalDevices, vk.vkCreateDevice,
              vk.vkAllocateMemory, vk.vkEnumerateDeviceExtensionProperties):
        f.restype = C.c_int
    return vk


def has_memory_budget(vk, phys):
    n = C.c_uint32(0)
    vk.vkEnumerateDeviceExtensionProperties(phys, None, C.byref(n), None)
    exts = (ExtensionProperties * max(n.value, 1))()
    vk.vkEnumerateDeviceExtensionProperties(phys, None, C.byref(n), exts)
    return any(exts[i].extensionName == EXT_MEMORY_BUDGET for i in range(n.value))


def query_budget(vk, phys):
    """Chain the budget struct so venus actually round-trips to the host.

    Without the chained struct the guest answers from its own cached copy and the host
    never sees the query at all (vn_physical_device.c: `if (memory_budget) vn_call_...`),
    so a version of this that queried plain memory properties would read a snapshot and
    call it a live budget.
    """
    budget = MemoryBudgetPropertiesEXT()
    budget.sType = ST_PHYSICAL_DEVICE_MEMORY_BUDGET_PROPERTIES_EXT
    props = PhysicalDeviceMemoryProperties2()
    props.sType = ST_PHYSICAL_DEVICE_MEMORY_PROPERTIES_2
    props.pNext = vp(budget)
    vk.vkGetPhysicalDeviceMemoryProperties2(phys, C.byref(props))
    return props.memoryProperties, budget


def report_heaps(mp, budget, when):
    for i in range(mp.memoryHeapCount):
        print(
            f"BUDGET HEAP {i} {when} flags=0x{mp.memoryHeaps[i].flags:x} "
            f"size={mp.memoryHeaps[i].size} budget={budget.heapBudget[i]} "
            f"usage={budget.heapUsage[i]}",
            flush=True,
        )


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
    if mode == "budget":
        # Before the device: the query is on the PHYSICAL device, and the extension being
        # advertised is what makes venus round-trip at all.
        if not has_memory_budget(vk, phys):
            print("BUDGET NOEXT", flush=True)
            print("BUDGET FAIL noext", flush=True)
            return
        mp, budget = query_budget(vk, phys)
        # Which heap our allocations actually land in — the hog uses memory type 0, so
        # that type's heap is the one whose numbers must move. Reporting it rather than
        # assuming heap 0 keeps the test honest if the type/heap mapping ever changes.
        target = mp.memoryTypes[0].heapIndex if mp.memoryTypeCount else 0
        print(f"BUDGET TARGET {target}", flush=True)
        report_heaps(mp, budget, "before")

        dev = make_device(vk, phys)
        if not dev:
            return
        got = 0
        for _ in range(max_chunks):
            r, _mem = alloc(vk, dev, chunk)
            if r != VK_SUCCESS:
                break
            got += 1
        print(f"ALLOCATED {got}", flush=True)

        mp, budget = query_budget(vk, phys)
        report_heaps(mp, budget, "after")
        print("BUDGET DONE budget", flush=True)
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
