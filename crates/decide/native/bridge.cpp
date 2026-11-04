// Heimdall C ABI over llama.cpp's typed decision route. No socket or server process.
#include "server-context.h"
#include "json.h"
#include "common.h"
#include "llama.h"
#include "log.h"
#include <cstdlib>
#include <cstring>
#include <mutex>
#include "ownership.h"
#include <thread>
#include <atomic>
#include <chrono>
#include <map>
#include "ggml.h"
#include "ggml-backend.h"
#include "gguf.h"
#include <fstream>
#include <cmath>
#include <regex>
#include <cstdint>
extern "C" uint64_t heimdall_metal_gpu_ns(void);
extern "C" bool heimdall_metal_recreate_devices(void);
extern "C" bool heimdall_metal_synchronize_owned(void *, bool);
extern "C" bool heimdall_d1_recreate_device(void) {
    heimdall_native_scope ownership(nullptr);
    return heimdall_metal_recreate_devices();
}
struct node_profile {
 std::chrono::steady_clock::time_point start;
 std::map<std::string,std::pair<uint64_t,double>> ops;
};
static bool profile_node(ggml_tensor * t, bool ask, void * data) {
 auto &p=*static_cast<node_profile*>(data);
 if(ask) {p.start=std::chrono::steady_clock::now();return true;}
 if (const char *dir=getenv("HEIMDALL_D1_CAPTURE")) {
  if (std::string(t->name).find("conformer.pre_encode.conv")!=std::string::npos && t->type==GGML_TYPE_F32) {
   std::vector<float> values(ggml_nelements(t));ggml_backend_tensor_get(t,values.data(),0,values.size()*sizeof(float));
   std::ofstream out(std::string(dir)+"/"+t->name+".bin",std::ios::binary);out.write(reinterpret_cast<const char*>(values.data()),values.size()*sizeof(float));
  }
 }
 auto &v=p.ops[std::string(ggml_op_name(t->op))+":"+t->name];v.first++;v.second+=std::chrono::duration<double,std::milli>(std::chrono::steady_clock::now()-p.start).count();return true;
}

