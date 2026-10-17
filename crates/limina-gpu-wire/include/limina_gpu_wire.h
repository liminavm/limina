/* Generated from crates/limina-gpu-wire by its c_header test; do not edit.
 * SPDX-License-Identifier: MIT
 *
 * The guest<->GPU host protocol of limina's Metal-level GPU transport. The Rust crate is
 * the definition; its doc comments are the documentation. */
#ifndef LIMINA_GPU_WIRE_H
#define LIMINA_GPU_WIRE_H

#include <stddef.h>
#include <stdint.h>

#define LMN_WIRE_VERSION 5u

/* Ring (crates/limina-gpu-wire/src/ring.rs). */
#define LMN_RING_MAGIC 0x52474d4cu
#define LMN_RING_DATA_OFFSET 512u
#define LMN_RING_CMD_ALIGN 8u
#define LMN_RING_MIN_DATA_SIZE 4096u
#define LMN_RING_STATE_RUNNING 0u
#define LMN_RING_STATE_PARKED 1u
#define LMN_RING_OFF_MAGIC 0u
#define LMN_RING_OFF_VERSION 4u
#define LMN_RING_OFF_TAIL 128u
#define LMN_RING_OFF_HEAD 256u
#define LMN_RING_OFF_COMPLETED 264u
#define LMN_RING_OFF_STATE 272u
#define LMN_RING_OFF_ERROR 276u
#define LMN_RING_OFF_ERROR_AT 280u

/* ring::Error: why a context was lost, or why a reply failed. */
#define LMN_ERROR_CORRUPT 1u
#define LMN_ERROR_MALFORMED 2u
#define LMN_ERROR_UNKNOWN_OP 3u
#define LMN_ERROR_BAD_PAYLOAD 4u
#define LMN_ERROR_BAD_HANDLE 5u
#define LMN_ERROR_BAD_RANGE 6u
#define LMN_ERROR_BAD_VALUE 7u
#define LMN_ERROR_SHADER_COMPILE 8u
#define LMN_ERROR_PIPELINE_CREATE 9u
#define LMN_ERROR_BAD_STATE 10u
#define LMN_ERROR_GPU_FAULT 11u
#define LMN_ERROR_OUT_OF_MEMORY 12u
#define LMN_ERROR_RESTORE 13u
#define LMN_ERROR_HOST_GONE 14u

#define LMN_ARENA_PAGE 16384u
#define LMN_MAX_COLOR_ATTACHMENTS 8u
#define LMN_MAX_SAMPLE_POSITIONS 16u
#define LMN_REPLY_DONE 1u
#define LMN_SYNC_FENCE 0u
#define LMN_SYNC_EVENT 1u
#define LMN_SYNC_SHARED_EVENT 2u
#define LMN_BARRIER_AFTER_STAGES 0u
#define LMN_BARRIER_AFTER_ENCODER_STAGES 1u
#define LMN_BARRIER_AFTER_QUEUE_STAGES 2u
#define LMN_RS_STENCIL_REFERENCE 0u
#define LMN_RS_WINDING 1u
#define LMN_RS_CULL_MODE 2u
#define LMN_RS_DEPTH_CLIP_MODE 3u
#define LMN_RS_DEPTH_BIAS 4u
#define LMN_RS_DEPTH_TEST_BOUNDS 5u
#define LMN_RS_VISIBILITY_RESULT 6u
#define LMN_RS_COLOR_STORE_ACTION 7u
#define LMN_RS_DEPTH_STORE_ACTION 8u
#define LMN_RS_STENCIL_STORE_ACTION 9u

/* Opcodes. */
#define LMN_OP_PAD 0x0000u
#define LMN_OP_CREATE_TEXTURE 0x0100u
#define LMN_OP_CREATE_TEXTURE_FROM_SURFACE 0x0109u
#define LMN_OP_CREATE_TEXTURE_VIEW 0x0101u
#define LMN_OP_CREATE_SAMPLER 0x0102u
#define LMN_OP_CREATE_LIBRARY 0x0103u
#define LMN_OP_CREATE_LIBRARY_DATA 0x010bu
#define LMN_OP_CREATE_RENDER_PIPELINE 0x0104u
#define LMN_OP_CREATE_COMPUTE_PIPELINE 0x0105u
#define LMN_OP_CREATE_DEPTH_STENCIL 0x0106u
#define LMN_OP_CREATE_ARGUMENT_TABLE 0x0107u
#define LMN_OP_CREATE_SYNC 0x0108u
#define LMN_OP_CREATE_QUEUE 0x010cu
#define LMN_OP_CREATE_COUNTER_HEAP 0x010au
#define LMN_OP_DESTROY 0x01ffu
#define LMN_OP_SET_ADDRESSES 0x0180u
#define LMN_OP_SET_SHARED_EVENT 0x0181u
#define LMN_OP_TEXTURE_TRANSFER 0x0182u
#define LMN_OP_QUERY_DEVICE 0x0190u
#define LMN_OP_QUERY_TEXTURE_SIZE 0x0191u
#define LMN_OP_QUERY_LINEAR_ALIGNMENT 0x0192u
#define LMN_OP_QUERY_TIMESTAMP 0x0193u
#define LMN_OP_BEGIN_COMMAND_BUFFER 0x0200u
#define LMN_OP_END_COMMAND_BUFFER 0x0201u
#define LMN_OP_BEGIN_COMPUTE 0x0202u
#define LMN_OP_BEGIN_RENDER_PASS 0x0203u
#define LMN_OP_END_ENCODING 0x0204u
#define LMN_OP_BARRIER 0x0205u
#define LMN_OP_FENCE 0x0206u
#define LMN_OP_WRITE_TIMESTAMP 0x0207u
#define LMN_OP_RESOLVE_COUNTER_HEAP 0x0208u
#define LMN_OP_SET_COMPUTE_PIPELINE 0x0210u
#define LMN_OP_SET_ARGUMENT_TABLE 0x0211u
#define LMN_OP_DISPATCH_THREADS 0x0212u
#define LMN_OP_DISPATCH_THREADGROUPS 0x0213u
#define LMN_OP_DISPATCH_INDIRECT 0x0214u
#define LMN_OP_COPY_BUFFER 0x0215u
#define LMN_OP_COPY_BUFFER_TEXTURE 0x0216u
#define LMN_OP_COPY_TEXTURE 0x0217u
#define LMN_OP_FILL_BUFFER 0x0218u
#define LMN_OP_SET_RENDER_PIPELINE 0x0220u
#define LMN_OP_SET_VIEWPORTS 0x0221u
#define LMN_OP_SET_SCISSORS 0x0222u
#define LMN_OP_SET_DEPTH_STENCIL 0x0223u
#define LMN_OP_RENDER_STATE 0x0224u
#define LMN_OP_SET_AMPLIFICATION 0x0225u
#define LMN_OP_DRAW 0x0230u
#define LMN_OP_DRAW_INDEXED 0x0231u
#define LMN_OP_DRAW_INDIRECT 0x0232u
#define LMN_OP_COMMIT 0x0300u
#define LMN_OP_QUEUE_EVENT 0x0301u

struct lmn_cmd_header {
   uint16_t op;
   uint16_t flags;
   uint32_t len;
};
_Static_assert(sizeof(struct lmn_cmd_header) == 8, "lmn_cmd_header");

/* A range start in an arena: GPU address `arena base + offset`. Arena `0` is "none". */
struct lmn_buf_ref {
   uint32_t arena;
   uint32_t _pad;
   uint64_t offset;
};
_Static_assert(sizeof(struct lmn_buf_ref) == 16, "lmn_buf_ref size");
_Static_assert(offsetof(struct lmn_buf_ref, arena) == 0, "lmn_buf_ref.arena");
_Static_assert(offsetof(struct lmn_buf_ref, _pad) == 4, "lmn_buf_ref._pad");
_Static_assert(offsetof(struct lmn_buf_ref, offset) == 8, "lmn_buf_ref.offset");

/* Written by the host for a command that names it. The host writes `status` and `values` */
/* first and `done` (= `REPLY_DONE`) last, with release ordering; the guest zeroes `done` */
/* before sending and reads the rest after seeing it set. */
struct lmn_reply {
   uint32_t done;
   uint32_t status;
   uint64_t values[6];
};
_Static_assert(sizeof(struct lmn_reply) == 56, "lmn_reply size");
_Static_assert(offsetof(struct lmn_reply, done) == 0, "lmn_reply.done");
_Static_assert(offsetof(struct lmn_reply, status) == 4, "lmn_reply.status");
_Static_assert(offsetof(struct lmn_reply, values) == 8, "lmn_reply.values");

