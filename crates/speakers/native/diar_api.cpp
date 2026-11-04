// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
// Maps the nemo_speech_asr_* C ABI onto the C++ recognizer. Exceptions become status
// codes and thread-local errors; no C++ types cross the boundary.
#include <chrono>
#include <algorithm>
#include <cstddef>
#include <cstring>
#include <memory>
#include <new>
#include <optional>
#include <stdexcept>
#include <string>
#include <vector>

#include "diar/diarizer.h"
#include "nemo_speech/asr.h"
#include "nemo_speech/diar.h"

namespace asr_core = nemo_speech::asr;
struct nemo_speech_diar_model {
    std::shared_ptr<asr_core::Diarizer> impl;
};
// One diarization job: a live stream (impl set) or a finished offline result
// (offline set). Core stream state retains the model resources it needs.
struct nemo_speech_diar_stream {
    std::unique_ptr<asr_core::DiarizationStream> impl;
    std::optional<asr_core::DiarizationResult> offline;
};

namespace {
thread_local std::string g_last_error;
void
set_last_error(const std::string& m) {
    g_last_error = m;
}

// A field is present iff the caller's struct `size` covers it (append-only ABI:
// a caller compiled against an older header sends a smaller size; missing tail
// fields keep their library defaults).
#define HAS_FIELD(ptr, type, member) \
    ((ptr) != nullptr && (ptr)->size >= offsetof(type, member) + sizeof(((type*)nullptr)->member))

// Converts exceptions to status codes and last-error text.
template <class F>
nemo_speech_asr_status
guard(F&& f) {
    try {
        return f();
    }
    catch (const std::bad_alloc&) {
        set_last_error("out of memory");
        return NEMO_SPEECH_ASR_ERROR_OUT_OF_MEMORY;
    }
    catch (const std::invalid_argument& e) {
        set_last_error(e.what());
        return NEMO_SPEECH_ASR_ERROR_INVALID_ARGUMENT;
    }
    catch (const std::exception& e) {
        set_last_error(e.what());
        return NEMO_SPEECH_ASR_ERROR_RUNTIME;
    }
    catch (...) {
        set_last_error("unknown error");
        return NEMO_SPEECH_ASR_ERROR_RUNTIME;
    }
}

} // namespace
extern "C" {
const char*
nemo_speech_asr_last_error(void) {
    return g_last_error.c_str();
}

// Standalone diarization shares ASR status and error handling.

nemo_speech_asr_status
nemo_speech_diar_create(const nemo_speech_diar_model_config* cfg, nemo_speech_diar_model** out) {
    return guard([&] {
        if (!out)
            throw std::invalid_argument("out must not be NULL");
        *out = nullptr;
        if (!cfg || !HAS_FIELD(cfg, nemo_speech_diar_model_config, model_path) ||
            !cfg->model_path || !*cfg->model_path)
            throw std::invalid_argument("nemo_speech_diar_model_config.model_path is required");

        asr_core::DiarGeometry geo;
        if (HAS_FIELD(cfg, nemo_speech_diar_model_config, preset) && cfg->preset && *cfg->preset)
            geo = asr_core::DiarGeometry::preset(cfg->preset);
        if (HAS_FIELD(cfg, nemo_speech_diar_model_config, chunk_frames) && cfg->chunk_frames > 0)
            geo.chunk_len = cfg->chunk_frames;
        if (HAS_FIELD(cfg, nemo_speech_diar_model_config, right_context_frames) &&
            cfg->right_context_frames > 0)
            geo.chunk_right_context = cfg->right_context_frames;
        if (HAS_FIELD(cfg, nemo_speech_diar_model_config, left_context_frames) &&
            cfg->left_context_frames >= 0)
            geo.chunk_left_context = cfg->left_context_frames;
        if (HAS_FIELD(cfg, nemo_speech_diar_model_config, fifo_frames) && cfg->fifo_frames > 0)
            geo.fifo_len = cfg->fifo_frames;
        if (HAS_FIELD(cfg, nemo_speech_diar_model_config, spkcache_frames) &&
            cfg->spkcache_frames > 0)
            geo.spkcache_len = cfg->spkcache_frames;
        if (HAS_FIELD(cfg, nemo_speech_diar_model_config, update_period_frames) &&
            cfg->update_period_frames > 0)
            geo.spkcache_update_period = cfg->update_period_frames;

        const int gpu = HAS_FIELD(cfg, nemo_speech_diar_model_config, gpu) ? cfg->gpu : 0;
        auto handle = std::make_unique<nemo_speech_diar_model>();
        handle->impl = asr_core::Diarizer::load(gpu, cfg->model_path, geo);
        *out = handle.release();
        return NEMO_SPEECH_ASR_OK;
    });
}

void
nemo_speech_diar_destroy(nemo_speech_diar_model* model) {
    delete model;
}

int32_t
nemo_speech_diar_num_speakers(const nemo_speech_diar_model* model) {
    return model ? model->impl->num_speakers() : 0;
}

double
nemo_speech_diar_seconds_per_frame(const nemo_speech_diar_model* model) {
    if (!model)
        return 0.0;
    return model->impl->seconds_per_frame();
}

nemo_speech_asr_status
nemo_speech_diar_stream_open(nemo_speech_diar_model* model, nemo_speech_diar_stream** out) {
    return guard([&] {
        if (!model || !out)
            throw std::invalid_argument("model and out must not be NULL");
        *out = nullptr;  // never leave a stale pointer on failure
        auto s = std::make_unique<nemo_speech_diar_stream>();
        s->impl = model->impl->streaming_diarize();
        *out = s.release();
        return NEMO_SPEECH_ASR_OK;
    });
}

nemo_speech_asr_status
nemo_speech_diar_stream_push_f32(
    nemo_speech_diar_stream* stream, const float* samples, size_t n_samples, int32_t sample_rate) {
    return guard([&] {
        if (!stream || !stream->impl)
            throw std::invalid_argument("not a live diarization stream");
        if (!samples && n_samples > 0)
            throw std::invalid_argument("samples must not be NULL");
        stream->impl->push(samples, n_samples, sample_rate);
        return NEMO_SPEECH_ASR_OK;
    });
}

nemo_speech_asr_status
nemo_speech_diar_stream_finish(nemo_speech_diar_stream* stream) {
    return guard([&] {
        if (!stream || !stream->impl)
            throw std::invalid_argument("not a live diarization stream");
        stream->impl->finish();
        return NEMO_SPEECH_ASR_OK;
    });
}

void
nemo_speech_diar_stream_close(nemo_speech_diar_stream* stream) {
    delete stream;
}

nemo_speech_asr_status
nemo_speech_diar_offline_f32(
    nemo_speech_diar_model* model, const float* samples, size_t n_samples, int32_t sample_rate,
    nemo_speech_diar_stream** out) {
    return guard([&] {
        if (!model || !out)
            throw std::invalid_argument("model and out must not be NULL");
        *out = nullptr;  // never leave a stale pointer on failure
        if (!samples && n_samples > 0)
            throw std::invalid_argument("samples must not be NULL");
        auto s = std::make_unique<nemo_speech_diar_stream>();
        s->offline = model->impl->diarize(
            samples, n_samples, sample_rate, asr_core::DiarizationMode::Offline);
        *out = s.release();
        return NEMO_SPEECH_ASR_OK;
    });
}

namespace {
// Unify the two job flavors for the result accessors. `probs` covers frames
// [base, n_frames): long-lived streams compact old raw probabilities into
// frozen segments, so base can be > 0 there (offline jobs always have base 0).
struct DiarResultView {
    const float* probs;
    int64_t base;
    int64_t n_frames;
    int n_spk;
    double sec_per_frame;
};
DiarResultView
diar_view(const nemo_speech_diar_stream* s) {
    if (s->impl)
        return {
            s->impl->frame_probabilities().data(), s->impl->frame_probs_start(),
            s->impl->frame_count(), s->impl->num_speakers(), s->impl->seconds_per_frame()};
    const auto& offline = *s->offline;
    return {
        offline.frame_probabilities.data(), offline.frame_probs_start, offline.frame_count,
        offline.num_speakers, offline.seconds_per_frame};
}
}  // namespace

extern "C" int heimdall_diar_drain(nemo_speech_diar_stream* s, nemo_speech_diar_segment* out, size_t capacity, size_t* count) {
    return guard([&] {
        if (!s || !s->impl || !count) throw std::invalid_argument("invalid drain");
        *count=s->impl->finalized_segment_count();
        if(!out) return NEMO_SPEECH_ASR_OK;
        if(capacity<*count) throw std::invalid_argument("drain capacity too small");
        auto segments = s->impl->take_finalized_segments();
        *count = segments.size();
        for (size_t i = 0; i < segments.size(); ++i)
            out[i] = {segments[i].t0, segments[i].t1, segments[i].speaker + 1};
        return NEMO_SPEECH_ASR_OK;
    });
}
extern "C" int64_t heimdall_diar_output_revision(const nemo_speech_diar_stream* s) {
    return s && s->impl ? s->impl->output_revision() : (s ? diar_view(s).n_frames : 0);
}
extern "C" int64_t heimdall_diar_stable_frames(const nemo_speech_diar_stream* s) {
    return s && s->impl ? s->impl->stable_frames() : (s ? diar_view(s).n_frames : 0);
}
extern "C" int heimdall_diar_probs_since(const nemo_speech_diar_stream* s, int64_t since, float* out, size_t capacity) {
    return guard([&] {
        if (!s || !out) throw std::invalid_argument("stream and output required");
        const auto v=diar_view(s);
        const auto first=std::max(v.base,std::min(since,v.n_frames));
        const size_t count=static_cast<size_t>(v.n_frames-first)*v.n_spk;
        if(capacity<count) throw std::invalid_argument("probability tail capacity");
        std::memcpy(out,v.probs+(first-v.base)*v.n_spk,count*sizeof(float));
        return NEMO_SPEECH_ASR_OK;
    });
}

int64_t
nemo_speech_diar_frame_count(const nemo_speech_diar_stream* stream) {
    return stream ? diar_view(stream).n_frames : 0;
}

int64_t
nemo_speech_diar_frame_probs_start(const nemo_speech_diar_stream* stream) {
    return stream ? diar_view(stream).base : 0;
}

nemo_speech_asr_status
nemo_speech_diar_frame_probs(
    const nemo_speech_diar_stream* stream, float* out, size_t capacity_floats) {
    return guard([&] {
        if (!stream || !out)
            throw std::invalid_argument("stream and out must not be NULL");
        const auto v = diar_view(stream);
        const size_t need = static_cast<size_t>(v.n_frames - v.base) * v.n_spk;
        if (capacity_floats < need)
            throw std::invalid_argument(
                "capacity_floats too small (need " + std::to_string(need) + ")");
        std::memcpy(out, v.probs, need * sizeof(float));
        return NEMO_SPEECH_ASR_OK;
    });
}

nemo_speech_asr_status
nemo_speech_diar_segments(
    const nemo_speech_diar_stream* stream, const nemo_speech_diar_segmentation_config* cfg,
    nemo_speech_diar_segment* out, size_t capacity, size_t* count) {
    return guard([&] {
        if (!stream || !count)
            throw std::invalid_argument("stream and count must not be NULL");
        asr_core::DiarSegmentationCfg sc;  // library defaults
        if (cfg) {
            if (HAS_FIELD(cfg, nemo_speech_diar_segmentation_config, onset) && cfg->onset > 0)
                sc.onset = cfg->onset;
            if (HAS_FIELD(cfg, nemo_speech_diar_segmentation_config, offset) && cfg->offset > 0)
                sc.offset = cfg->offset;
            if (HAS_FIELD(cfg, nemo_speech_diar_segmentation_config, pad_onset_sec) &&
                cfg->pad_onset_sec > 0)
                sc.pad_onset = cfg->pad_onset_sec;
            if (HAS_FIELD(cfg, nemo_speech_diar_segmentation_config, pad_offset_sec) &&
                cfg->pad_offset_sec > 0)
                sc.pad_offset = cfg->pad_offset_sec;
            if (HAS_FIELD(cfg, nemo_speech_diar_segmentation_config, min_gap_sec) &&
                cfg->min_gap_sec > 0)
                sc.min_duration_off = cfg->min_gap_sec;
            if (HAS_FIELD(cfg, nemo_speech_diar_segmentation_config, min_duration_sec) &&
                cfg->min_duration_sec > 0)
                sc.min_duration_on = cfg->min_duration_sec;
        }
        // Live streams go through the core stream so compacted prefixes
        // (frozen segments) are included; offline jobs segment the full probs.
        const auto v = diar_view(stream);
        const auto segs = stream->impl ? stream->impl->segments(sc)
                                       : asr_core::diar_segments_from_probs(
                                             v.probs, v.n_frames, v.n_spk, v.sec_per_frame, sc);
        *count = segs.size();
        if (!out)
            return NEMO_SPEECH_ASR_OK;  // size query
        if (capacity < segs.size())
            throw std::invalid_argument(
                "capacity too small (need " + std::to_string(segs.size()) + ")");
        for (size_t i = 0; i < segs.size(); i++) {
            out[i].start_time = segs[i].t0;
            out[i].end_time = segs[i].t1;
            out[i].speaker = segs[i].speaker + 1;  // 1-based, like WordInfo.speaker_tag
        }
        return NEMO_SPEECH_ASR_OK;
    });
}
#ifndef NEMO_SPEECH_VERSION_STR
#define NEMO_SPEECH_VERSION_STR "0.0.0"  // overridden by the build from ./VERSION
#endif
const char*
nemo_speech_asr_version(void) {
    return "nemo-speech-asr " NEMO_SPEECH_VERSION_STR;
}

}  // extern "C"

