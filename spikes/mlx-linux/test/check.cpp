// MLX ops on the GPU (limina-metal, over the transport) against the same ops on MLX's CPU backend.
// Prints one line per check and exits nonzero if any differs.
#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <functional>
#include <string>

#include "mlx/mlx.h"

namespace mx = mlx::core;

static int failures = 0;

static void check(const char* what, bool ok, const std::string& detail = "") {
  std::printf("%-44s %s %s\n", what, ok ? "ok  " : "FAIL", detail.c_str());
  failures += !ok;
}

// f on the GPU and on the CPU, from the same inputs; the largest difference relative to the
// largest magnitude.
static void same(const char* what, std::function<mx::array(mx::StreamOrDevice)> f,
                 float rtol = 1e-4f) {
  auto g = f(mx::Device::gpu);
  auto c = f(mx::Device::cpu);
  mx::eval({g, c});
  auto gf = mx::astype(g, mx::float32, mx::Device::cpu);
  auto cf = mx::astype(c, mx::float32, mx::Device::cpu);
  float diff = mx::max(mx::abs(gf - cf, mx::Device::cpu), mx::Device::cpu).item<float>();
  float scale = mx::max(mx::abs(cf, mx::Device::cpu), mx::Device::cpu).item<float>();
  char d[96];
  std::snprintf(d, sizeof d, "max|gpu-cpu| %.3g of %.3g", diff, scale);
  check(what, g.shape() == c.shape() && diff <= rtol * std::max(scale, 1.0f), d);
}

int main(int argc, char** argv) {
  // `check burn N`: N seconds of 2048^3 matmuls on the GPU, for a host-side look at the GPU.
  if (argc == 3 && std::string(argv[1]) == "burn") {
    auto a = mx::random::normal({2048, 2048});
    auto t0 = std::chrono::steady_clock::now();
    int n = 0;
    while (std::chrono::steady_clock::now() - t0 < std::chrono::seconds(std::atoi(argv[2]))) {
      mx::eval(mx::matmul(a, a));
      n++;
    }
    std::printf("burn: %d matmuls\n", n);
    return 0;
  }

  check("GPU available", mx::gpu::is_available());
  check("default device is the GPU", mx::default_device() == mx::Device::gpu);

  // Exact: 2i+1 for i < 2^20 is an integer a float holds exactly.
  {
    int n = 1 << 20;
    auto a = mx::arange(n, mx::float32);
    auto b = a * 2.0f + 1.0f;
    mx::eval(b);
    auto p = b.data<float>();
    int bad = 0;
    for (int i = 0; i < n; i++) {
      bad += p[i] != 2.0f * i + 1.0f;
    }
    check("2i+1 over 2^20 floats, every element", bad == 0, std::to_string(bad) + " wrong");
  }

  mx::random::seed(7);
  auto x = mx::random::normal({512, 256});
  auto w = mx::random::normal({256, 384});
  auto v = mx::random::normal({4096});
  auto img = mx::random::normal({2, 32, 32, 8});
  auto k = mx::random::normal({16, 3, 3, 8});
  mx::eval({x, w, v, img, k});

  same("add, multiply, exp", [&](auto s) { return mx::exp(mx::multiply(mx::add(x, x, s), x, s), s); });
  same("matmul 512x256 @ 256x384", [&](auto s) { return mx::matmul(x, w, s); }, 1e-3f);
  same("matmul in float16", [&](auto s) {
    return mx::matmul(mx::astype(x, mx::float16, s), mx::astype(w, mx::float16, s), s);
  }, 1e-2f);
  same("sum over rows", [&](auto s) { return mx::sum(x, 1, false, s); }, 1e-4f);
  same("sum of everything", [&](auto s) { return mx::sum(v, false, s); }, 1e-4f);
  same("softmax", [&](auto s) { return mx::softmax(x, std::vector<int>{1}, false, s); });
  same("argmax", [&](auto s) { return mx::argmax(x, 1, false, s); }, 0.0f);
  same("sort", [&](auto s) { return mx::sort(v, s); }, 0.0f);
  same("transpose and copy", [&](auto s) { return mx::copy(mx::transpose(x, s), s); }, 0.0f);
  same("conv2d 3x3", [&](auto s) { return mx::conv2d(img, k, {1, 1}, {1, 1}, {1, 1}, 1, s); }, 1e-3f);
  same("layer norm", [&](auto s) {
    return mx::fast::layer_norm(x, std::nullopt, std::nullopt, 1e-5f, s);
  }, 1e-3f);
  same("quantized matmul, 4 bits", [&](auto s) {
    auto wt = mx::transpose(w, s);
    auto q = mx::quantize(wt, 64, 4, "affine", std::nullopt, s);
    return mx::quantized_matmul(x, q[0], q[1], q[2], true, 64, 4, "affine", s);
  }, 1e-2f);

  // A kernel of our own, from MSL source: only Metal can run it, and the host compiles it. Every
  // value stays below 2^24, so a fused multiply-add rounds no differently from the CPU's.
  {
    int n = 4096;
    auto kernel = mx::fast::metal_kernel(
        "limina_check", {"inp"}, {"out"},
        "uint i = thread_position_in_grid.x;\n"
        "out[i] = inp[i] * inp[i] + float(i % 7);\n");
    auto in = mx::arange(n, mx::float32);
    auto out = kernel({in}, {{n}}, {mx::float32}, {n, 1, 1}, {256, 1, 1}, {}, std::nullopt, false,
                      mx::Device::gpu)[0];
    mx::eval(out);
    auto p = out.data<float>();
    int bad = 0;
    for (int i = 0; i < n; i++) {
      bad += p[i] != float(i) * float(i) + float(i % 7);
    }
    check("a custom Metal kernel from MSL source", bad == 0, std::to_string(bad) + " wrong");
  }

  // Time a larger matmul on each, after a warm-up.
  auto a = mx::random::normal({2048, 2048});
  auto b = mx::random::normal({2048, 2048});
  mx::eval({a, b});
  for (auto dev : {mx::Device::gpu, mx::Device::cpu}) {
    mx::eval(mx::matmul(a, b, dev));
    auto t0 = std::chrono::steady_clock::now();
    for (int i = 0; i < 5; i++) {
      mx::eval(mx::matmul(a, b, dev));
    }
    double s = std::chrono::duration<double>(std::chrono::steady_clock::now() - t0).count() / 5;
    std::printf("matmul 2048^3 on the %s: %.1f ms, %.0f GFLOP/s\n",
                dev == mx::Device::gpu ? "GPU" : "CPU", s * 1e3, 2.0 * 2048 * 2048 * 2048 / s / 1e9);
  }

  std::printf("%s\n", failures ? "FAILED" : "all ok");
  return failures != 0;
}