static std::once_flag initialized;
static std::atomic<int> test_audio_delay_ms{0};
static std::atomic<int> test_audio_phase{0};
static char * copy_text(const std::string & s) {
    char * p = static_cast<char *>(std::malloc(s.size() + 1));
    if (p) std::memcpy(p, s.c_str(), s.size() + 1);
    return p;
}
struct decider {
    bool gpu;
    bool fail_next = false;
    node_profile profile;
    double gpu_ms=0;
    common_params params;
    server_context ctx;
    std::unique_ptr<server_routes> routes;
    std::thread loop;
    ~decider() { ctx.terminate(); if (loop.joinable()) loop.join(); }
};
// True for an LFM2 decision encoder (decision head blocks, non-causal trunk).
static bool heimdall_joint_decisions(const char * model) {
    gguf_init_params params = {/*.no_alloc =*/ true, /*.ctx =*/ nullptr};
    gguf_context * meta = gguf_init_from_file(model, params);
    if (!meta) return false;
    bool joint = false;
    const int64_t arch = gguf_find_key(meta, "general.architecture");
    if (arch >= 0 && std::string(gguf_get_val_str(meta, arch)) == "lfm2") {
        const int64_t blocks = gguf_find_key(meta, "lfm2.decision.block_count");
        joint = blocks >= 0 && gguf_get_val_u32(meta, blocks) > 0;
    }
    gguf_free(meta);
    return joint;
}
extern "C" {
const char * heimdall_d1_version() { return getenv("HEIMDALL_D1_CONV") && std::string(getenv("HEIMDALL_D1_CONV"))=="direct" ? "llama.cpp@62a6f74bab748b1755f4cd8dfc0871caf865c6c8;F16;direct-conv;native-ownership-v1;selected-head-v1;request-media-v1;pcm-v1;centered-conv-v1;audio-fft512-v1;direct-mm-v1;joint-decision-v1;flat-im2col-v1;tiled-cpy-v1;sparse-mel-v1;current-first-v1;text-send-v1;async-owner-v1;text-window-v1" : "llama.cpp@62a6f74bab748b1755f4cd8dfc0871caf865c6c8;F16;precise-pointwise-v1;native-ownership-v1;selected-head-v1;request-media-v1;pcm-v1;centered-conv-v1;audio-fft512-v1;direct-mm-v1;joint-decision-v1;flat-im2col-v1;tiled-cpy-v1;sparse-mel-v1;current-first-v1;text-send-v1;async-owner-v1;text-window-v1"; }
void heimdall_d1_free(char * p) { std::free(p); }
static void * create_backend(const char * model, const char * projector, int threads, bool gpu, bool text_only, char ** error) {
    *error = nullptr;
    try {
        heimdall_native_scope creation(nullptr);
        std::call_once(initialized, [] { common_init(); common_log_set_verbosity_thold(-1); llama_backend_init(); });
        auto d = std::make_unique<decider>();
        heimdall_native_scope ownership(d.get());
        d->gpu = gpu;
        if (getenv("HEIMDALL_D1_FAULT_TRACE")) fprintf(stderr,"d1 create gpu=%d\n",gpu);
        if (getenv("HEIMDALL_D1_PROFILE")) {d->params.cb_eval=profile_node;d->params.cb_eval_user_data=&d->profile;}
        d->params.model.path = model;
        d->params.mmproj.path = projector;
        // Production questions are all text, fitted to 2,000 tokens in Rust.
        // Historical independent media parity tests retain the legacy ABI slot.
        d->params.n_parallel = 1;
        d->params.n_ctx = text_only ? 2048 : 16384;
        d->params.n_batch = d->params.n_ctx;
        d->params.n_ubatch = d->params.n_ctx;
        d->params.n_gpu_layers = gpu ? 99 : 0;
        d->params.mmproj_use_gpu = gpu;
        d->params.cpuparams.n_threads = threads;
        d->params.cpuparams_batch.n_threads = threads;
        d->params.cpuparams.poll = 0;
        d->params.cpuparams_batch.poll = 0;
        d->params.warmup = false;
        d->params.ctx_shift = false;
        d->params.cache_prompt = false;
        d->params.use_jinja = true;
        d->params.fit_params = false;
        d->routes = std::make_unique<server_routes>(d->params, d->ctx);
        if (!d->ctx.load_model(d->params)) throw std::runtime_error("d1 model load failed");
        d->routes->update_meta(d->ctx);
        auto ptr = d.get();
        d->loop = std::thread([ptr] { ptr->ctx.start_loop(); });
        return d.release();
    } catch (const std::exception & e) { *error = copy_text(e.what()); return nullptr; }
    catch (...) { *error = copy_text("unknown native create error"); return nullptr; }
}
void * heimdall_d1_create_text_backend(const char * model, const char *, int threads, bool gpu, char ** error) {
    return create_backend(model, "", threads, gpu, true, error);
}
void * heimdall_d1_create_backend(const char * model, const char * projector, int threads, bool gpu, char ** error) {
    return create_backend(model, projector, threads, gpu, false, error);
}
void * heimdall_d1_create(const char * model, const char * projector, int threads, char ** error) {
    return heimdall_d1_create_backend(model, projector, threads, true, error);
}
static char * decide_impl(void * handle, const char * body, const float * pcm, size_t samples, int timeout_ms, char ** error, bool (*interrupted)(void *) = nullptr, void * interrupt_ctx = nullptr) {
    *error = nullptr;
    try {
        auto d = static_cast<decider *>(handle);
        auto deadline = std::chrono::steady_clock::now() + std::chrono::milliseconds(timeout_ms);
        const std::function<bool()> stop = [=] { return std::chrono::steady_clock::now() > deadline || (interrupted && interrupted(interrupt_ctx)); };
        server_http_req req { {}, {}, "/v1/systemone", "", body, {}, stop };
        if (pcm) {
            if (const int ms = test_audio_delay_ms.exchange(0); ms > 0) {
                test_audio_phase.store(2);
                std::this_thread::sleep_for(std::chrono::milliseconds(ms));
                test_audio_phase.store(3);
            }
            if (samples < 1 || samples > 480000) throw std::invalid_argument("d1 PCM sample bound");
            for (size_t i = 0; i < samples; ++i) if (!std::isfinite(pcm[i]) || std::abs(pcm[i]) > 1) throw std::invalid_argument("invalid d1 PCM");
            // Native upload instead of base64 in JSON. The same pinned audio
            // loader still receives a mono 16-kHz float WAV, byte-exact PCM.
            auto & upload = req.files["heimdall_pcm"];
            upload.content_type = "audio/wav";
            upload.data.resize(44 + samples * sizeof(float));
            auto * bytes = upload.data.data();
            auto u32 = [&](size_t offset, uint32_t value) { std::memcpy(bytes + offset, &value, 4); };
            auto u16 = [&](size_t offset, uint16_t value) { std::memcpy(bytes + offset, &value, 2); };
            std::memcpy(bytes, "RIFF", 4); u32(4, 36 + samples * sizeof(float));
            std::memcpy(bytes + 8, "WAVEfmt ", 8); u32(16, 16); u16(20, 3); u16(22, 1);
            u32(24, 16000); u32(28, 64000); u16(32, 4); u16(34, 32);
            std::memcpy(bytes + 36, "data", 4); u32(40, samples * sizeof(float));
            std::memcpy(bytes + 44, pcm, samples * sizeof(float));
        }

        const bool latency = getenv("HEIMDALL_D1_LATENCY") != nullptr;
        const auto started = std::chrono::steady_clock::now();
        uint64_t gpu_before;
        {
            heimdall_native_scope preparation(d);
            gpu_before = heimdall_metal_gpu_ns();
            d->profile.ops.clear();
        }
        const auto prepared = std::chrono::steady_clock::now();
        auto res = d->routes->post_systemone(req);
        const auto responded = std::chrono::steady_clock::now();
        // Acquire after the response: update_slots may publish it before its
        // callback has finished. Wait for that active callback before readback.
        heimdall_native_scope ownership(d);
        const auto acquired = std::chrono::steady_clock::now();
        const bool inject = d->fail_next;
        d->fail_next = false;
        if (d->gpu && !heimdall_metal_synchronize_owned(d, inject))
            throw std::runtime_error("HEIMDALL_GPU_FAULT: d1 synchronization failed");
        d->gpu_ms=(heimdall_metal_gpu_ns()-gpu_before)/1e6;
        if (latency && res->status == 200) {
            auto out = common_json::parse(res->data);
            auto ms = [](auto a, auto b) { return std::chrono::duration<double,std::milli>(b-a).count(); };
            out["heimdall_latency"]["initial_ownership_ms"] = ms(started, prepared);
            out["heimdall_latency"]["route_ms"] = ms(prepared, responded);
            out["heimdall_latency"]["final_ownership_ms"] = ms(responded, acquired);
            out["heimdall_latency"]["final_sync_ms"] = ms(acquired, std::chrono::steady_clock::now());
            res->data = out.dump();
        }
        if(getenv("HEIMDALL_D1_PROFILE")) {auto out=common_json::parse(res->data);out["heimdall_profile"]=common_json::array();for(auto &v:d->profile.ops)out["heimdall_profile"].push_back({{"node",v.first},{"count",v.second.first},{"ms",v.second.second}});res->data=out.dump();}
        if (interrupted && interrupted(interrupt_ctx)) throw std::runtime_error("d1 send preempted by foreground question; transcript retained");
        if (stop()) throw std::runtime_error("d1 request deadline exceeded");
        if (res->status != 200) throw std::runtime_error(res->data);
        return copy_text(res->data);
    } catch (const std::exception & e) { *error = copy_text(e.what()); return nullptr; }
    catch (...) { *error = copy_text("unknown native decide error"); return nullptr; }
}
int heimdall_d1_tokens(void * handle, const char * text) {
    heimdall_native_scope ownership(handle);
    auto d = static_cast<decider *>(handle);
    // Match the pinned d1-omni state escaping before budgeting. Otherwise a
    // pasted special token counts as one here but expands inside the route.
    static const std::regex special("<\\|([A-Za-z0-9_]+)\\|>");
    auto escaped = std::regex_replace(std::string(text), special, "<\xC2\xA6$1\xC2\xA6>");
    size_t at = 0;
    while ((at = escaped.find("<<d1omni:", at)) != std::string::npos) {
        escaped.replace(at, 9, "<<d1omni "); at += 9;
    }
    return common_tokenize(llama_model_get_vocab(llama_get_model(d->ctx.get_llama_context())), escaped, false, true).size();
}
char * heimdall_d1_decide(void * handle, const char * body, int timeout_ms, char ** error) {
    return decide_impl(handle, body, nullptr, 0, timeout_ms, error);
}
char * heimdall_d1_decide_interruptible(void * handle, const char * body, int timeout_ms, bool (*interrupted)(void *), void * ctx, char ** error) {
    return decide_impl(handle, body, nullptr, 0, timeout_ms, error, interrupted, ctx);
}
// Test exercises the exact process-wide lock used by model preparation and GPU
// callbacks; no separate mock mutex. The callback announces lock acquisition.
void heimdall_d1_test_delay_audio(int ms) { test_audio_phase.store(1); test_audio_delay_ms.store(ms); }
int heimdall_d1_test_audio_phase() { return test_audio_phase.load(); }
void heimdall_d1_test_hold_lock(int ms, void (*entered)(void *), void * ctx) {
    heimdall_native_scope ownership(nullptr);
    entered(ctx);
    std::this_thread::sleep_for(std::chrono::milliseconds(ms));
}
char * heimdall_d1_decide_pcm(void * handle, const char * body, const float * pcm, size_t samples, int timeout_ms, char ** error) {
    if (!pcm) { *error = copy_text("null d1 PCM"); return nullptr; }
    return decide_impl(handle, body, pcm, samples, timeout_ms, error);
}
void heimdall_d1_test_fail_next(void * handle) {
    heimdall_native_scope ownership(handle);
    static_cast<decider *>(handle)->fail_next = true;
}
double heimdall_d1_gpu_ms(void * handle) {
    heimdall_native_scope ownership(handle);
    return static_cast<decider *>(handle)->gpu_ms;
}
void heimdall_d1_destroy(void * handle) {
    auto d = static_cast<decider *>(handle);
    // Join without holding ownership: a pending callback must be able to drain.
    d->ctx.terminate();
    if (d->loop.joinable()) d->loop.join();
    heimdall_native_scope ownership(d);
    delete d;
}
}
