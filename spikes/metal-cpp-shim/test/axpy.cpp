// A compute program written against Apple's metal-cpp, unmodified, the way MLX uses it: a
// .metallib loaded by URL, a kernel specialised with function constants, buffers and setBytes,
// a concurrent encoder with a memory barrier, a std::function completion handler, an MSL source
// library, and autorelease pools. With "unknown" it calls a method limina-metal lacks, which must
// abort naming it. With "thread-exit" a thread's C++ thread_local destructor uses an autorelease
// pool after the shim's own per-thread state may already be gone, as MLX's per-thread state does.
// With "many-buffers" it holds more buffers than a context has arenas (4096), as a loaded model
// does, frees and remakes them, and runs axpy on buffers spread across them; then it makes and
// frees more large buffers, each an arena of its own, than there are arena ids, and runs axpy in
// the last of them.
//
// Usage: axpy <kernels.metallib> <kernels.metal> [unknown | thread-exit | many-buffers]

#define NS_PRIVATE_IMPLEMENTATION
#define MTL_PRIVATE_IMPLEMENTATION
#include <Foundation/Foundation.hpp>
#include <Metal/Metal.hpp>

#include <atomic>
#include <cmath>
#include <cstdio>
#include <cstring>
#include <fstream>
#include <sstream>
#include <thread>
#include <vector>

static const uint32_t N = 1 << 20;
static const float A = 2.5f;

static float x(uint32_t i) { return (i % 1000) * 0.25f; }
static float y0(uint32_t i) { return 1.0f - (i % 77); }

// Made on a thread's first use; destroyed at its exit, in reverse order of construction among the
// thread's destructors.
struct UsesPoolAtThreadExit {
    ~UsesPoolAtThreadExit() {
        NS::AutoreleasePool* p = NS::AutoreleasePool::alloc()->init();
        NS::String::string("at thread exit", NS::UTF8StringEncoding);
        p->release();
    }
};
static thread_local UsesPoolAtThreadExit uses_pool_at_thread_exit;

static int fail(const char* what, NS::Error* err) {
    std::printf("FAIL: %s: %s\n", what, err ? err->localizedDescription()->utf8String() : "(no error)");
    return 1;
}

