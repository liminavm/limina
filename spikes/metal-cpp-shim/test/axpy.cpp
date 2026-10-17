// A compute program written against Apple's metal-cpp, unmodified, the way MLX uses it: a
// .metallib loaded by URL, a kernel specialised with function constants, buffers and setBytes,
// a concurrent encoder with a memory barrier, a std::function completion handler, an MSL source
// library, and autorelease pools. With "unknown" it calls a method limina-metal lacks, which must
// abort naming it. With "thread-exit" a thread's C++ thread_local destructor uses an autorelease
// pool after the shim's own per-thread state may already be gone, as MLX's per-thread state does.
//
// Usage: axpy <kernels.metallib> <kernels.metal> [unknown | thread-exit]

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
