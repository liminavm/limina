// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva
//
// The Metal transport's own costs, measured with Apple's metal-cpp the way MLX calls it. The same
// source builds in a guest against limina-metal and on macOS against Metal.framework, so the
// native run is the floor every guest number is read against (scripts/perf-compute.sh).
//
//   commit-rtt       one tiny dispatch committed and waited for, alone: µs, median and p90
//   dispatch-rate    10 000 tiny dispatches in one command buffer: dispatches per second
//   cb-rate          2 000 command buffers of one tiny dispatch, committed back to back and waited
//                    for at the end: command buffers per second
//   copy-bandwidth   a 256 MiB float4 copy: GB/s, read plus written
//   two-queue        a long single-thread kernel on each of two queues against one alone: 1.00
//                    when the queues run side by side, 2.00 when they take turns
//
// Each prints `ROW <name> <metric> <value>`; everything else is commentary.
// Usage: metal-bench <metal-bench.metallib>

#define NS_PRIVATE_IMPLEMENTATION
#define MTL_PRIVATE_IMPLEMENTATION
#include <Foundation/Foundation.hpp>
#include <Metal/Metal.hpp>

#include <algorithm>
#include <chrono>
#include <cstdio>
#include <vector>

using Clock = std::chrono::steady_clock;
static double secs(Clock::time_point t) { return std::chrono::duration<double>(Clock::now() - t).count(); }

static double median(std::vector<double> v) {
    std::sort(v.begin(), v.end());
    return v[v.size() / 2];
}
static double pct(std::vector<double> v, double p) {
    std::sort(v.begin(), v.end());
    return v[std::min(v.size() - 1, size_t(p * v.size()))];
}

