# MUL_MAT + bias/residual ADD for the direct tensor matmul (direct-mm*.inc).
# Owned copies of the pinned fusion table and device capability probe.
file(READ "${kernel_root}/ggml-metal-fusion.cpp" fusion_source)
file(READ "${CMAKE_CURRENT_SOURCE_DIR}/direct-mm-predicate.inc" direct_predicate)
set(fusion_entry_anchor "    { GGML_METAL_FUSION_MUL_MAT_ADD,    ops_mul_mat_add,            {},     false, ggml_metal_fusion_check_mul_mat_add },")
set(fusion_table_anchor "static const std::vector<ggml_metal_fusion> ggml_metal_fusions = {")
foreach(anchor IN ITEMS "${fusion_entry_anchor}" "${fusion_table_anchor}")
    string(FIND "${fusion_source}" "${anchor}" found)
    if(found EQUAL -1)
        message(FATAL_ERROR "Pinned Metal fusion anchor changed: ${anchor}")
    endif()
endforeach()
set(fusion_check "${direct_predicate}
// MUL_MAT + ADD of a bias row vector or a same-shape residual, applied in the
// direct matmul's store. Claimed only where that kernel runs (other shapes keep
// the pinned few-row MMA residual fusion below).
static bool heimdall_fusion_check_mul_mat_epilogue(
        const ggml_metal_fusion * fusion, const ggml_tensor * const * nodes, const ggml_cgraph * gf,
        const int * node_idxs, int idx, ggml_metal_fusion_mode mode) {
    GGML_UNUSED(fusion); GGML_UNUSED(gf); GGML_UNUSED(node_idxs); GGML_UNUSED(idx);
    const ggml_tensor * mm  = nodes[0];
    const ggml_tensor * add = nodes[1];
    if (add->op != GGML_OP_ADD || (add->src[0] == mm) == (add->src[1] == mm)) return false;
    const ggml_tensor * x = add->src[0] == mm ? add->src[1] : add->src[0];
    if (x->type != GGML_TYPE_F32 || add->type != GGML_TYPE_F32 || !ggml_is_contiguous(x) || !ggml_is_contiguous(add)) return false;
    const bool bias = x->ne[0] == mm->ne[0] && ggml_nelements(x) == mm->ne[0];
    if (!(bias || ggml_are_same_shape(x, mm)) || !heimdall_mul_mm_direct_ok(mm)) return false;
    if (mode == GGML_METAL_FUSION_STRUCTURAL) return true;
    return !ggml_metal_fusion_overlap(add, mm->src[0]) && !ggml_metal_fusion_overlap(add, mm->src[1]) &&
        (add->data == x->data || !ggml_metal_fusion_overlap(add, x));
}

${fusion_table_anchor}")
string(REPLACE "${fusion_table_anchor}" "${fusion_check}" fusion_source "${fusion_source}")
string(REPLACE "${fusion_entry_anchor}" "    { GGML_METAL_FUSION_MUL_MAT_ADD,    ops_mul_mat_add,            {},     false, heimdall_fusion_check_mul_mat_epilogue },\n${fusion_entry_anchor}" fusion_source "${fusion_source}")
file(WRITE "${CMAKE_CURRENT_BINARY_DIR}/heimdall-metal-fusion.cpp" "${fusion_source}")
get_target_property(metal_sources ggml-metal SOURCES)
list(FILTER metal_sources EXCLUDE REGEX "ggml-metal-fusion.cpp$")
set_property(TARGET ggml-metal PROPERTY SOURCES "${metal_sources};${CMAKE_CURRENT_BINARY_DIR}/heimdall-metal-fusion.cpp")

# Publish the device capability the predicate needs (set once at device init).
file(READ "${residency_device}" residency_source)
set(caps_anchor "                dev->props.max_buffer_size            = dev->mtl_device.maxBufferLength;")
string(FIND "${residency_source}" "${caps_anchor}" found)
if(found EQUAL -1)
    message(FATAL_ERROR "Pinned Metal device props anchor changed")
endif()
string(REPLACE "${caps_anchor}" "                heimdall_metal_direct_caps = (dev->props.has_tensor && dev->props.has_simdgroup_mm ? 1 : 0) | (dev->props.has_bfloat ? 2 : 0);\n${caps_anchor}" residency_source "${residency_source}")
string(PREPEND residency_source "int heimdall_metal_direct_caps = 0;\n")
file(WRITE "${residency_device}" "${residency_source}")
