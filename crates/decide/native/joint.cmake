# The questions of one request in one forward, shared media evaluated once
# (joint-decision.inc, decision-input.inc). Owned copy of the pinned LFM2
# decision graph region; it supersedes the centred-conv anchor in that region.
file(READ "${CMAKE_CURRENT_BINARY_DIR}/heimdall-lfm2.cpp" lfm_source)
file(READ "${CMAKE_CURRENT_SOURCE_DIR}/joint-decision.inc" joint_source)
set(joint_begin "// non-causal within a sequence, the media never reads the text, so it is a function of the media alone")
set(joint_end "// same as llama_model_modern_bert::graph::build_decision_head(), with the head counts of the head layers")
set(joint_selected "        selected = ggml_new_tensor_1d(ctx0, GGML_TYPE_I32, n_tokens);")
set(joint_head "        res->add_input(std::make_unique<heimdall_head_input>(selected));")
foreach(anchor IN ITEMS "${joint_begin}" "${joint_end}" "${joint_selected}" "${joint_head}")
    string(FIND "${lfm_source}" "${anchor}" found)
    if(found EQUAL -1)
        message(FATAL_ERROR "Pinned d1 joint decision anchor changed: ${anchor}")
    endif()
endforeach()
string(FIND "${lfm_source}" "${joint_begin}" joint_begin_at)
string(FIND "${lfm_source}" "${joint_end}" joint_end_at)
string(SUBSTRING "${lfm_source}" 0 ${joint_begin_at} before_joint)
string(SUBSTRING "${lfm_source}" ${joint_end_at} -1 after_joint)
string(REPLACE "${joint_selected}" "        selected = ggml_new_tensor_1d(ctx0, GGML_TYPE_I32, inp->ne[1]);" after_joint "${after_joint}")
string(REPLACE "${joint_head}" "        res->add_input(std::make_unique<heimdall_head_input>(selected, n_embd));" after_joint "${after_joint}")
set(joint_head_attn "            cur = build_attn(inp_attn,\n                        layer.wo, layer.wo_b, layer.wo_s,\n                        Qcur, Kcur, Vcur, nullptr, nullptr, nullptr, 1.0f/sqrtf(float(n_embd_head)), il);")
string(FIND "${after_joint}" "${joint_head_attn}" found)
if(found EQUAL -1)
    message(FATAL_ERROR "Pinned d1 head attention anchor changed")
endif()
string(REPLACE "${joint_head_attn}" "            cur = heimdall_joint\n                ? heimdall_attend(*this, heimdall_joint->head, layer.wo, layer.wo_b, layer.wo_s, Qcur, Kcur, Vcur, 1.0f/sqrtf(float(n_embd_head)), il)\n                : build_attn(inp_attn,\n                        layer.wo, layer.wo_b, layer.wo_s,\n                        Qcur, Kcur, Vcur, nullptr, nullptr, nullptr, 1.0f/sqrtf(float(n_embd_head)), il);" after_joint "${after_joint}")
file(WRITE "${CMAKE_CURRENT_BINARY_DIR}/heimdall-lfm2.cpp" "${before_joint}${joint_source}\n${after_joint}")

# Q/K/V views must follow the actual input tensor after shared-media compaction.
# Keep llm_graph_context::n_tokens immutable (the original ubatch row count).
# All callers with ordinary inputs retain the pinned shape and arithmetic.
file(READ "${LLAMA_SOURCE}/src/llama-graph.cpp" graph_source)
set(qkv_rows_anchor "    const int64_t n_embd_q = n_embd_head_q * n_head_q;")
string(FIND "${graph_source}" "${qkv_rows_anchor}" found)
if(found EQUAL -1)
    message(FATAL_ERROR "Pinned QKV effective-row anchor changed")
endif()
string(REPLACE "${qkv_rows_anchor}" "    const int64_t n_tokens = cur->ne[1];  // effective rows, including compacted media
${qkv_rows_anchor}" graph_source "${graph_source}")
file(WRITE "${CMAKE_CURRENT_BINARY_DIR}/heimdall-llama-graph.cpp" "${graph_source}")
get_target_property(llama_sources llama SOURCES)
list(FILTER llama_sources EXCLUDE REGEX "llama-graph.cpp$")
set_property(TARGET llama PROPERTY SOURCES "${llama_sources};${CMAKE_CURRENT_BINARY_DIR}/heimdall-llama-graph.cpp")
