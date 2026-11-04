# CPU request preparation (owned copies; fail closed on anchor changes).
# 1. Mel filterbank: each triangular band is nonzero over a few FFT bins. Skip
#    the 4-bin chunks whose filter weights are all zero. A skipped chunk would
#    add exactly +0.0 (non-negative power times 0), so the accumulation order
#    and every rounded partial of the pinned loop are preserved bit for bit.
file(READ "${CMAKE_CURRENT_BINARY_DIR}/heimdall-audio.cpp" audio_source)
set(mel_setup_anchor "    const auto & filters = cache.filters;\n")
set(mel_loop_anchor "            for (k = 0; k < n_fft_bins - 3; k += 4) {\n                size_t idx = size_t(j) * size_t(n_fft_bins) + size_t(k);")
foreach(anchor IN ITEMS "${mel_setup_anchor}" "${mel_loop_anchor}")
    string(FIND "${audio_source}" "${anchor}" found)
    if(found EQUAL -1)
        message(FATAL_ERROR "Pinned d1 mel anchor changed: ${anchor}")
    endif()
endforeach()
string(REPLACE "${mel_setup_anchor}" "${mel_setup_anchor}    // [first, last) 4-bin chunk of each band that has a nonzero weight\n    std::vector<std::pair<int, int>> heimdall_band(out.n_mel, {0, 0});\n    for (int64_t j = 0; j < out.n_mel; j++) {\n        int first = -1, last = -1;\n        for (int64_t k = 0; k + 3 < params.n_fft_bins; k += 4) {\n            const float * f = filters.data.data() + size_t(j) * size_t(params.n_fft_bins) + size_t(k);\n            if (f[0] != 0.0f || f[1] != 0.0f || f[2] != 0.0f || f[3] != 0.0f) { if (first < 0) first = (int) k; last = (int) k + 4; }\n        }\n        heimdall_band[j] = first < 0 ? std::make_pair(0, 0) : std::make_pair(first, last);\n    }\n" audio_source "${audio_source}")
string(REPLACE "${mel_loop_anchor}" "            for (k = heimdall_band[j].first; k < heimdall_band[j].second; k += 4) {\n                size_t idx = size_t(j) * size_t(n_fft_bins) + size_t(k);" audio_source "${audio_source}")
# The remainder loop starts where the pinned main loop ends.
set(mel_tail_anchor "            // handle n_fft remainder\n            for (; k < n_fft_bins; k++) {")
string(FIND "${audio_source}" "${mel_tail_anchor}" found)
if(found EQUAL -1)
    message(FATAL_ERROR "Pinned d1 mel remainder anchor changed")
endif()
string(REPLACE "${mel_tail_anchor}" "            // handle n_fft remainder\n            for (k = (int) ((n_fft_bins - 3 + 3) / 4 * 4); k < n_fft_bins; k++) {" audio_source "${audio_source}")
file(WRITE "${CMAKE_CURRENT_BINARY_DIR}/heimdall-audio.cpp" "${audio_source}")

# 2. Media ids: the same SHA-256 digest/hex string through CommonCrypto's
#    hardware-backed implementation instead of the portable byte loop.
file(READ "${LLAMA_SOURCE}/vendor/hash/hash.cpp" hash_source)
set(hash_anchor "    sha256_hash(digest, (const unsigned char *) data, len);")
string(FIND "${hash_source}" "${hash_anchor}" found)
if(found EQUAL -1)
    message(FATAL_ERROR "Pinned SHA-256 anchor changed")
endif()
string(REPLACE "${hash_anchor}" "    if (len <= 0xffffffffu) CC_SHA256(data, (CC_LONG) len, digest);\n    else sha256_hash(digest, (const unsigned char *) data, len);" hash_source "${hash_source}")
file(WRITE "${CMAKE_CURRENT_BINARY_DIR}/heimdall-hash.cpp" "#include <CommonCrypto/CommonDigest.h>\n${hash_source}")
get_target_property(hash_sources vendor-hash SOURCES)
list(FILTER hash_sources EXCLUDE REGEX "hash.cpp$")
set_property(TARGET vendor-hash PROPERTY SOURCES "${hash_sources};${CMAKE_CURRENT_BINARY_DIR}/heimdall-hash.cpp")
target_include_directories(vendor-hash PRIVATE "${LLAMA_SOURCE}/vendor/hash")