// Model-free 60-minute continuous overlapping speech through the identical
// live compaction/segmentation functions. Measures queries at a fixed tail size.
extern "C" int heimdall_diar_continuous_hour(double* query_us, size_t* max_frames,
    size_t* max_pending, double* archived_speaker_seconds) {
    return guard([&] {
        {
            std::vector<float> values(100,0.5f); values[0]=0.9f;
            std::vector<asr_core::DiarSegment> saved; std::vector<bool> active;
            int64_t base=0;
            asr_core::compact_diar_timeline(values,base,saved,active,1,0.01,100,60,30);
            auto tail=asr_core::diar_segments_from_probs(values.data(),values.size(),1,0.01,{},&active);
            if(saved.size()!=1 || tail.size()!=1 || std::abs(tail[0].t1-0.3)>1e-9)
                throw std::runtime_error("cut lost hysteresis at exact threshold");
        }
        const int speakers=8;
        const double step=0.01;
        std::vector<float> probs;
        std::vector<asr_core::DiarSegment> finalized;
        int64_t base=0;
        std::vector<bool> active;
        *max_frames=0; *max_pending=0; *archived_speaker_seconds=0;
        std::vector<double> measurements;
        for (int second=1;second<=3600;++second) {
            // Every channel remains active, so no all-speaker silence is possible.
            probs.insert(probs.end(),100*speakers,0.9f);
            asr_core::compact_diar_timeline(probs,base,finalized,active,speakers,step,
                second*100,6000,3000);
            *max_frames=std::max(*max_frames,probs.size()/speakers);
            *max_pending=std::max(*max_pending,finalized.size());
            for (const auto& seg:finalized) *archived_speaker_seconds+=seg.t1-seg.t0;
            finalized.clear(); // Same drain/acknowledgement as the archive sink.
            if (second%600==0 || second==120) {
                // Equal 30 s input removes sawtooth tail-length noise.
                std::vector<double> timing;
                for (int repeat=0;repeat<101;++repeat) {
                    const auto begin=std::chrono::steady_clock::now();
                    auto tail=asr_core::diar_segments_from_probs(probs.data()+probs.size()-3000*speakers,3000,speakers,step,{});
                    if(tail.size()!=8) throw std::runtime_error("continuous interval missing");
                    const auto end=std::chrono::steady_clock::now();
                    timing.push_back(std::chrono::duration<double,std::micro>(end-begin).count());
                }
                std::sort(timing.begin(),timing.end());
                measurements.push_back(timing[50]);
            }
        }
        const auto tail=asr_core::diar_segments_from_probs(probs.data(),probs.size()/speakers,speakers,step,{});
        for(const auto& seg:tail) *archived_speaker_seconds+=seg.t1-seg.t0;
        std::copy(measurements.begin(),measurements.end(),query_us);
        return NEMO_SPEECH_ASR_OK;
    });
}