/* Written by the host for each [`Commit`] before its seqno is published, `done` last. */
struct lmn_feedback {
   uint32_t done;
   uint32_t error;
   double gpu_start;
   double gpu_end;
};
_Static_assert(sizeof(struct lmn_feedback) == 24, "lmn_feedback size");
_Static_assert(offsetof(struct lmn_feedback, done) == 0, "lmn_feedback.done");
_Static_assert(offsetof(struct lmn_feedback, error) == 4, "lmn_feedback.error");
_Static_assert(offsetof(struct lmn_feedback, gpu_start) == 8, "lmn_feedback.gpu_start");
_Static_assert(offsetof(struct lmn_feedback, gpu_end) == 16, "lmn_feedback.gpu_end");

/* What the guest needs to know about the host GPU, written by [`QueryDevice`]. */
struct lmn_device_info {
   uint32_t version;
   uint32_t apple_family;
   uint8_t name[32];
   uint8_t architecture[32];
   uint64_t registry_id;
   uint32_t sample_counts;
   uint32_t max_threadgroup_memory;
   uint32_t max_threads_per_threadgroup[3];
   uint32_t max_argument_buffer_samplers;
   uint64_t max_buffer_length;
   uint64_t recommended_max_working_set;
   uint64_t timestamp_frequency;
   uint32_t sparse_tile_bytes;
   uint32_t heap_buffer_align;
   uint32_t os_version[3];
   uint32_t features;
};
_Static_assert(sizeof(struct lmn_device_info) == 152, "lmn_device_info size");
_Static_assert(offsetof(struct lmn_device_info, version) == 0, "lmn_device_info.version");
_Static_assert(offsetof(struct lmn_device_info, apple_family) == 4, "lmn_device_info.apple_family");
_Static_assert(offsetof(struct lmn_device_info, name) == 8, "lmn_device_info.name");
_Static_assert(offsetof(struct lmn_device_info, architecture) == 40, "lmn_device_info.architecture");
_Static_assert(offsetof(struct lmn_device_info, registry_id) == 72, "lmn_device_info.registry_id");
_Static_assert(offsetof(struct lmn_device_info, sample_counts) == 80, "lmn_device_info.sample_counts");
_Static_assert(offsetof(struct lmn_device_info, max_threadgroup_memory) == 84, "lmn_device_info.max_threadgroup_memory");
_Static_assert(offsetof(struct lmn_device_info, max_threads_per_threadgroup) == 88, "lmn_device_info.max_threads_per_threadgroup");
_Static_assert(offsetof(struct lmn_device_info, max_argument_buffer_samplers) == 100, "lmn_device_info.max_argument_buffer_samplers");
_Static_assert(offsetof(struct lmn_device_info, max_buffer_length) == 104, "lmn_device_info.max_buffer_length");
_Static_assert(offsetof(struct lmn_device_info, recommended_max_working_set) == 112, "lmn_device_info.recommended_max_working_set");
_Static_assert(offsetof(struct lmn_device_info, timestamp_frequency) == 120, "lmn_device_info.timestamp_frequency");
_Static_assert(offsetof(struct lmn_device_info, sparse_tile_bytes) == 128, "lmn_device_info.sparse_tile_bytes");
_Static_assert(offsetof(struct lmn_device_info, heap_buffer_align) == 132, "lmn_device_info.heap_buffer_align");
_Static_assert(offsetof(struct lmn_device_info, os_version) == 136, "lmn_device_info.os_version");
_Static_assert(offsetof(struct lmn_device_info, features) == 148, "lmn_device_info.features");

/* The fields of an `MTLTextureDescriptor` the guest chooses. */
struct lmn_texture_desc {
   uint32_t texture_type;
   uint32_t pixel_format;
   uint32_t width;
   uint32_t height;
   uint32_t depth;
   uint32_t mip_levels;
   uint32_t array_length;
   uint32_t sample_count;
   uint32_t usage;
   uint32_t allow_gpu_optimized;
   uint32_t _pad[2];
};
_Static_assert(sizeof(struct lmn_texture_desc) == 48, "lmn_texture_desc size");
_Static_assert(offsetof(struct lmn_texture_desc, texture_type) == 0, "lmn_texture_desc.texture_type");
_Static_assert(offsetof(struct lmn_texture_desc, pixel_format) == 4, "lmn_texture_desc.pixel_format");
_Static_assert(offsetof(struct lmn_texture_desc, width) == 8, "lmn_texture_desc.width");
_Static_assert(offsetof(struct lmn_texture_desc, height) == 12, "lmn_texture_desc.height");
_Static_assert(offsetof(struct lmn_texture_desc, depth) == 16, "lmn_texture_desc.depth");
_Static_assert(offsetof(struct lmn_texture_desc, mip_levels) == 20, "lmn_texture_desc.mip_levels");
_Static_assert(offsetof(struct lmn_texture_desc, array_length) == 24, "lmn_texture_desc.array_length");
_Static_assert(offsetof(struct lmn_texture_desc, sample_count) == 28, "lmn_texture_desc.sample_count");
_Static_assert(offsetof(struct lmn_texture_desc, usage) == 32, "lmn_texture_desc.usage");
_Static_assert(offsetof(struct lmn_texture_desc, allow_gpu_optimized) == 36, "lmn_texture_desc.allow_gpu_optimized");
_Static_assert(offsetof(struct lmn_texture_desc, _pad) == 40, "lmn_texture_desc._pad");

/* A texture in an arena. With `bytes_per_row` zero it is placed in the arena's heap */
/* (`newTextureWithDescriptor:offset:`); otherwise it views the arena's buffer linearly */
/* (`newTextureWithDescriptor:offset:bytesPerRow:`). The reply's `values[0]` is its */
/* `gpuResourceID`. */
struct lmn_create_texture {
   uint32_t handle;
   uint32_t arena;
   uint64_t offset;
   uint64_t bytes_per_row;
   struct lmn_texture_desc desc;
   struct lmn_buf_ref reply;
};
_Static_assert(sizeof(struct lmn_create_texture) == 88, "lmn_create_texture size");
_Static_assert(offsetof(struct lmn_create_texture, handle) == 0, "lmn_create_texture.handle");
_Static_assert(offsetof(struct lmn_create_texture, arena) == 4, "lmn_create_texture.arena");
_Static_assert(offsetof(struct lmn_create_texture, offset) == 8, "lmn_create_texture.offset");
_Static_assert(offsetof(struct lmn_create_texture, bytes_per_row) == 16, "lmn_create_texture.bytes_per_row");
_Static_assert(offsetof(struct lmn_create_texture, desc) == 24, "lmn_create_texture.desc");
_Static_assert(offsetof(struct lmn_create_texture, reply) == 72, "lmn_create_texture.reply");

/* A texture whose storage is surface `surface` of the context (`newTextureWithDescriptor: */
/* iosurface:plane:`), as the worker filed it for a buffer shared beyond this transport: a 2D */
/* texture of one level and one sample, no larger than the surface, in the surface's own pixel */
/* layout. The reply's `values[0]` is its `gpuResourceID`. */
struct lmn_create_texture_from_surface {
   uint32_t handle;
   uint32_t surface;
   struct lmn_texture_desc desc;
   struct lmn_buf_ref reply;
};
_Static_assert(sizeof(struct lmn_create_texture_from_surface) == 72, "lmn_create_texture_from_surface size");
_Static_assert(offsetof(struct lmn_create_texture_from_surface, handle) == 0, "lmn_create_texture_from_surface.handle");
_Static_assert(offsetof(struct lmn_create_texture_from_surface, surface) == 4, "lmn_create_texture_from_surface.surface");
_Static_assert(offsetof(struct lmn_create_texture_from_surface, desc) == 8, "lmn_create_texture_from_surface.desc");
_Static_assert(offsetof(struct lmn_create_texture_from_surface, reply) == 56, "lmn_create_texture_from_surface.reply");

