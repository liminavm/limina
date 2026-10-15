# Workgroup counts Metal runs past KosmicKrisp's advertised limit

KosmicKrisp advertised `maxComputeWorkGroupCount = {0x7fffffff, 65535, 65535}`, the AGX values
Asahi's driver reports. ggml's Vulkan backend dispatches RMS_NORM with one workgroup per row along y
and asserts against that limit, so test-backend-ops' `RMS_NORM(ne=[4,1,65536,1])` aborted the
process. `probe.c` dispatches a grid of `count.comp`, which counts the workgroups that ran and keeps
the largest id in each dimension; `probe X Y Z` checks that every workgroup ran once.

    cc -Wall -I/opt/homebrew/include probe.c -L/opt/homebrew/lib -lvulkan -o wgp
    VK_ICD_FILENAMES=<kk build>/.../kosmickrisp_mesa_devenv_icd.aarch64.json ./wgp 1 70000 1

Measured on an M1 Max, macOS 27, through KK's own dispatch path: grids of 65536, 200000, 1048576
and 16777217 workgroups in y, 200000 and 16777217 in z, {4, 70000, 3} and {3, 3, 16777217} all ran
every workgroup exactly once. Metal is not bound by 65535.

limina-kk `kk: advertise the workgroup counts Metal takes in y and z` raises y and z to 0x3fffff,
where a 1024-invocation workgroup still counts its threads in 32 bits (a vkCmdDispatch reaches
Metal, and the limina wire, as a grid of threads). It also stops the empty-dispatch check multiplying
the counts in 32 bits: before it `65536 65536 1` ran none of its 2^32 workgroups (largest id
{0, 0, 0}), after it all of them. The counter is 32 bits, so at 2^32 and up the largest ids are the
evidence.

x stays 0x7fffffff, so a dispatch of more than 2^32 threads in x still overflows that grid; a
threadgroup-count dispatch through the bridges would remove the limit altogether.
