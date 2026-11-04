# Inject only owned copies; fail on upstream changes instead of patching vendors.
file(READ "${CMAKE_CURRENT_BINARY_DIR}/heimdall-lfm2.cpp" lfm_source)
file(READ "${CMAKE_CURRENT_SOURCE_DIR}/centered-conv.inc" centered_helper)
string(PREPEND lfm_source "#include \"ggml.h\"\n${centered_helper}\n")
set(centered_anchor "            cur = build_lora_mm(layer.shortconv.out_proj, ggml_mul(ctx0, c, y));")
string(FIND "${lfm_source}" "${centered_anchor}" centered_found)
if(centered_found EQUAL -1)
    message(FATAL_ERROR "Pinned d1 centred convolution anchor changed")
endif()
string(REPLACE "${centered_anchor}" "            auto * gated = heimdall_centered(ctx0, bcx, layer.shortconv.conv, conv_left, conv_right);\n            cur = build_lora_mm(layer.shortconv.out_proj, gated);" lfm_source "${lfm_source}")

file(WRITE "${CMAKE_CURRENT_BINARY_DIR}/heimdall-lfm2.cpp" "${lfm_source}")

# Include the custom shader in the existing owned embedded library.
file(READ "${CMAKE_CURRENT_SOURCE_DIR}/centered.metal" centered_metal)
string(APPEND mm_embed "\n${centered_metal}")
file(WRITE "${CMAKE_CURRENT_BINARY_DIR}/heimdall-mul-mm.metal" "${mm_embed}")

# A named CUSTOM node is accepted only with the exact four FP32 operand shapes.
set(centered_guard "    if (op->op == GGML_OP_CUSTOM && strcmp(op->name, \"heimdall.centered.fp32\") == 0) {\n        return op->type == GGML_TYPE_F32 && ggml_is_contiguous(op) &&\n               op->src[0] && op->src[1] && op->src[2] && op->src[3] && op->src[4] && op->src[5] &&\n               op->src[0]->type == GGML_TYPE_F32 && ggml_is_contiguous(op->src[0]) &&\n               op->src[0]->ne[0] == 3*op->ne[0] && op->src[0]->ne[1] == op->ne[1] &&\n               op->src[1]->type == GGML_TYPE_F32 && ggml_is_contiguous(op->src[1]) &&\n               op->src[1]->ne[0] == 3 && op->src[1]->ne[1] == op->ne[0] &&\n               op->src[2]->type == GGML_TYPE_F32 && ggml_is_contiguous(op->src[2]) && ggml_nelements(op->src[2]) == op->ne[1] &&\n               op->src[3]->type == GGML_TYPE_F32 && ggml_is_contiguous(op->src[3]) && ggml_nelements(op->src[3]) == op->ne[1] &&\n               op->src[4]->type == GGML_TYPE_I32 && ggml_is_contiguous(op->src[4]) && ggml_nelements(op->src[4]) == op->ne[1] &&\n               op->src[5]->type == GGML_TYPE_I32 && ggml_is_contiguous(op->src[5]) && ggml_nelements(op->src[5]) == op->ne[1];\n    }\n")
file(READ "${residency_device}" residency_source)
string(REPLACE "bool ggml_metal_device_supports_op(ggml_metal_device_t dev, const struct ggml_tensor * op) {" "bool ggml_metal_device_supports_op(ggml_metal_device_t dev, const struct ggml_tensor * op) {\n${centered_guard}" residency_source "${residency_source}")
file(WRITE "${residency_device}" "${residency_source}")

file(READ "${kernel_root}/ggml-metal-ops.cpp" metal_ops)
set(centered_dispatch "    if (node->op == GGML_OP_CUSTOM && strcmp(node->name, \"heimdall.centered.fp32\") == 0) {\n        auto pipeline = ggml_metal_library_get_pipeline(ctx->lib, \"kernel_heimdall_centered\");\n        if (!pipeline.pipeline) pipeline = ggml_metal_library_compile_pipeline(ctx->lib, \"kernel_heimdall_centered\", \"kernel_heimdall_centered\", nullptr);\n        uint64_t args[] = {(uint64_t) node->ne[0], (uint64_t) node->ne[1], node->src[0]->nb[1]/4, 0};\n        ggml_metal_encoder_set_pipeline(ctx->enc, pipeline);\n        ggml_metal_encoder_set_bytes(ctx->enc, args, sizeof(args), 0);\n        for (int j=0; j<4; ++j) ggml_metal_encoder_set_buffer(ctx->enc, ggml_metal_get_buffer_id(node->src[j]), j+1);\n        ggml_metal_encoder_set_buffer(ctx->enc, ggml_metal_get_buffer_id(node), 5);\n        ggml_metal_encoder_set_buffer(ctx->enc, ggml_metal_get_buffer_id(node->src[4]), 6);\n        ggml_metal_encoder_set_buffer(ctx->enc, ggml_metal_get_buffer_id(node->src[5]), 7);\n        ggml_metal_encoder_dispatch_threadgroups(ctx->enc, (ggml_nelements(node)+255)/256, 1, 1, 256, 1, 1);\n        return 1;\n    }\n")
# Dispatch after concurrency tracking has registered all inputs and inserted barriers.
set(dispatch_anchor "    switch (node->op) {\n        case GGML_OP_CONCAT:")
string(FIND "${metal_ops}" "${dispatch_anchor}" dispatch_found)
if(dispatch_found EQUAL -1)
    message(FATAL_ERROR "Pinned Metal dispatch anchor changed")
endif()
string(REPLACE "        return 1;" "        if (!ggml_metal_op_concurrency_add(ctx, node)) ggml_metal_op_concurrency_reset(ctx);\n        return 1;" centered_dispatch "${centered_dispatch}")
string(REPLACE "${dispatch_anchor}" "${centered_dispatch}\n${dispatch_anchor}" metal_ops "${metal_ops}")
file(WRITE "${CMAKE_CURRENT_BINARY_DIR}/heimdall-metal-ops.cpp" "${metal_ops}")
get_target_property(metal_sources ggml-metal SOURCES)
list(FILTER metal_sources EXCLUDE REGEX "ggml-metal-ops.cpp$")
set_property(TARGET ggml-metal PROPERTY SOURCES "${metal_sources};${CMAKE_CURRENT_BINARY_DIR}/heimdall-metal-ops.cpp")