/* `newTextureViewWithPixelFormat:textureType:levels:slices:[swizzle:]` of `parent`. The */
/* reply's `values[0]` is its `gpuResourceID`. */
struct lmn_create_texture_view {
   uint32_t handle;
   uint32_t parent;
   uint32_t pixel_format;
   uint32_t texture_type;
   uint32_t base_level;
   uint32_t level_count;
   uint32_t base_slice;
   uint32_t slice_count;
   uint32_t swizzle[4];
   uint32_t use_swizzle;
   uint32_t _pad;
   struct lmn_buf_ref reply;
};
_Static_assert(sizeof(struct lmn_create_texture_view) == 72, "lmn_create_texture_view size");
_Static_assert(offsetof(struct lmn_create_texture_view, handle) == 0, "lmn_create_texture_view.handle");
_Static_assert(offsetof(struct lmn_create_texture_view, parent) == 4, "lmn_create_texture_view.parent");
_Static_assert(offsetof(struct lmn_create_texture_view, pixel_format) == 8, "lmn_create_texture_view.pixel_format");
_Static_assert(offsetof(struct lmn_create_texture_view, texture_type) == 12, "lmn_create_texture_view.texture_type");
_Static_assert(offsetof(struct lmn_create_texture_view, base_level) == 16, "lmn_create_texture_view.base_level");
_Static_assert(offsetof(struct lmn_create_texture_view, level_count) == 20, "lmn_create_texture_view.level_count");
_Static_assert(offsetof(struct lmn_create_texture_view, base_slice) == 24, "lmn_create_texture_view.base_slice");
_Static_assert(offsetof(struct lmn_create_texture_view, slice_count) == 28, "lmn_create_texture_view.slice_count");
_Static_assert(offsetof(struct lmn_create_texture_view, swizzle) == 32, "lmn_create_texture_view.swizzle");
_Static_assert(offsetof(struct lmn_create_texture_view, use_swizzle) == 48, "lmn_create_texture_view.use_swizzle");
_Static_assert(offsetof(struct lmn_create_texture_view, _pad) == 52, "lmn_create_texture_view._pad");
_Static_assert(offsetof(struct lmn_create_texture_view, reply) == 56, "lmn_create_texture_view.reply");

/* The fields of an `MTLSamplerDescriptor` the guest chooses. */
struct lmn_sampler_desc {
   uint32_t min_filter;
   uint32_t mag_filter;
   uint32_t mip_filter;
   uint32_t address_s;
   uint32_t address_t;
   uint32_t address_r;
   uint32_t border_color;
   uint32_t compare_function;
   uint32_t reduction_mode;
   uint32_t normalized_coordinates;
   uint32_t max_anisotropy;
   float lod_min_clamp;
   float lod_max_clamp;
   uint32_t _pad;
};
_Static_assert(sizeof(struct lmn_sampler_desc) == 56, "lmn_sampler_desc size");
_Static_assert(offsetof(struct lmn_sampler_desc, min_filter) == 0, "lmn_sampler_desc.min_filter");
_Static_assert(offsetof(struct lmn_sampler_desc, mag_filter) == 4, "lmn_sampler_desc.mag_filter");
_Static_assert(offsetof(struct lmn_sampler_desc, mip_filter) == 8, "lmn_sampler_desc.mip_filter");
_Static_assert(offsetof(struct lmn_sampler_desc, address_s) == 12, "lmn_sampler_desc.address_s");
_Static_assert(offsetof(struct lmn_sampler_desc, address_t) == 16, "lmn_sampler_desc.address_t");
_Static_assert(offsetof(struct lmn_sampler_desc, address_r) == 20, "lmn_sampler_desc.address_r");
_Static_assert(offsetof(struct lmn_sampler_desc, border_color) == 24, "lmn_sampler_desc.border_color");
_Static_assert(offsetof(struct lmn_sampler_desc, compare_function) == 28, "lmn_sampler_desc.compare_function");
_Static_assert(offsetof(struct lmn_sampler_desc, reduction_mode) == 32, "lmn_sampler_desc.reduction_mode");
_Static_assert(offsetof(struct lmn_sampler_desc, normalized_coordinates) == 36, "lmn_sampler_desc.normalized_coordinates");
_Static_assert(offsetof(struct lmn_sampler_desc, max_anisotropy) == 40, "lmn_sampler_desc.max_anisotropy");
_Static_assert(offsetof(struct lmn_sampler_desc, lod_min_clamp) == 44, "lmn_sampler_desc.lod_min_clamp");
_Static_assert(offsetof(struct lmn_sampler_desc, lod_max_clamp) == 48, "lmn_sampler_desc.lod_max_clamp");
_Static_assert(offsetof(struct lmn_sampler_desc, _pad) == 52, "lmn_sampler_desc._pad");

/* An `MTLSamplerState`. The reply's `values[0]` is its `gpuResourceID`. */
struct lmn_create_sampler {
   uint32_t handle;
   uint32_t _pad;
   struct lmn_sampler_desc desc;
   struct lmn_buf_ref reply;
};
_Static_assert(sizeof(struct lmn_create_sampler) == 80, "lmn_create_sampler size");
_Static_assert(offsetof(struct lmn_create_sampler, handle) == 0, "lmn_create_sampler.handle");
_Static_assert(offsetof(struct lmn_create_sampler, _pad) == 4, "lmn_create_sampler._pad");
_Static_assert(offsetof(struct lmn_create_sampler, desc) == 8, "lmn_create_sampler.desc");
_Static_assert(offsetof(struct lmn_create_sampler, reply) == 64, "lmn_create_sampler.reply");

/* An MSL library, compiled by the context's `MTL4Compiler` from the `source_len` bytes of */
/* UTF-8 that follow. */
struct lmn_create_library {
   uint32_t handle;
   uint32_t source_len;
   uint32_t language_version;
   uint32_t math_mode;
   uint32_t math_fp_functions;
   uint32_t _pad;
   struct lmn_buf_ref reply;
};
_Static_assert(sizeof(struct lmn_create_library) == 40, "lmn_create_library size");
_Static_assert(offsetof(struct lmn_create_library, handle) == 0, "lmn_create_library.handle");
_Static_assert(offsetof(struct lmn_create_library, source_len) == 4, "lmn_create_library.source_len");
_Static_assert(offsetof(struct lmn_create_library, language_version) == 8, "lmn_create_library.language_version");
_Static_assert(offsetof(struct lmn_create_library, math_mode) == 12, "lmn_create_library.math_mode");
_Static_assert(offsetof(struct lmn_create_library, math_fp_functions) == 16, "lmn_create_library.math_fp_functions");
_Static_assert(offsetof(struct lmn_create_library, _pad) == 20, "lmn_create_library._pad");
_Static_assert(offsetof(struct lmn_create_library, reply) == 24, "lmn_create_library.reply");

/* A library from a compiled `.metallib` (`newLibraryWithData:`), `data_len` bytes of at */
/* most `MAX_LIBRARY_DATA`: at `data` in an arena, or, with `data` none, following the */
/* command. The host keeps the bytes in the object's record, so a restore needs no arena. */
/* Only for a host with `feature::LIBRARY_DATA`. */
struct lmn_create_library_data {
   uint32_t handle;
   uint32_t _pad;
   uint64_t data_len;
   struct lmn_buf_ref data;
   struct lmn_buf_ref reply;
};
_Static_assert(sizeof(struct lmn_create_library_data) == 48, "lmn_create_library_data size");
_Static_assert(offsetof(struct lmn_create_library_data, handle) == 0, "lmn_create_library_data.handle");
_Static_assert(offsetof(struct lmn_create_library_data, _pad) == 4, "lmn_create_library_data._pad");
_Static_assert(offsetof(struct lmn_create_library_data, data_len) == 8, "lmn_create_library_data.data_len");
_Static_assert(offsetof(struct lmn_create_library_data, data) == 16, "lmn_create_library_data.data");
_Static_assert(offsetof(struct lmn_create_library_data, reply) == 32, "lmn_create_library_data.reply");

/* An `MTL4RenderPipelineDescriptor`; the vertex function's name follows, then the */
/* fragment's (absent when `fragment_library` is `0`). */
struct lmn_create_render_pipeline {
   uint32_t handle;
   uint32_t vertex_library;
   uint32_t fragment_library;
   uint32_t vertex_name_len;
   uint32_t fragment_name_len;
   uint32_t topology_class;
   uint32_t raster_sample_count;
   uint32_t rasterization_enabled;
   uint32_t max_vertex_amplification;
   uint32_t _pad;
   uint32_t color_formats[8];
   struct lmn_buf_ref reply;
};
_Static_assert(sizeof(struct lmn_create_render_pipeline) == 88, "lmn_create_render_pipeline size");
_Static_assert(offsetof(struct lmn_create_render_pipeline, handle) == 0, "lmn_create_render_pipeline.handle");
_Static_assert(offsetof(struct lmn_create_render_pipeline, vertex_library) == 4, "lmn_create_render_pipeline.vertex_library");
_Static_assert(offsetof(struct lmn_create_render_pipeline, fragment_library) == 8, "lmn_create_render_pipeline.fragment_library");
_Static_assert(offsetof(struct lmn_create_render_pipeline, vertex_name_len) == 12, "lmn_create_render_pipeline.vertex_name_len");
_Static_assert(offsetof(struct lmn_create_render_pipeline, fragment_name_len) == 16, "lmn_create_render_pipeline.fragment_name_len");
_Static_assert(offsetof(struct lmn_create_render_pipeline, topology_class) == 20, "lmn_create_render_pipeline.topology_class");
_Static_assert(offsetof(struct lmn_create_render_pipeline, raster_sample_count) == 24, "lmn_create_render_pipeline.raster_sample_count");
_Static_assert(offsetof(struct lmn_create_render_pipeline, rasterization_enabled) == 28, "lmn_create_render_pipeline.rasterization_enabled");
_Static_assert(offsetof(struct lmn_create_render_pipeline, max_vertex_amplification) == 32, "lmn_create_render_pipeline.max_vertex_amplification");
_Static_assert(offsetof(struct lmn_create_render_pipeline, _pad) == 36, "lmn_create_render_pipeline._pad");
_Static_assert(offsetof(struct lmn_create_render_pipeline, color_formats) == 40, "lmn_create_render_pipeline.color_formats");
_Static_assert(offsetof(struct lmn_create_render_pipeline, reply) == 72, "lmn_create_render_pipeline.reply");

