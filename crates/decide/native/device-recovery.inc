
extern "C" ggml_metal_device_t heimdall_metal_device_recreate(ggml_metal_device_t old, int device, int count);
extern "C" bool heimdall_metal_recreate_devices(void) {
    std::lock_guard<std::recursive_mutex> lock(heimdall_native_mutex());
    if (getenv("HEIMDALL_D1_FAULT_TRACE")) fprintf(stderr,"device reset active=%d\n",heimdall_active_backends);
    if (heimdall_active_backends != 0) return false;
    auto reg = ggml_backend_metal_reg();
    const size_t count = ggml_backend_reg_dev_count(reg);
    for (size_t i = 0; i < count; ++i) {
        auto dev = ggml_backend_reg_dev_get(reg, i);
        dev->context = heimdall_metal_device_recreate((ggml_metal_device_t)dev->context, (int)i, (int)count);
        if (!dev->context) return false;
    }
    return true;
}

extern "C" bool heimdall_metal_context_failed(ggml_metal_t ctx);
extern "C" void heimdall_metal_context_fail(ggml_metal_t ctx);
extern "C" bool heimdall_metal_synchronize_owned(void * owner, bool inject) {
    heimdall_native_scope ownership(owner);
    bool healthy = true;
    for (auto entry : heimdall_contexts) {
        if (entry.second != owner) continue;
        if (inject) { heimdall_metal_context_fail(entry.first); inject = false; }
        ggml_metal_synchronize(entry.first);
        healthy &= !heimdall_metal_context_failed(entry.first);
    }
    return healthy;
}