int main(int argc, char** argv) {
    if (argc < 3) {
        std::fprintf(stderr, "usage: %s <kernels.metallib> <kernels.metal> [unknown]\n", argv[0]);
        return 2;
    }
    if (argc > 3 && std::strcmp(argv[3], "thread-exit") == 0) {
        std::thread([] {
            // Construct the C++ thread_local first, so its destructor runs after anything the
            // shim registers for this thread later.
            (void)&uses_pool_at_thread_exit;
            NS::AutoreleasePool* p = NS::AutoreleasePool::alloc()->init();
            NS::String::string("on the thread", NS::UTF8StringEncoding);
            p->release();
        }).join();
        std::printf("a pool in a thread_local destructor at thread exit: OK\n");
        return 0;
    }
    NS::AutoreleasePool* pool = NS::AutoreleasePool::alloc()->init();
    MTL::Device* dev = MTL::CreateSystemDefaultDevice();
    MTL::Size max = dev->maxThreadsPerThreadgroup();
    std::printf("device: %s, max threads per threadgroup %lux%lux%lu\n", dev->name()->utf8String(),
                (unsigned long)max.width, (unsigned long)max.height, (unsigned long)max.depth);
    if (argc > 3 && std::strcmp(argv[3], "unknown") == 0) {
        std::printf("calling a method limina-metal does not implement\n");
        std::fflush(stdout);
        dev->isHeadless();
        return 0;
    }

    if (argc > 3 && std::strcmp(argv[3], "many-buffers") == 0) {
        const int count = 5000, n = 1024;
        MTL::Buffer* tiny = dev->newBuffer(1, MTL::ResourceStorageModeShared);
        std::printf("a 1-byte buffer takes %lu bytes\n", (unsigned long)tiny->allocatedSize());
        tiny->release();
        std::vector<MTL::Buffer*> bufs;
        // Twice round: what the first round frees must come back for the second.
        for (int round = 0; round < 2; round++) {
            for (MTL::Buffer* b : bufs)
                b->release();
            bufs.clear();
            for (int i = 0; i < count; i++) {
                MTL::Buffer* b = dev->newBuffer(n * 4, MTL::ResourceStorageModeShared);
                if (!b) {
                    std::printf("FAIL: buffer %d of %d (round %d) was not made\n", i, count, round);
                    return 1;
                }
                float* p = static_cast<float*>(b->contents());
                for (int j = 0; j < n; j++)
                    p[j] = float(i + j);
                bufs.push_back(b);
            }
        }
        NS::Error* err = nullptr;
        NS::URL* url = NS::URL::fileURLWithPath(NS::String::string(argv[1], NS::UTF8StringEncoding));
        MTL::Library* lib = dev->newLibrary(url, &err);
        MTL::Function* axpy = lib ? lib->newFunction(NS::String::string("axpy", NS::ASCIIStringEncoding)) : nullptr;
        MTL::ComputePipelineState* pso = axpy ? dev->newComputePipelineState(axpy, &err) : nullptr;
        if (!pso)
            return fail("axpy pipeline", err);
        MTL::CommandQueue* queue = dev->newCommandQueue();
        MTL::CommandBuffer* cb = queue->commandBufferWithUnretainedReferences();
        MTL::ComputeCommandEncoder* enc = cb->computeCommandEncoder(MTL::DispatchTypeConcurrent);
        enc->setComputePipelineState(pso);
        // y = A*x + y for (x, y) = (bufs[k], bufs[count-1-k]), k spread across all of them.
        const int pairs[] = {0, 1, 1000, 2047, 2048, 2499};
        for (int k : pairs) {
            enc->setBuffer(bufs[k], 0, 0);
            enc->setBuffer(bufs[count - 1 - k], 0, 1);
            enc->setBytes(&A, sizeof(A), 2);
            enc->dispatchThreads(MTL::Size(n, 1, 1), MTL::Size(256, 1, 1));
        }
        enc->endEncoding();
        cb->commit();
        cb->waitUntilCompleted();
        int wrong = 0;
        for (int k : pairs) {
            const float* y = static_cast<const float*>(bufs[count - 1 - k]->contents());
            for (int j = 0; j < n; j++)
                wrong += y[j] != std::fma(A, float(k + j), float(count - 1 - k + j));
        }
        std::printf("%d buffers, twice; axpy across them: %d of %d wrong\n", count, wrong,
                    int(sizeof(pairs) / sizeof(pairs[0])) * n);
        for (MTL::Buffer* b : bufs)
            b->release();

        const int big = 4500;
        const uint32_t bn = (20u << 20) / 4;
        MTL::Buffer* last = nullptr;
        for (int i = 0; i < big; i++) {
            if (last)
                last->release();
            last = dev->newBuffer(bn * 4, MTL::ResourceStorageModeShared);
            if (!last) {
                std::printf("FAIL: large buffer %d of %d was not made\n", i, big);
                return 1;
            }
        }
        float* lp = static_cast<float*>(last->contents());
        for (uint32_t j = 0; j < bn; j++)
            lp[j] = float(j % 4096);
        MTL::Buffer* lx = dev->newBuffer(bn * 4, MTL::ResourceStorageModeShared);
        float* xp = static_cast<float*>(lx->contents());
        for (uint32_t j = 0; j < bn; j++)
            xp[j] = 1.0f;
        cb = queue->commandBufferWithUnretainedReferences();
        enc = cb->computeCommandEncoder(MTL::DispatchTypeConcurrent);
        enc->setComputePipelineState(pso);
        enc->setBuffer(lx, 0, 0);
        enc->setBuffer(last, 0, 1);
        enc->setBytes(&A, sizeof(A), 2);
        enc->dispatchThreads(MTL::Size(bn, 1, 1), MTL::Size(256, 1, 1));
        enc->endEncoding();
        cb->commit();
        cb->waitUntilCompleted();
        int bwrong = 0;
        for (uint32_t j = 0; j < bn; j++)
            bwrong += lp[j] != A + float(j % 4096);
        std::printf("%d large buffers made and freed; axpy in the last: %d of %u wrong\n", big,
                    bwrong, bn);
        wrong += bwrong;
        last->release();
        lx->release();
        pso->release();
        axpy->release();
        lib->release();
        queue->release();
        return wrong ? 1 : 0;
    }

    NS::Error* err = nullptr;
    NS::URL* url = NS::URL::fileURLWithPath(NS::String::string(argv[1], NS::UTF8StringEncoding));
    MTL::Library* lib = dev->newLibrary(url, &err);
    if (!lib)
        return fail("newLibrary(url)", err);
    MTL::Function* axpy = lib->newFunction(NS::String::string("axpy", NS::ASCIIStringEncoding));
    MTL::ComputePipelineState* pso = dev->newComputePipelineState(axpy, &err);
    if (!pso)
        return fail("axpy pipeline", err);

    // spec with NEGATE = true, K = 40, through a function descriptor as MLX's get_function_ does.
    bool negate = true;
    int k = 40;
    MTL::FunctionConstantValues* fcv = MTL::FunctionConstantValues::alloc()->init()->autorelease();
    fcv->setConstantValue(&negate, MTL::DataTypeBool, NS::UInteger(0));
    fcv->setConstantValue(&k, MTL::DataTypeInt, NS::UInteger(1));
    MTL::FunctionDescriptor* desc = MTL::FunctionDescriptor::functionDescriptor();
    desc->setName(NS::String::string("spec", NS::ASCIIStringEncoding));
    desc->setSpecializedName(NS::String::string("spec_n1_k40", NS::ASCIIStringEncoding));
    desc->setConstantValues(fcv);
    MTL::Function* spec = lib->newFunction(desc, &err);
    MTL::ComputePipelineState* spso = spec ? dev->newComputePipelineState(spec, &err) : nullptr;
    if (!spso)
        return fail("spec pipeline", err);

    MTL::Buffer* bx = dev->newBuffer(N * 4, MTL::ResourceStorageModeShared);
    MTL::Buffer* by = dev->newBuffer(N * 4, MTL::ResourceStorageModeShared);
    MTL::Buffer* bd = dev->newBuffer(N * 4, MTL::ResourceStorageModeShared);
    float* px = static_cast<float*>(bx->contents());
    float* py = static_cast<float*>(by->contents());
    for (uint32_t i = 0; i < N; i++) {
        px[i] = x(i);
        py[i] = y0(i);
    }

    MTL::CommandQueue* queue = dev->newCommandQueue();
    MTL::CommandBuffer* cb = queue->commandBufferWithUnretainedReferences();
    MTL::ComputeCommandEncoder* enc = cb->computeCommandEncoder(MTL::DispatchTypeConcurrent);
    enc->setComputePipelineState(pso);
    enc->setBuffer(bx, 0, 0);
    enc->setBuffer(by, 0, 1);
    enc->setBytes(&A, sizeof(A), 2);
    enc->dispatchThreads(MTL::Size(N, 1, 1), MTL::Size(256, 1, 1));
    enc->memoryBarrier(MTL::BarrierScopeBuffers);
    enc->setComputePipelineState(spso);
    enc->setBuffer(bd, 0, 0);
    enc->dispatchThreads(MTL::Size(N, 1, 1), MTL::Size(256, 1, 1));
    enc->endEncoding();
    std::atomic<int> handled{-1};
    cb->addCompletedHandler([&handled](MTL::CommandBuffer* c) { handled = int(c->status()); });
    cb->commit();
    cb->waitUntilCompleted();

    uint32_t wrong = 0, swrong = 0;
    const int* pd = static_cast<const int*>(bd->contents());
    for (uint32_t i = 0; i < N; i++) {
        float want = std::fma(A, x(i), y0(i));
        if (py[i] != want && std::fabs(py[i] - want) > 1e-4f * std::fmax(std::fabs(want), 1.0f))
            wrong++;
        if (pd[i] != -k + int(i))
            swrong++;
    }
    std::printf("axpy from the metallib: %u of %u right\n", N - wrong, N);
    std::printf("spec (NEGATE, K=40): %u of %u right\n", N - swrong, N);
    std::printf("completion handler saw status %d (want %d)\n", handled.load(), int(MTL::CommandBufferStatusCompleted));

    // MSL source, as MLX's JIT kernels are built; then a source that does not compile.
    std::ifstream f(argv[2]);
    std::stringstream src;
    src << f.rdbuf();
    MTL::CompileOptions* opts = MTL::CompileOptions::alloc()->init()->autorelease();
    opts->setFastMathEnabled(false);
    MTL::Library* jit = dev->newLibrary(NS::String::string(src.str().c_str(), NS::UTF8StringEncoding), opts, &err);
    if (!jit)
        return fail("newLibrary(source)", err);
    MTL::Library* bad = dev->newLibrary(NS::String::string("not MSL", NS::UTF8StringEncoding), opts, &err);
    std::printf("a source that does not compile: %s (%s)\n", bad ? "LOADED" : "refused",
                err ? err->localizedDescription()->utf8String() : "no error");

    bool ok = wrong == 0 && swrong == 0 && handled.load() == int(MTL::CommandBufferStatusCompleted) && !bad;
    for (NS::Object* o : std::initializer_list<NS::Object*>{jit, bd, by, bx, spso, spec, pso, axpy, lib, queue, dev})
        o->release();
    pool->release();
    std::printf(ok ? "OK\n" : "FAIL\n");
    return ok ? 0 : 1;
}