/* An `MTL4ComputePipelineDescriptor`; `constant_count` [`FunctionConstant`]s follow, then */
/* the function's name. With constants, the function is an `MTL4SpecializedFunctionDescriptor` */
/* over the library's; only for a host with `feature::FUNCTION_CONSTANTS`. */
struct lmn_create_compute_pipeline {
   uint32_t handle;
   uint32_t library;
   uint32_t name_len;
   uint32_t constant_count;
   uint64_t max_total_threads;
   struct lmn_buf_ref reply;
};
_Static_assert(sizeof(struct lmn_create_compute_pipeline) == 40, "lmn_create_compute_pipeline size");
_Static_assert(offsetof(struct lmn_create_compute_pipeline, handle) == 0, "lmn_create_compute_pipeline.handle");
_Static_assert(offsetof(struct lmn_create_compute_pipeline, library) == 4, "lmn_create_compute_pipeline.library");
_Static_assert(offsetof(struct lmn_create_compute_pipeline, name_len) == 8, "lmn_create_compute_pipeline.name_len");
_Static_assert(offsetof(struct lmn_create_compute_pipeline, constant_count) == 12, "lmn_create_compute_pipeline.constant_count");
_Static_assert(offsetof(struct lmn_create_compute_pipeline, max_total_threads) == 16, "lmn_create_compute_pipeline.max_total_threads");
_Static_assert(offsetof(struct lmn_create_compute_pipeline, reply) == 24, "lmn_create_compute_pipeline.reply");

/* One `setConstantValue:type:atIndex:`. `data_type` is a scalar `MTLDataType` (bool, char, */
/* short, int, long, their unsigned forms, half, bfloat or float); the value sits in the low */
/* bytes of `value`, little-endian, and the bytes above it are zero. */
struct lmn_function_constant {
   uint32_t index;
   uint32_t data_type;
   uint64_t value;
};
_Static_assert(sizeof(struct lmn_function_constant) == 16, "lmn_function_constant size");
_Static_assert(offsetof(struct lmn_function_constant, index) == 0, "lmn_function_constant.index");
_Static_assert(offsetof(struct lmn_function_constant, data_type) == 4, "lmn_function_constant.data_type");
_Static_assert(offsetof(struct lmn_function_constant, value) == 8, "lmn_function_constant.value");

/* An `MTLStencilDescriptor`, with `MTLStencilOperation` and `MTLCompareFunction` values. */
struct lmn_stencil_desc {
   uint32_t stencil_failure;
   uint32_t depth_failure;
   uint32_t depth_stencil_pass;
   uint32_t compare_function;
   uint32_t read_mask;
   uint32_t write_mask;
};
_Static_assert(sizeof(struct lmn_stencil_desc) == 24, "lmn_stencil_desc size");
_Static_assert(offsetof(struct lmn_stencil_desc, stencil_failure) == 0, "lmn_stencil_desc.stencil_failure");
_Static_assert(offsetof(struct lmn_stencil_desc, depth_failure) == 4, "lmn_stencil_desc.depth_failure");
_Static_assert(offsetof(struct lmn_stencil_desc, depth_stencil_pass) == 8, "lmn_stencil_desc.depth_stencil_pass");
_Static_assert(offsetof(struct lmn_stencil_desc, compare_function) == 12, "lmn_stencil_desc.compare_function");
_Static_assert(offsetof(struct lmn_stencil_desc, read_mask) == 16, "lmn_stencil_desc.read_mask");
_Static_assert(offsetof(struct lmn_stencil_desc, write_mask) == 20, "lmn_stencil_desc.write_mask");

/* An `MTLDepthStencilState`. */
struct lmn_create_depth_stencil {
   uint32_t handle;
   uint32_t depth_compare;
   uint32_t depth_write;
   uint32_t has_front;
   uint32_t has_back;
   uint32_t _pad;
   struct lmn_stencil_desc front;
   struct lmn_stencil_desc back;
};
_Static_assert(sizeof(struct lmn_create_depth_stencil) == 72, "lmn_create_depth_stencil size");
_Static_assert(offsetof(struct lmn_create_depth_stencil, handle) == 0, "lmn_create_depth_stencil.handle");
_Static_assert(offsetof(struct lmn_create_depth_stencil, depth_compare) == 4, "lmn_create_depth_stencil.depth_compare");
_Static_assert(offsetof(struct lmn_create_depth_stencil, depth_write) == 8, "lmn_create_depth_stencil.depth_write");
_Static_assert(offsetof(struct lmn_create_depth_stencil, has_front) == 12, "lmn_create_depth_stencil.has_front");
_Static_assert(offsetof(struct lmn_create_depth_stencil, has_back) == 16, "lmn_create_depth_stencil.has_back");
_Static_assert(offsetof(struct lmn_create_depth_stencil, _pad) == 20, "lmn_create_depth_stencil._pad");
_Static_assert(offsetof(struct lmn_create_depth_stencil, front) == 24, "lmn_create_depth_stencil.front");
_Static_assert(offsetof(struct lmn_create_depth_stencil, back) == 48, "lmn_create_depth_stencil.back");

/* An `MTL4ArgumentTable` with `max_buffer_bind_count` buffer bindings. */
struct lmn_create_argument_table {
   uint32_t handle;
   uint32_t max_buffer_bind_count;
};
_Static_assert(sizeof(struct lmn_create_argument_table) == 8, "lmn_create_argument_table size");
_Static_assert(offsetof(struct lmn_create_argument_table, handle) == 0, "lmn_create_argument_table.handle");
_Static_assert(offsetof(struct lmn_create_argument_table, max_buffer_bind_count) == 4, "lmn_create_argument_table.max_buffer_bind_count");

/* An `MTLFence`, `MTLEvent` or `MTLSharedEvent` (`kind`, one of `sync_kind`). A shared */
/* event starts at `initial`, and the host keeps the `u64` at `mirror` at least as high as its */
/* signaled value, which is how the guest reads and waits on it. */
struct lmn_sync_object {
   uint32_t handle;
   uint32_t kind;
   uint64_t initial;
   struct lmn_buf_ref mirror;
};
_Static_assert(sizeof(struct lmn_sync_object) == 32, "lmn_sync_object size");
_Static_assert(offsetof(struct lmn_sync_object, handle) == 0, "lmn_sync_object.handle");
_Static_assert(offsetof(struct lmn_sync_object, kind) == 4, "lmn_sync_object.kind");
_Static_assert(offsetof(struct lmn_sync_object, initial) == 8, "lmn_sync_object.initial");
_Static_assert(offsetof(struct lmn_sync_object, mirror) == 16, "lmn_sync_object.mirror");

/* An `MTL4CommandQueue`, which the context's residency set is added to. The host stores the */
/* highest finished seqno of its commits to the `u64` at `completed`, in the ring's arena and */
/* 8-aligned, which is how the guest and the guest kernel's fences learn of them. */
struct lmn_create_queue {
   uint32_t handle;
   uint32_t _pad;
   struct lmn_buf_ref completed;
};
_Static_assert(sizeof(struct lmn_create_queue) == 24, "lmn_create_queue size");
_Static_assert(offsetof(struct lmn_create_queue, handle) == 0, "lmn_create_queue.handle");
_Static_assert(offsetof(struct lmn_create_queue, _pad) == 4, "lmn_create_queue._pad");
_Static_assert(offsetof(struct lmn_create_queue, completed) == 8, "lmn_create_queue.completed");

/* An `MTL4CounterHeap` of `count` timestamp entries (`MTL4CounterHeapTypeTimestamp`). */
struct lmn_create_counter_heap {
   uint32_t handle;
   uint32_t count;
   struct lmn_buf_ref reply;
};
_Static_assert(sizeof(struct lmn_create_counter_heap) == 24, "lmn_create_counter_heap size");
_Static_assert(offsetof(struct lmn_create_counter_heap, handle) == 0, "lmn_create_counter_heap.handle");
_Static_assert(offsetof(struct lmn_create_counter_heap, count) == 4, "lmn_create_counter_heap.count");
_Static_assert(offsetof(struct lmn_create_counter_heap, reply) == 8, "lmn_create_counter_heap.reply");

