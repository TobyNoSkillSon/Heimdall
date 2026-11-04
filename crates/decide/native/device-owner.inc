
static std::vector<ggml_metal_device_ptr>& heimdall_device_owners() {
    static std::vector<ggml_metal_device_ptr> devs;
    return devs;
}
extern "C" ggml_metal_device_t heimdall_metal_device_recreate(ggml_metal_device_t old, int device, int count) {
    for (auto& owned : heimdall_device_owners()) {
        if (owned.get() == old) {
            owned.reset();
            owned.reset(ggml_metal_device_init(device, count));
            return owned.get();
        }
    }
    return nullptr;
}