int main(int argc, char** argv) {
    if (argc < 2) {
        std::fprintf(stderr, "usage: %s <metal-bench.metallib>\n", argv[0]);
        return 2;
    }
    NS::AutoreleasePool* pool = NS::AutoreleasePool::alloc()->init();
    MTL::Device* dev = MTL::CreateSystemDefaultDevice();
    if (!dev) {
        std::printf("FAIL: no Metal device\n");
        return 1;
    }
    std::printf("DEVICE %s\n", dev->name()->utf8String());
    NS::Error* err = nullptr;
    NS::URL* url = NS::URL::fileURLWithPath(NS::String::string(argv[1], NS::UTF8StringEncoding));
    MTL::Library* lib = dev->newLibrary(url, &err);
    if (!lib) {
        std::printf("FAIL: library: %s\n", err ? err->localizedDescription()->utf8String() : "?");
        return 1;
    }
    auto pipeline = [&](const char* name) {
        MTL::Function* f = lib->newFunction(NS::String::string(name, NS::ASCIIStringEncoding));
        MTL::ComputePipelineState* p = f ? dev->newComputePipelineState(f, &err) : nullptr;
        if (!p) {
            std::printf("FAIL: pipeline %s\n", name);
            std::exit(1);
        }
        return p;
    };
    MTL::ComputePipelineState* tiny = pipeline("tiny");
    MTL::ComputePipelineState* copy4 = pipeline("copy4");
    MTL::ComputePipelineState* spin = pipeline("spin");
    MTL::CommandQueue* q = dev->newCommandQueue();
    MTL::Buffer* counters = dev->newBuffer(64 * 1024, MTL::ResourceStorageModeShared);

    auto encode_tiny = [&](MTL::ComputeCommandEncoder* e, uint32_t i) {
        e->setComputePipelineState(tiny);
        e->setBuffer(counters, (i % 1024) * 4, 0);
        e->dispatchThreads(MTL::Size(1, 1, 1), MTL::Size(1, 1, 1));
    };
    auto one_tiny = [&](MTL::CommandQueue* queue, uint32_t i) {
        MTL::CommandBuffer* cb = queue->commandBuffer();
        MTL::ComputeCommandEncoder* e = cb->computeCommandEncoder(MTL::DispatchTypeConcurrent);
        encode_tiny(e, i);
        e->endEncoding();
        cb->commit();
        return cb;
    };

    // commit-rtt
    for (uint32_t i = 0; i < 50; i++)
        one_tiny(q, i)->waitUntilCompleted();
    std::vector<double> rtt;
    for (uint32_t i = 0; i < 1000; i++) {
        NS::AutoreleasePool* p = NS::AutoreleasePool::alloc()->init();
        auto t = Clock::now();
        one_tiny(q, i)->waitUntilCompleted();
        rtt.push_back(secs(t) * 1e6);
        p->release();
    }
    std::printf("ROW metal-commit-rtt us_median %.1f\n", median(rtt));
    std::printf("ROW metal-commit-rtt us_p90 %.1f\n", pct(rtt, 0.9));

    // dispatch-rate
    std::vector<double> dr;
    for (int r = 0; r < 7; r++) {
        NS::AutoreleasePool* p = NS::AutoreleasePool::alloc()->init();
        const uint32_t n = 10000;
        auto t = Clock::now();
        MTL::CommandBuffer* cb = q->commandBuffer();
        MTL::ComputeCommandEncoder* e = cb->computeCommandEncoder(MTL::DispatchTypeConcurrent);
        for (uint32_t i = 0; i < n; i++)
            encode_tiny(e, i);
        e->endEncoding();
        cb->commit();
        cb->waitUntilCompleted();
        if (r > 0)
            dr.push_back(n / secs(t));
        p->release();
    }
    std::printf("ROW metal-dispatch-rate per_s %.0f\n", median(dr));

    // cb-rate
    std::vector<double> cr;
    for (int r = 0; r < 5; r++) {
        NS::AutoreleasePool* p = NS::AutoreleasePool::alloc()->init();
        const uint32_t n = 2000;
        auto t = Clock::now();
        MTL::CommandBuffer* last = nullptr;
        for (uint32_t i = 0; i < n; i++)
            last = one_tiny(q, i);
        last->waitUntilCompleted();
        if (r > 0)
            cr.push_back(n / secs(t));
        p->release();
    }
    std::printf("ROW metal-cb-rate per_s %.0f\n", median(cr));

    // copy-bandwidth
    {
        const size_t bytes = 256u << 20;
        MTL::Buffer* src = dev->newBuffer(bytes, MTL::ResourceStorageModePrivate);
        MTL::Buffer* dst = dev->newBuffer(bytes, MTL::ResourceStorageModePrivate);
        std::vector<double> bw;
        for (int r = 0; r < 11; r++) {
            NS::AutoreleasePool* p = NS::AutoreleasePool::alloc()->init();
            auto t = Clock::now();
            MTL::CommandBuffer* cb = q->commandBuffer();
            MTL::ComputeCommandEncoder* e = cb->computeCommandEncoder(MTL::DispatchTypeConcurrent);
            e->setComputePipelineState(copy4);
            e->setBuffer(src, 0, 0);
            e->setBuffer(dst, 0, 1);
            e->dispatchThreads(MTL::Size(bytes / 16, 1, 1), MTL::Size(256, 1, 1));
            e->endEncoding();
            cb->commit();
            cb->waitUntilCompleted();
            if (r > 0)
                bw.push_back(2.0 * bytes / secs(t) / 1e9);
            p->release();
        }
        std::printf("ROW metal-copy-bandwidth GB_s %.1f\n", median(bw));
        src->release();
        dst->release();
    }

    // two-queue
    {
        MTL::Buffer* out = dev->newBuffer(64, MTL::ResourceStorageModeShared);
        auto run = [&](MTL::CommandQueue* queue, uint32_t n, uint32_t slot) {
            MTL::CommandBuffer* cb = queue->commandBuffer();
            MTL::ComputeCommandEncoder* e = cb->computeCommandEncoder(MTL::DispatchTypeConcurrent);
            e->setComputePipelineState(spin);
            e->setBuffer(out, slot * 16, 0);
            e->setBytes(&n, sizeof(n), 1);
            e->dispatchThreads(MTL::Size(1, 1, 1), MTL::Size(1, 1, 1));
            e->endEncoding();
            cb->commit();
            return cb;
        };
        MTL::CommandQueue* qb = dev->newCommandQueue();
        uint32_t n = 1u << 20;
        double alone = 0;
        for (;;) {
            auto t = Clock::now();
            run(q, n, 0)->waitUntilCompleted();
            alone = secs(t);
            if (alone > 0.15 || n >= (1u << 31))
                break;
            n *= 2;
        }
        std::vector<double> ratio;
        for (int r = 0; r < 3; r++) {
            auto t = Clock::now();
            run(q, n, 0)->waitUntilCompleted();
            alone = secs(t);
            t = Clock::now();
            MTL::CommandBuffer* a = run(q, n, 0);
            MTL::CommandBuffer* b = run(qb, n, 1);
            a->waitUntilCompleted();
            b->waitUntilCompleted();
            ratio.push_back(secs(t) / alone);
        }
        std::printf("two-queue: spin of %u steps, %.0f ms alone\n", n, alone * 1e3);
        std::printf("ROW metal-two-queue ratio %.2f\n", median(ratio));
    }
    pool->release();
    return 0;
}