/* Releases the guest's handle. Metal 4 does not retain what command buffers reference, so */
/* the host keeps the object until every commit made before the destroy has completed; the */
/* handle is free for reuse at once. */
struct lmn_destroy {
   uint32_t handle;
   uint32_t _pad;
};
_Static_assert(sizeof(struct lmn_destroy) == 8, "lmn_destroy size");
_Static_assert(offsetof(struct lmn_destroy, handle) == 0, "lmn_destroy.handle");
_Static_assert(offsetof(struct lmn_destroy, _pad) == 4, "lmn_destroy._pad");

/* One `setAddress:atIndex:` of [`SetAddresses`]. */
struct lmn_address_binding {
   uint32_t index;
   uint32_t _pad;
   uint64_t address;
};
_Static_assert(sizeof(struct lmn_address_binding) == 16, "lmn_address_binding size");
_Static_assert(offsetof(struct lmn_address_binding, index) == 0, "lmn_address_binding.index");
_Static_assert(offsetof(struct lmn_address_binding, _pad) == 4, "lmn_address_binding._pad");
_Static_assert(offsetof(struct lmn_address_binding, address) == 8, "lmn_address_binding.address");

/* `count` [`AddressBinding`]s follow, applied to argument table `table` in order. */
struct lmn_set_addresses {
   uint32_t table;
   uint32_t count;
};
_Static_assert(sizeof(struct lmn_set_addresses) == 8, "lmn_set_addresses size");
_Static_assert(offsetof(struct lmn_set_addresses, table) == 0, "lmn_set_addresses.table");
_Static_assert(offsetof(struct lmn_set_addresses, count) == 4, "lmn_set_addresses.count");

/* `MTLSharedEvent.signaledValue = value`, from the CPU. */
struct lmn_set_shared_event {
   uint32_t handle;
   uint32_t _pad;
   uint64_t value;
};
_Static_assert(sizeof(struct lmn_set_shared_event) == 16, "lmn_set_shared_event size");
_Static_assert(offsetof(struct lmn_set_shared_event, handle) == 0, "lmn_set_shared_event.handle");
_Static_assert(offsetof(struct lmn_set_shared_event, _pad) == 4, "lmn_set_shared_event._pad");
_Static_assert(offsetof(struct lmn_set_shared_event, value) == 8, "lmn_set_shared_event.value");

/* `getBytes:` (`to_texture` zero) or `replaceRegion:` of a texture region, from or into */
/* arena memory at `data`, on the CPU when the command executes. */
struct lmn_texture_transfer {
   uint32_t texture;
   uint32_t to_texture;
   uint32_t level;
   uint32_t slice;
   uint32_t origin[3];
   uint32_t size[3];
   uint64_t bytes_per_row;
   uint64_t bytes_per_image;
   struct lmn_buf_ref data;
   struct lmn_buf_ref reply;
};
_Static_assert(sizeof(struct lmn_texture_transfer) == 88, "lmn_texture_transfer size");
_Static_assert(offsetof(struct lmn_texture_transfer, texture) == 0, "lmn_texture_transfer.texture");
_Static_assert(offsetof(struct lmn_texture_transfer, to_texture) == 4, "lmn_texture_transfer.to_texture");
_Static_assert(offsetof(struct lmn_texture_transfer, level) == 8, "lmn_texture_transfer.level");
_Static_assert(offsetof(struct lmn_texture_transfer, slice) == 12, "lmn_texture_transfer.slice");
_Static_assert(offsetof(struct lmn_texture_transfer, origin) == 16, "lmn_texture_transfer.origin");
_Static_assert(offsetof(struct lmn_texture_transfer, size) == 28, "lmn_texture_transfer.size");
_Static_assert(offsetof(struct lmn_texture_transfer, bytes_per_row) == 40, "lmn_texture_transfer.bytes_per_row");
_Static_assert(offsetof(struct lmn_texture_transfer, bytes_per_image) == 48, "lmn_texture_transfer.bytes_per_image");
_Static_assert(offsetof(struct lmn_texture_transfer, data) == 56, "lmn_texture_transfer.data");
_Static_assert(offsetof(struct lmn_texture_transfer, reply) == 72, "lmn_texture_transfer.reply");

/* Writes a [`DeviceInfo`] at `dst`. */
struct lmn_query_device {
   struct lmn_buf_ref dst;
   struct lmn_buf_ref reply;
};
_Static_assert(sizeof(struct lmn_query_device) == 32, "lmn_query_device size");
_Static_assert(offsetof(struct lmn_query_device, dst) == 0, "lmn_query_device.dst");
_Static_assert(offsetof(struct lmn_query_device, reply) == 16, "lmn_query_device.reply");

/* `heapTextureSizeAndAlignWithDescriptor:`; reply `values[0]` is the size, `[1]` the */
/* alignment. */
struct lmn_query_texture_size {
   struct lmn_texture_desc desc;
   struct lmn_buf_ref reply;
};
_Static_assert(sizeof(struct lmn_query_texture_size) == 64, "lmn_query_texture_size size");
_Static_assert(offsetof(struct lmn_query_texture_size, desc) == 0, "lmn_query_texture_size.desc");
_Static_assert(offsetof(struct lmn_query_texture_size, reply) == 48, "lmn_query_texture_size.reply");

/* `minimumLinearTextureAlignmentForPixelFormat:`; reply `values[0]`. */
struct lmn_query_linear_alignment {
   uint32_t pixel_format;
   uint32_t _pad;
   struct lmn_buf_ref reply;
};
_Static_assert(sizeof(struct lmn_query_linear_alignment) == 24, "lmn_query_linear_alignment size");
_Static_assert(offsetof(struct lmn_query_linear_alignment, pixel_format) == 0, "lmn_query_linear_alignment.pixel_format");
_Static_assert(offsetof(struct lmn_query_linear_alignment, _pad) == 4, "lmn_query_linear_alignment._pad");
_Static_assert(offsetof(struct lmn_query_linear_alignment, reply) == 8, "lmn_query_linear_alignment.reply");

/* `sampleTimestamps:gpuTimestamp:`; reply `values[0]` is the GPU timestamp, `[1]` the CPU's. */
struct lmn_query_timestamp {
   struct lmn_buf_ref reply;
};
_Static_assert(sizeof(struct lmn_query_timestamp) == 16, "lmn_query_timestamp size");
_Static_assert(offsetof(struct lmn_query_timestamp, reply) == 0, "lmn_query_timestamp.reply");

/* lmn_begin_command_buffer: no payload. */

/* lmn_end_command_buffer: no payload. */

/* lmn_begin_compute: no payload. */

/* One attachment of a [`BeginRenderPass`]. `clear` is RGBA for a color attachment and */
/* `clear[0]` the depth for the depth attachment. */
struct lmn_render_attachment {
   uint32_t texture;
   uint32_t level;
   uint32_t slice;
   uint32_t depth_plane;
   uint32_t load_action;
   uint32_t store_action;
   uint32_t resolve_texture;
   uint32_t resolve_level;
   uint32_t resolve_slice;
   uint32_t resolve_depth_plane;
   uint32_t resolve_filter;
   uint32_t clear_stencil;
   double clear[4];
};
_Static_assert(sizeof(struct lmn_render_attachment) == 80, "lmn_render_attachment size");
_Static_assert(offsetof(struct lmn_render_attachment, texture) == 0, "lmn_render_attachment.texture");
_Static_assert(offsetof(struct lmn_render_attachment, level) == 4, "lmn_render_attachment.level");
_Static_assert(offsetof(struct lmn_render_attachment, slice) == 8, "lmn_render_attachment.slice");
_Static_assert(offsetof(struct lmn_render_attachment, depth_plane) == 12, "lmn_render_attachment.depth_plane");
_Static_assert(offsetof(struct lmn_render_attachment, load_action) == 16, "lmn_render_attachment.load_action");
_Static_assert(offsetof(struct lmn_render_attachment, store_action) == 20, "lmn_render_attachment.store_action");
_Static_assert(offsetof(struct lmn_render_attachment, resolve_texture) == 24, "lmn_render_attachment.resolve_texture");
_Static_assert(offsetof(struct lmn_render_attachment, resolve_level) == 28, "lmn_render_attachment.resolve_level");
_Static_assert(offsetof(struct lmn_render_attachment, resolve_slice) == 32, "lmn_render_attachment.resolve_slice");
_Static_assert(offsetof(struct lmn_render_attachment, resolve_depth_plane) == 36, "lmn_render_attachment.resolve_depth_plane");
_Static_assert(offsetof(struct lmn_render_attachment, resolve_filter) == 40, "lmn_render_attachment.resolve_filter");
_Static_assert(offsetof(struct lmn_render_attachment, clear_stencil) == 44, "lmn_render_attachment.clear_stencil");
_Static_assert(offsetof(struct lmn_render_attachment, clear) == 48, "lmn_render_attachment.clear");

