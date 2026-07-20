#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

"""vkpipeline — live-pipeline survival across suspend/resume (M9.4 vkmark crash).

The 2026-07-20 dogfood crash (spikes/m9-vkmark-resume-crash/RESULTS.md): the venus
re-creation journal prunes a destroyed object's create entry, but a retained CREATE
that merely *references* the id in its wire args can then never replay. The canonical
legal pattern is exactly what every real app does:

    create shader module(s) -> create pipeline -> DESTROY the modules (+ layout)

The module destroys prune their create entries; at restore the pipeline's create
fails its module lookups and is dropped; the first post-restore command referencing
the (guest-live!) pipeline hits a now-sticky ring FATAL, the host ring thread exits
setting VK_RING_STATUS_FATAL_BIT_MESA, and the app's next submit abort()s in
vn_ring_submit_locked — vkmark's exact death.

This probe stages that pattern deterministically:

  1. compute pipeline from a minimal embedded SPIR-V module + fresh layout
  2. destroy the shader module AND the pipeline layout (both legal after create)
  3. heartbeat: every ~3 s record a fresh one-shot command buffer that binds the
     pipeline and dispatches 1x1x1, submit, wait idle — live wire traffic that
     references the pipeline id

Post-restore the harness asserts the process is the SAME pid and the beats still
advance. Pre-fix the first post-restore beat aborts the process.

Output lines the harness asserts on:
    INSTANCE OK | INSTANCE ERR <r>
    DEVICE <name> / NODEV <substring>
    PIPE FAIL <stage> <r>
    PIPE READY
    PIPE BEAT <n>

Reuses vkfdcycle.py (staged next to it in /tmp) for the ctypes plumbing.
Usage: vkpipeline.py [device-substring]   (default: Venus)
"""
import ctypes as C
import sys
import time

sys.path.insert(0, "/tmp")
from vkfdcycle import VK_SUCCESS, H, P, load, make_device, make_instance, pick_device, vp

# Minimal valid Vulkan compute shader, hand-assembled SPIR-V 1.0:
#   OpCapability Shader; OpMemoryModel Logical GLSL450;
#   OpEntryPoint GLCompute %main "main"; OpExecutionMode %main LocalSize 1 1 1;
#   void main() {}
SPIRV = (
    0x07230203, 0x00010000, 0x00000000, 0x00000006, 0x00000000,
    0x00020011, 0x00000001,  # OpCapability Shader
    0x0003000E, 0x00000000, 0x00000001,  # OpMemoryModel Logical GLSL450
    0x0005000F, 0x00000005, 0x00000004, 0x6E69616D, 0x00000000,  # OpEntryPoint "main"
    0x00060010, 0x00000004, 0x00000011, 0x00000001, 0x00000001, 0x00000001,  # LocalSize
    0x00020013, 0x00000002,  # %2 = OpTypeVoid
    0x00030021, 0x00000003, 0x00000002,  # %3 = OpTypeFunction %2
    0x00050036, 0x00000002, 0x00000004, 0x00000000, 0x00000003,  # %4 = OpFunction
    0x000200F8, 0x00000005,  # %5 = OpLabel
    0x000100FD,  # OpReturn
    0x00010038,  # OpFunctionEnd
)


class ShaderModuleCreateInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),  # 16
        ("pNext", P),
        ("flags", C.c_uint32),
        ("codeSize", C.c_size_t),
        ("pCode", P),
    ]


class PipelineLayoutCreateInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),  # 30
        ("pNext", P),
        ("flags", C.c_uint32),
        ("setLayoutCount", C.c_uint32),
        ("pSetLayouts", P),
        ("pushConstantRangeCount", C.c_uint32),
        ("pPushConstantRanges", P),
    ]


class PipelineShaderStageCreateInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),  # 18
        ("pNext", P),
        ("flags", C.c_uint32),
        ("stage", C.c_uint32),  # 0x20 = COMPUTE
        ("module", H),
        ("pName", C.c_char_p),
        ("pSpecializationInfo", P),
    ]


class ComputePipelineCreateInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),  # 29
        ("pNext", P),
        ("flags", C.c_uint32),
        ("stage", PipelineShaderStageCreateInfo),
        ("layout", H),
        ("basePipelineHandle", H),
        ("basePipelineIndex", C.c_int32),
    ]


class CommandPoolCreateInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),  # 39
        ("pNext", P),
        ("flags", C.c_uint32),
        ("queueFamilyIndex", C.c_uint32),
    ]


class CommandBufferAllocateInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),  # 40
        ("pNext", P),
        ("commandPool", H),
        ("level", C.c_uint32),
        ("commandBufferCount", C.c_uint32),
    ]


class CommandBufferBeginInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),  # 42
        ("pNext", P),
        ("flags", C.c_uint32),
        ("pInheritanceInfo", P),
    ]


class SubmitInfo(C.Structure):
    _fields_ = [
        ("sType", C.c_uint32),  # 4
        ("pNext", P),
        ("waitSemaphoreCount", C.c_uint32),
        ("pWaitSemaphores", P),
        ("pWaitDstStageMask", P),
        ("commandBufferCount", C.c_uint32),
        ("pCommandBuffers", P),
        ("signalSemaphoreCount", C.c_uint32),
        ("pSignalSemaphores", P),
    ]


def load_extra(vk):
    vk.vkGetDeviceQueue.argtypes = [P, C.c_uint32, C.c_uint32, P]
    vk.vkCreateShaderModule.argtypes = [P, P, P, P]
    vk.vkDestroyShaderModule.argtypes = [P, H, P]
    vk.vkCreatePipelineLayout.argtypes = [P, P, P, P]
    vk.vkDestroyPipelineLayout.argtypes = [P, H, P]
    vk.vkCreateComputePipelines.argtypes = [P, H, C.c_uint32, P, P, P]
    vk.vkCreateCommandPool.argtypes = [P, P, P, P]
    vk.vkDestroyCommandPool.argtypes = [P, H, P]
    vk.vkAllocateCommandBuffers.argtypes = [P, P, P]
    vk.vkBeginCommandBuffer.argtypes = [P, P]
    vk.vkCmdBindPipeline.argtypes = [P, C.c_uint32, H]
    vk.vkCmdDispatch.argtypes = [P, C.c_uint32, C.c_uint32, C.c_uint32]
    vk.vkEndCommandBuffer.argtypes = [P]
    vk.vkQueueSubmit.argtypes = [P, C.c_uint32, P, H]
    vk.vkQueueWaitIdle.argtypes = [P]
    for f in (
        vk.vkCreateShaderModule,
        vk.vkCreatePipelineLayout,
        vk.vkCreateComputePipelines,
        vk.vkCreateCommandPool,
        vk.vkAllocateCommandBuffers,
        vk.vkBeginCommandBuffer,
        vk.vkEndCommandBuffer,
        vk.vkQueueSubmit,
        vk.vkQueueWaitIdle,
    ):
        f.restype = C.c_int


def make_pipeline(vk, dev):
    """Compute pipeline whose module AND layout are destroyed right after create —
    the journal-closure hazard under test. Returns (err, pipeline)."""
    code = (C.c_uint32 * len(SPIRV))(*SPIRV)
    smci = ShaderModuleCreateInfo()
    smci.sType = 16
    smci.codeSize = len(SPIRV) * 4
    smci.pCode = C.cast(code, C.c_void_p)
    module = H()
    r = vk.vkCreateShaderModule(dev, C.byref(smci), None, C.byref(module))
    if r != VK_SUCCESS:
        return f"vkCreateShaderModule {r}", None

    plci = PipelineLayoutCreateInfo()
    plci.sType = 30
    layout = H()
    r = vk.vkCreatePipelineLayout(dev, C.byref(plci), None, C.byref(layout))
    if r != VK_SUCCESS:
        return f"vkCreatePipelineLayout {r}", None

    cpci = ComputePipelineCreateInfo()
    cpci.sType = 29
    cpci.stage.sType = 18
    cpci.stage.stage = 0x20  # COMPUTE
    cpci.stage.module = module.value
    cpci.stage.pName = b"main"
    cpci.layout = layout.value
    cpci.basePipelineIndex = -1
    pipeline = H()
    r = vk.vkCreateComputePipelines(dev, 0, 1, C.byref(cpci), None, C.byref(pipeline))
    if r != VK_SUCCESS:
        return f"vkCreateComputePipelines {r}", None

    # The hazard: both creates the pipeline's wire args reference are now pruned
    # from the journal unless their entries are pinned by the pipeline's create.
    vk.vkDestroyShaderModule(dev, module, None)
    vk.vkDestroyPipelineLayout(dev, layout, None)
    return None, pipeline


