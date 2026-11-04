// One thread per im2col output element. The pinned kernel launches one
// threadgroup per output pixel with KH*KW (or one) threads, which leaves the
// GPU mostly idle on the audio front end's 3x3/1x1 convolutions. Same copy and
// float->T conversion per element, so outputs are bit-identical.
template <typename T>
kernel void kernel_heimdall_im2col(
        constant ggml_metal_kargs_im2col & args,
        device const float * x,
        device        char * dst,
        constant uint4 & shape,   // OH, OW, total (< 2^32), unused
        uint tid [[thread_position_in_grid]],
        uint tpg [[threads_per_grid]]) {
    const uint total = shape.z;
    const uint OH = shape.x, OW = shape.y, CHW = args.CHW, KHW = args.KHW, KW = args.KW;
    device T * pdst = (device T *) dst;
    for (uint i = tid; i < total; i += tpg) {
        // 32-bit index arithmetic (the host requires total < 2^32); offsets are 64-bit.
        const uint p = i / CHW;
        const uint chw = i - p*CHW;
        const uint iow = p % OW;
        const uint q = p / OW;
        const uint ioh = q % OH;
        const uint64_t in = q / OH;
        const uint iic = chw / KHW;
        const uint khw = chw - iic*KHW;
        const uint ikh = khw / KW;
        const uint ikw = khw - ikh*KW;
        const int iiw = (int) (iow*args.s0 + ikw*args.d0) - args.p0;
        const int iih = (int) (ioh*args.s1 + ikh*args.d1) - args.p1;
        if (iih < 0 || iih >= args.IH || iiw < 0 || iiw >= args.IW) {
            pdst[i] = 0.0f;
        } else {
            pdst[i] = x[in*args.ofs0 + (uint64_t) iic*args.ofs1 + (uint64_t) iih*args.IW + iiw];
        }
    }
}
typedef decltype(kernel_heimdall_im2col<float>) heimdall_im2col_t;
template [[host_name("kernel_heimdall_im2col_f32")]] kernel heimdall_im2col_t kernel_heimdall_im2col<float>;
template [[host_name("kernel_heimdall_im2col_f16")]] kernel heimdall_im2col_t kernel_heimdall_im2col<half>;