/* Opens an `MTL4RenderCommandEncoder` on an `MTL4RenderPassDescriptor`. */
struct lmn_begin_render_pass {
   uint32_t width;
   uint32_t height;
   uint32_t array_length;
   uint32_t default_raster_sample_count;
   uint32_t sample_position_count;
   uint32_t _pad;
   struct lmn_buf_ref visibility;
   float sample_positions[32];
   struct lmn_render_attachment colors[8];
   struct lmn_render_attachment depth;
   struct lmn_render_attachment stencil;
};
_Static_assert(sizeof(struct lmn_begin_render_pass) == 968, "lmn_begin_render_pass size");
_Static_assert(offsetof(struct lmn_begin_render_pass, width) == 0, "lmn_begin_render_pass.width");
_Static_assert(offsetof(struct lmn_begin_render_pass, height) == 4, "lmn_begin_render_pass.height");
_Static_assert(offsetof(struct lmn_begin_render_pass, array_length) == 8, "lmn_begin_render_pass.array_length");
_Static_assert(offsetof(struct lmn_begin_render_pass, default_raster_sample_count) == 12, "lmn_begin_render_pass.default_raster_sample_count");
_Static_assert(offsetof(struct lmn_begin_render_pass, sample_position_count) == 16, "lmn_begin_render_pass.sample_position_count");
_Static_assert(offsetof(struct lmn_begin_render_pass, _pad) == 20, "lmn_begin_render_pass._pad");
_Static_assert(offsetof(struct lmn_begin_render_pass, visibility) == 24, "lmn_begin_render_pass.visibility");
_Static_assert(offsetof(struct lmn_begin_render_pass, sample_positions) == 40, "lmn_begin_render_pass.sample_positions");
_Static_assert(offsetof(struct lmn_begin_render_pass, colors) == 168, "lmn_begin_render_pass.colors");
_Static_assert(offsetof(struct lmn_begin_render_pass, depth) == 808, "lmn_begin_render_pass.depth");
_Static_assert(offsetof(struct lmn_begin_render_pass, stencil) == 888, "lmn_begin_render_pass.stencil");

/* lmn_end_encoding: no payload. */

/* A barrier of kind `kind` (one of `barrier_kind`), with `MTLStages` masks. */
struct lmn_barrier {
   uint32_t kind;
   uint32_t after;
   uint32_t before;
   uint32_t visibility;
};
_Static_assert(sizeof(struct lmn_barrier) == 16, "lmn_barrier size");
_Static_assert(offsetof(struct lmn_barrier, kind) == 0, "lmn_barrier.kind");
_Static_assert(offsetof(struct lmn_barrier, after) == 4, "lmn_barrier.after");
_Static_assert(offsetof(struct lmn_barrier, before) == 8, "lmn_barrier.before");
_Static_assert(offsetof(struct lmn_barrier, visibility) == 12, "lmn_barrier.visibility");

/* `updateFence:afterEncoderStages:` (`wait` zero) or `waitForFence:beforeEncoderStages:`. */
struct lmn_fence_op {
   uint32_t fence;
   uint32_t stages;
   uint32_t wait;
   uint32_t _pad;
};
_Static_assert(sizeof(struct lmn_fence_op) == 16, "lmn_fence_op size");
_Static_assert(offsetof(struct lmn_fence_op, fence) == 0, "lmn_fence_op.fence");
_Static_assert(offsetof(struct lmn_fence_op, stages) == 4, "lmn_fence_op.stages");
_Static_assert(offsetof(struct lmn_fence_op, wait) == 8, "lmn_fence_op.wait");
_Static_assert(offsetof(struct lmn_fence_op, _pad) == 12, "lmn_fence_op._pad");

/* `writeTimestampWithGranularity:intoHeap:atIndex:` on the open compute encoder (`stage` */
/* zero), or `writeTimestampWithGranularity:afterStage:intoHeap:atIndex:` with the one */
/* `MTLRenderStages` bit `stage` on the open render encoder. `granularity` is an */
/* `MTL4TimestampGranularity`. */
struct lmn_write_timestamp {
   uint32_t heap;
   uint32_t index;
   uint32_t stage;
   uint32_t granularity;
};
_Static_assert(sizeof(struct lmn_write_timestamp) == 16, "lmn_write_timestamp size");
_Static_assert(offsetof(struct lmn_write_timestamp, heap) == 0, "lmn_write_timestamp.heap");
_Static_assert(offsetof(struct lmn_write_timestamp, index) == 4, "lmn_write_timestamp.index");
_Static_assert(offsetof(struct lmn_write_timestamp, stage) == 8, "lmn_write_timestamp.stage");
_Static_assert(offsetof(struct lmn_write_timestamp, granularity) == 12, "lmn_write_timestamp.granularity");

/* `resolveCounterHeap:withRange:intoBuffer:waitFence:updateFence:` on the open command */
/* buffer, with no encoder open: entries `first..first + count` of `heap`, as 8-byte */
/* `MTL4TimestampHeapEntry`s, to GPU address `dst`. The fences (`0` for none) are the only */
/* ordering a resolve has: no barrier reaches it, so it waits on a fence the encoders that */
/* wrote the entries update, and updates one for whatever reads `dst` next. */
struct lmn_resolve_counter_heap {
   uint32_t heap;
   uint32_t first;
   uint32_t count;
   uint32_t wait_fence;
   uint32_t update_fence;
   uint32_t _pad;
   uint64_t dst;
};
_Static_assert(sizeof(struct lmn_resolve_counter_heap) == 32, "lmn_resolve_counter_heap size");
_Static_assert(offsetof(struct lmn_resolve_counter_heap, heap) == 0, "lmn_resolve_counter_heap.heap");
_Static_assert(offsetof(struct lmn_resolve_counter_heap, first) == 4, "lmn_resolve_counter_heap.first");
_Static_assert(offsetof(struct lmn_resolve_counter_heap, count) == 8, "lmn_resolve_counter_heap.count");
_Static_assert(offsetof(struct lmn_resolve_counter_heap, wait_fence) == 12, "lmn_resolve_counter_heap.wait_fence");
_Static_assert(offsetof(struct lmn_resolve_counter_heap, update_fence) == 16, "lmn_resolve_counter_heap.update_fence");
_Static_assert(offsetof(struct lmn_resolve_counter_heap, _pad) == 20, "lmn_resolve_counter_heap._pad");
_Static_assert(offsetof(struct lmn_resolve_counter_heap, dst) == 24, "lmn_resolve_counter_heap.dst");

/* `setComputePipelineState:`. */
struct lmn_set_compute_pipeline {
   uint32_t handle;
   uint32_t _pad;
};
_Static_assert(sizeof(struct lmn_set_compute_pipeline) == 8, "lmn_set_compute_pipeline size");
_Static_assert(offsetof(struct lmn_set_compute_pipeline, handle) == 0, "lmn_set_compute_pipeline.handle");
_Static_assert(offsetof(struct lmn_set_compute_pipeline, _pad) == 4, "lmn_set_compute_pipeline._pad");

/* `setArgumentTable:` on a compute encoder, or `setArgumentTable:atStages:` with */
/* `MTLRenderStages` `stages` on a render encoder. */
struct lmn_set_argument_table {
   uint32_t table;
   uint32_t stages;
};
_Static_assert(sizeof(struct lmn_set_argument_table) == 8, "lmn_set_argument_table size");
_Static_assert(offsetof(struct lmn_set_argument_table, table) == 0, "lmn_set_argument_table.table");
_Static_assert(offsetof(struct lmn_set_argument_table, stages) == 4, "lmn_set_argument_table.stages");

/* `dispatchThreads:threadsPerThreadgroup:`. */
struct lmn_dispatch_threads {
   uint32_t grid[3];
   uint32_t threads_per_group[3];
};
_Static_assert(sizeof(struct lmn_dispatch_threads) == 24, "lmn_dispatch_threads size");
_Static_assert(offsetof(struct lmn_dispatch_threads, grid) == 0, "lmn_dispatch_threads.grid");
_Static_assert(offsetof(struct lmn_dispatch_threads, threads_per_group) == 12, "lmn_dispatch_threads.threads_per_group");

