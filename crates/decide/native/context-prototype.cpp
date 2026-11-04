// Offline-only companion to the shipped runtime. Never linked into the app.
// Reuse its exact graph, route, ownership and PCM plumbing; change only the
// slot/window configuration. Link against the pinned, already-built dylib.
#include "bridge.cpp"

extern "C" void * heimdall_context_create(const char * model, const char * projector,
                                         int window, char ** error) {
    *error = nullptr;
    try {
        if (window < 512 || window > 16384) throw std::runtime_error("window outside prototype budget");
        heimdall_native_scope creation(nullptr);
        std::call_once(initialized, [] { common_init(); common_log_set_verbosity_thold(-1); llama_backend_init(); });
        auto d = std::make_unique<decider>();
        heimdall_native_scope ownership(d.get());
        d->gpu = true;
        d->params.model.path = model;
        d->params.mmproj.path = projector;
        d->params.n_parallel = 1;
        d->params.n_ctx = window;
        d->params.n_batch = window;
        d->params.n_ubatch = window;
        d->params.n_gpu_layers = 99;
        d->params.mmproj_use_gpu = true;
        d->params.cpuparams.n_threads = 2;
        d->params.cpuparams_batch.n_threads = 2;
        d->params.cpuparams.poll = 0;
        d->params.cpuparams_batch.poll = 0;
        d->params.warmup = false;
        d->params.ctx_shift = false;
        d->params.cache_prompt = true;
        d->params.use_jinja = true;
        d->params.fit_params = false;
        d->routes = std::make_unique<server_routes>(d->params, d->ctx);
        if (!d->ctx.load_model(d->params)) throw std::runtime_error("prototype load failed");
        d->routes->update_meta(d->ctx);
        auto ptr = d.get();
        d->loop = std::thread([ptr] { ptr->ctx.start_loop(); });
        return d.release();
    } catch (const std::exception & e) { *error = copy_text(e.what()); return nullptr; }
}

extern "C" char * heimdall_context_info(void * handle) {
    heimdall_native_scope ownership(handle);
    auto d = static_cast<decider *>(handle);
    auto ctx = d->ctx.get_llama_context();
    auto memory = llama_get_memory(ctx);
    common_json out = {{"causal", llama_get_causal_attn(ctx)},
        {"has_sequence_memory", memory != nullptr}, {"window", d->params.n_ctx},
        {"sequence_state_bytes", llama_state_seq_get_size(ctx, 0)},
        {"prompt_cache_requested", true},
        {"runtime", heimdall_d1_version()}};
    // Do not call save/restore on a memory-less encoder: there is no prefix
    // state to restore, even if the API can serialize an empty envelope.
    out["cache_supported"] = memory != nullptr && llama_get_causal_attn(ctx);
    return copy_text(out.dump());
}

extern "C" int heimdall_context_tokens(void * handle, const char * text) {
    heimdall_native_scope ownership(handle);
    auto d = static_cast<decider *>(handle);
    return common_tokenize(llama_model_get_vocab(llama_get_model(d->ctx.get_llama_context())),
                           std::string(text), false, true).size();
}

extern "C" char * heimdall_context_decide(void * handle, const char * body,
        const float * pcm, size_t samples, char ** error) {
    return decide_impl(handle, body, pcm, samples, 30000, error);
}