def beat(vk, dev, queue, pipeline):
    """One heartbeat: fresh one-shot pool, bind the pipeline, dispatch, wait."""
    pci = CommandPoolCreateInfo()
    pci.sType = 39
    pci.flags = 0x1  # TRANSIENT
    pool = H()
    r = vk.vkCreateCommandPool(dev, C.byref(pci), None, C.byref(pool))
    if r != VK_SUCCESS:
        return f"vkCreateCommandPool {r}"
    cai = CommandBufferAllocateInfo()
    cai.sType = 40
    cai.commandPool = pool.value
    cai.level = 0  # PRIMARY
    cai.commandBufferCount = 1
    cmd = C.c_void_p()
    r = vk.vkAllocateCommandBuffers(dev, C.byref(cai), C.byref(cmd))
    if r != VK_SUCCESS:
        return f"vkAllocateCommandBuffers {r}"
    bi = CommandBufferBeginInfo()
    bi.sType = 42
    bi.flags = 0x1  # ONE_TIME_SUBMIT
    r = vk.vkBeginCommandBuffer(cmd, C.byref(bi))
    if r != VK_SUCCESS:
        return f"vkBeginCommandBuffer {r}"
    vk.vkCmdBindPipeline(cmd, 1, pipeline)  # BIND_POINT_COMPUTE
    vk.vkCmdDispatch(cmd, 1, 1, 1)
    r = vk.vkEndCommandBuffer(cmd)
    if r != VK_SUCCESS:
        return f"vkEndCommandBuffer {r}"
    si = SubmitInfo()
    si.sType = 4
    si.commandBufferCount = 1
    si.pCommandBuffers = C.cast(C.byref(cmd), C.c_void_p)
    r = vk.vkQueueSubmit(queue, 1, C.byref(si), 0)
    if r != VK_SUCCESS:
        return f"vkQueueSubmit {r}"
    r = vk.vkQueueWaitIdle(queue)
    if r != VK_SUCCESS:
        return f"vkQueueWaitIdle {r}"
    vk.vkDestroyCommandPool(dev, pool, None)
    return None


def main():
    want = sys.argv[1] if len(sys.argv) > 1 else "Venus"
    vk = load()
    load_extra(vk)

    r, inst = make_instance(vk)
    if r != VK_SUCCESS:
        print(f"INSTANCE ERR {r}", flush=True)
        return
    print("INSTANCE OK", flush=True)
    phys = pick_device(vk, inst, want)
    if phys is None:
        print(f"NODEV {want}", flush=True)
        return
    dev = make_device(vk, phys)
    if dev is None:
        return
    queue = C.c_void_p()
    vk.vkGetDeviceQueue(dev, 0, 0, C.byref(queue))

    err, pipeline = make_pipeline(vk, dev)
    if err:
        print(f"PIPE FAIL {err}", flush=True)
        return
    # Prove the pipeline works before declaring READY.
    err = beat(vk, dev, queue, pipeline)
    if err:
        print(f"PIPE FAIL first-{err}", flush=True)
        return
    print("PIPE READY", flush=True)

    n = 0
    while True:
        n += 1
        err = beat(vk, dev, queue, pipeline)
        if err:
            print(f"PIPE FAIL beat{n}-{err}", flush=True)
            return
        print(f"PIPE BEAT {n}", flush=True)
        time.sleep(3)


if __name__ == "__main__":
    main()