/* `dispatchThreadgroups:threadsPerThreadgroup:`. */
struct lmn_dispatch_threadgroups {
   uint32_t groups[3];
   uint32_t threads_per_group[3];
};
_Static_assert(sizeof(struct lmn_dispatch_threadgroups) == 24, "lmn_dispatch_threadgroups size");
_Static_assert(offsetof(struct lmn_dispatch_threadgroups, groups) == 0, "lmn_dispatch_threadgroups.groups");
_Static_assert(offsetof(struct lmn_dispatch_threadgroups, threads_per_group) == 12, "lmn_dispatch_threadgroups.threads_per_group");

/* `dispatchThreadgroupsWithIndirectBuffer:threadsPerThreadgroup:`. */
struct lmn_dispatch_indirect {
   uint32_t threads_per_group[3];
   uint32_t _pad;
   uint64_t address;
};
_Static_assert(sizeof(struct lmn_dispatch_indirect) == 24, "lmn_dispatch_indirect size");
_Static_assert(offsetof(struct lmn_dispatch_indirect, threads_per_group) == 0, "lmn_dispatch_indirect.threads_per_group");
_Static_assert(offsetof(struct lmn_dispatch_indirect, _pad) == 12, "lmn_dispatch_indirect._pad");
_Static_assert(offsetof(struct lmn_dispatch_indirect, address) == 16, "lmn_dispatch_indirect.address");

/* `copyFromBuffer:sourceOffset:toBuffer:destinationOffset:size:`. */
struct lmn_copy_buffer {
   struct lmn_buf_ref src;
   struct lmn_buf_ref dst;
   uint64_t length;
};
_Static_assert(sizeof(struct lmn_copy_buffer) == 40, "lmn_copy_buffer size");
_Static_assert(offsetof(struct lmn_copy_buffer, src) == 0, "lmn_copy_buffer.src");
_Static_assert(offsetof(struct lmn_copy_buffer, dst) == 16, "lmn_copy_buffer.dst");
_Static_assert(offsetof(struct lmn_copy_buffer, length) == 32, "lmn_copy_buffer.length");

/* Copies between a buffer and a texture region, in the direction `to_texture` says, with */
/* `MTLBlitOption` `options`. */
struct lmn_copy_buffer_texture {
   uint32_t texture;
   uint32_t to_texture;
   uint32_t level;
   uint32_t slice;
   uint32_t options;
   uint32_t origin[3];
   uint32_t size[3];
   uint32_t _pad;
   struct lmn_buf_ref buffer;
   uint64_t bytes_per_row;
   uint64_t bytes_per_image;
};
_Static_assert(sizeof(struct lmn_copy_buffer_texture) == 80, "lmn_copy_buffer_texture size");
_Static_assert(offsetof(struct lmn_copy_buffer_texture, texture) == 0, "lmn_copy_buffer_texture.texture");
_Static_assert(offsetof(struct lmn_copy_buffer_texture, to_texture) == 4, "lmn_copy_buffer_texture.to_texture");
_Static_assert(offsetof(struct lmn_copy_buffer_texture, level) == 8, "lmn_copy_buffer_texture.level");
_Static_assert(offsetof(struct lmn_copy_buffer_texture, slice) == 12, "lmn_copy_buffer_texture.slice");
_Static_assert(offsetof(struct lmn_copy_buffer_texture, options) == 16, "lmn_copy_buffer_texture.options");
_Static_assert(offsetof(struct lmn_copy_buffer_texture, origin) == 20, "lmn_copy_buffer_texture.origin");
_Static_assert(offsetof(struct lmn_copy_buffer_texture, size) == 32, "lmn_copy_buffer_texture.size");
_Static_assert(offsetof(struct lmn_copy_buffer_texture, _pad) == 44, "lmn_copy_buffer_texture._pad");
_Static_assert(offsetof(struct lmn_copy_buffer_texture, buffer) == 48, "lmn_copy_buffer_texture.buffer");
_Static_assert(offsetof(struct lmn_copy_buffer_texture, bytes_per_row) == 64, "lmn_copy_buffer_texture.bytes_per_row");
_Static_assert(offsetof(struct lmn_copy_buffer_texture, bytes_per_image) == 72, "lmn_copy_buffer_texture.bytes_per_image");

/* `copyFromTexture:sourceSlice:sourceLevel:sourceOrigin:sourceSize:toTexture:…`. */
struct lmn_copy_texture {
   uint32_t src;
   uint32_t src_slice;
   uint32_t src_level;
   uint32_t dst;
   uint32_t dst_slice;
   uint32_t dst_level;
   uint32_t src_origin[3];
   uint32_t src_size[3];
   uint32_t dst_origin[3];
   uint32_t _pad;
};
_Static_assert(sizeof(struct lmn_copy_texture) == 64, "lmn_copy_texture size");
_Static_assert(offsetof(struct lmn_copy_texture, src) == 0, "lmn_copy_texture.src");
_Static_assert(offsetof(struct lmn_copy_texture, src_slice) == 4, "lmn_copy_texture.src_slice");
_Static_assert(offsetof(struct lmn_copy_texture, src_level) == 8, "lmn_copy_texture.src_level");
_Static_assert(offsetof(struct lmn_copy_texture, dst) == 12, "lmn_copy_texture.dst");
_Static_assert(offsetof(struct lmn_copy_texture, dst_slice) == 16, "lmn_copy_texture.dst_slice");
_Static_assert(offsetof(struct lmn_copy_texture, dst_level) == 20, "lmn_copy_texture.dst_level");
_Static_assert(offsetof(struct lmn_copy_texture, src_origin) == 24, "lmn_copy_texture.src_origin");
_Static_assert(offsetof(struct lmn_copy_texture, src_size) == 36, "lmn_copy_texture.src_size");
_Static_assert(offsetof(struct lmn_copy_texture, dst_origin) == 48, "lmn_copy_texture.dst_origin");
_Static_assert(offsetof(struct lmn_copy_texture, _pad) == 60, "lmn_copy_texture._pad");

/* `fillBuffer:range:value:` over `length` bytes from `buffer`. */
struct lmn_fill_buffer {
   struct lmn_buf_ref buffer;
   uint64_t length;
   uint32_t value;
   uint32_t _pad;
};
_Static_assert(sizeof(struct lmn_fill_buffer) == 32, "lmn_fill_buffer size");
_Static_assert(offsetof(struct lmn_fill_buffer, buffer) == 0, "lmn_fill_buffer.buffer");
_Static_assert(offsetof(struct lmn_fill_buffer, length) == 16, "lmn_fill_buffer.length");
_Static_assert(offsetof(struct lmn_fill_buffer, value) == 24, "lmn_fill_buffer.value");
_Static_assert(offsetof(struct lmn_fill_buffer, _pad) == 28, "lmn_fill_buffer._pad");

/* `setRenderPipelineState:`. */
struct lmn_set_render_pipeline {
   uint32_t handle;
   uint32_t _pad;
};
_Static_assert(sizeof(struct lmn_set_render_pipeline) == 8, "lmn_set_render_pipeline size");
_Static_assert(offsetof(struct lmn_set_render_pipeline, handle) == 0, "lmn_set_render_pipeline.handle");
_Static_assert(offsetof(struct lmn_set_render_pipeline, _pad) == 4, "lmn_set_render_pipeline._pad");

/* `setViewports:count:`; `count` [`Viewport`]s follow. */
struct lmn_set_viewports {
   uint32_t count;
   uint32_t _pad;
};
_Static_assert(sizeof(struct lmn_set_viewports) == 8, "lmn_set_viewports size");
_Static_assert(offsetof(struct lmn_set_viewports, count) == 0, "lmn_set_viewports.count");
_Static_assert(offsetof(struct lmn_set_viewports, _pad) == 4, "lmn_set_viewports._pad");

/* An `MTLViewport`. */
struct lmn_viewport {
   double x;
   double y;
   double width;
   double height;
   double znear;
   double zfar;
};
_Static_assert(sizeof(struct lmn_viewport) == 48, "lmn_viewport size");
_Static_assert(offsetof(struct lmn_viewport, x) == 0, "lmn_viewport.x");
_Static_assert(offsetof(struct lmn_viewport, y) == 8, "lmn_viewport.y");
_Static_assert(offsetof(struct lmn_viewport, width) == 16, "lmn_viewport.width");
_Static_assert(offsetof(struct lmn_viewport, height) == 24, "lmn_viewport.height");
_Static_assert(offsetof(struct lmn_viewport, znear) == 32, "lmn_viewport.znear");
_Static_assert(offsetof(struct lmn_viewport, zfar) == 40, "lmn_viewport.zfar");

/* `setScissorRects:count:`; `count` [`Scissor`]s follow. */
struct lmn_set_scissors {
   uint32_t count;
   uint32_t _pad;
};
_Static_assert(sizeof(struct lmn_set_scissors) == 8, "lmn_set_scissors size");
_Static_assert(offsetof(struct lmn_set_scissors, count) == 0, "lmn_set_scissors.count");
_Static_assert(offsetof(struct lmn_set_scissors, _pad) == 4, "lmn_set_scissors._pad");

/* An `MTLScissorRect`. */
struct lmn_scissor {
   uint32_t x;
   uint32_t y;
   uint32_t width;
   uint32_t height;
};
_Static_assert(sizeof(struct lmn_scissor) == 16, "lmn_scissor size");
_Static_assert(offsetof(struct lmn_scissor, x) == 0, "lmn_scissor.x");
_Static_assert(offsetof(struct lmn_scissor, y) == 4, "lmn_scissor.y");
_Static_assert(offsetof(struct lmn_scissor, width) == 8, "lmn_scissor.width");
_Static_assert(offsetof(struct lmn_scissor, height) == 12, "lmn_scissor.height");

/* `setDepthStencilState:`. */
struct lmn_set_depth_stencil {
   uint32_t handle;
   uint32_t _pad;
};
_Static_assert(sizeof(struct lmn_set_depth_stencil) == 8, "lmn_set_depth_stencil size");
_Static_assert(offsetof(struct lmn_set_depth_stencil, handle) == 0, "lmn_set_depth_stencil.handle");
_Static_assert(offsetof(struct lmn_set_depth_stencil, _pad) == 4, "lmn_set_depth_stencil._pad");

/* One fixed-function setter of the render encoder (`kind`, one of `render_state`). */
struct lmn_render_state {
   uint32_t kind;
   uint32_t a;
   uint32_t b;
   uint32_t _pad;
   uint64_t offset;
   float f[4];
};
_Static_assert(sizeof(struct lmn_render_state) == 40, "lmn_render_state size");
_Static_assert(offsetof(struct lmn_render_state, kind) == 0, "lmn_render_state.kind");
_Static_assert(offsetof(struct lmn_render_state, a) == 4, "lmn_render_state.a");
_Static_assert(offsetof(struct lmn_render_state, b) == 8, "lmn_render_state.b");
_Static_assert(offsetof(struct lmn_render_state, _pad) == 12, "lmn_render_state._pad");
_Static_assert(offsetof(struct lmn_render_state, offset) == 16, "lmn_render_state.offset");
_Static_assert(offsetof(struct lmn_render_state, f) == 24, "lmn_render_state.f");

/* `setVertexAmplificationCount:viewMappings:`; `count` `u32` render-target array index */
/* offsets follow. */
struct lmn_set_amplification {
   uint32_t count;
   uint32_t _pad;
};
_Static_assert(sizeof(struct lmn_set_amplification) == 8, "lmn_set_amplification size");
_Static_assert(offsetof(struct lmn_set_amplification, count) == 0, "lmn_set_amplification.count");
_Static_assert(offsetof(struct lmn_set_amplification, _pad) == 4, "lmn_set_amplification._pad");

/* `drawPrimitives:vertexStart:vertexCount:instanceCount:baseInstance:`. */
struct lmn_draw {
   uint32_t primitive;
   uint32_t vertex_start;
   uint32_t vertex_count;
   uint32_t instance_count;
   uint32_t base_instance;
   uint32_t _pad;
};
_Static_assert(sizeof(struct lmn_draw) == 24, "lmn_draw size");
_Static_assert(offsetof(struct lmn_draw, primitive) == 0, "lmn_draw.primitive");
_Static_assert(offsetof(struct lmn_draw, vertex_start) == 4, "lmn_draw.vertex_start");
_Static_assert(offsetof(struct lmn_draw, vertex_count) == 8, "lmn_draw.vertex_count");
_Static_assert(offsetof(struct lmn_draw, instance_count) == 12, "lmn_draw.instance_count");
_Static_assert(offsetof(struct lmn_draw, base_instance) == 16, "lmn_draw.base_instance");
_Static_assert(offsetof(struct lmn_draw, _pad) == 20, "lmn_draw._pad");

/* `drawIndexedPrimitives:indexCount:indexType:indexBuffer:indexBufferLength:…`. */
struct lmn_draw_indexed {
   uint32_t primitive;
   uint32_t index_count;
   uint32_t index_type;
   uint32_t instance_count;
   int32_t base_vertex;
   uint32_t base_instance;
   uint64_t index_address;
   uint64_t index_length;
};
_Static_assert(sizeof(struct lmn_draw_indexed) == 40, "lmn_draw_indexed size");
_Static_assert(offsetof(struct lmn_draw_indexed, primitive) == 0, "lmn_draw_indexed.primitive");
_Static_assert(offsetof(struct lmn_draw_indexed, index_count) == 4, "lmn_draw_indexed.index_count");
_Static_assert(offsetof(struct lmn_draw_indexed, index_type) == 8, "lmn_draw_indexed.index_type");
_Static_assert(offsetof(struct lmn_draw_indexed, instance_count) == 12, "lmn_draw_indexed.instance_count");
_Static_assert(offsetof(struct lmn_draw_indexed, base_vertex) == 16, "lmn_draw_indexed.base_vertex");
_Static_assert(offsetof(struct lmn_draw_indexed, base_instance) == 20, "lmn_draw_indexed.base_instance");
_Static_assert(offsetof(struct lmn_draw_indexed, index_address) == 24, "lmn_draw_indexed.index_address");
_Static_assert(offsetof(struct lmn_draw_indexed, index_length) == 32, "lmn_draw_indexed.index_length");

/* `drawPrimitives:indirectBuffer:`, or with `indexed` non-zero */
/* `drawIndexedPrimitives:indexType:indexBuffer:indexBufferLength:indirectBuffer:`. */
struct lmn_draw_indirect {
   uint32_t primitive;
   uint32_t indexed;
   uint32_t index_type;
   uint32_t _pad;
   uint64_t index_address;
   uint64_t index_length;
   uint64_t indirect;
};
_Static_assert(sizeof(struct lmn_draw_indirect) == 40, "lmn_draw_indirect size");
_Static_assert(offsetof(struct lmn_draw_indirect, primitive) == 0, "lmn_draw_indirect.primitive");
_Static_assert(offsetof(struct lmn_draw_indirect, indexed) == 4, "lmn_draw_indirect.indexed");
_Static_assert(offsetof(struct lmn_draw_indirect, index_type) == 8, "lmn_draw_indirect.index_type");
_Static_assert(offsetof(struct lmn_draw_indirect, _pad) == 12, "lmn_draw_indirect._pad");
_Static_assert(offsetof(struct lmn_draw_indirect, index_address) == 16, "lmn_draw_indirect.index_address");
_Static_assert(offsetof(struct lmn_draw_indirect, index_length) == 24, "lmn_draw_indirect.index_length");
_Static_assert(offsetof(struct lmn_draw_indirect, indirect) == 32, "lmn_draw_indirect.indirect");

/* Submits the command buffers ended since the last commit to `queue` (`0`, the context's */
/* own), in one `commit:count:options:`. The host fills `feedback` (arena `0` for none) and */
/* then publishes `seqno` as the queue's completed value. Seqnos must increase across the */
/* context. A commit with nothing to submit commits an empty command buffer. */
struct lmn_commit {
   uint64_t seqno;
   uint32_t queue;
   uint32_t _pad;
   struct lmn_buf_ref feedback;
};
_Static_assert(sizeof(struct lmn_commit) == 32, "lmn_commit size");
_Static_assert(offsetof(struct lmn_commit, seqno) == 0, "lmn_commit.seqno");
_Static_assert(offsetof(struct lmn_commit, queue) == 8, "lmn_commit.queue");
_Static_assert(offsetof(struct lmn_commit, _pad) == 12, "lmn_commit._pad");
_Static_assert(offsetof(struct lmn_commit, feedback) == 16, "lmn_commit.feedback");

/* `signalEvent:value:` (`wait` zero) or `waitForEvent:value:` on `queue`, between commits: */
/* no command buffer may be ended and not yet committed. A signal of a shared event also */
/* updates its mirror once the value is reached. */
struct lmn_queue_event {
   uint32_t event;
   uint32_t wait;
   uint64_t value;
   uint32_t queue;
   uint32_t _pad;
};
_Static_assert(sizeof(struct lmn_queue_event) == 24, "lmn_queue_event size");
_Static_assert(offsetof(struct lmn_queue_event, event) == 0, "lmn_queue_event.event");
_Static_assert(offsetof(struct lmn_queue_event, wait) == 4, "lmn_queue_event.wait");
_Static_assert(offsetof(struct lmn_queue_event, value) == 8, "lmn_queue_event.value");
_Static_assert(offsetof(struct lmn_queue_event, queue) == 16, "lmn_queue_event.queue");
_Static_assert(offsetof(struct lmn_queue_event, _pad) == 20, "lmn_queue_event._pad");

#endif /* LIMINA_GPU_WIRE_H */
