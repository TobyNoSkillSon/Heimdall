// Tiled copy for F32 CONT/CPY whose source rows are strided (a permuted
// view where dimension 1 is closer in memory than dimension 0). The pinned
// kernel reads such sources one cache line per element; here a 32x32 tile is
// read along dimension 1 and written along dimension 0. Pure copy: the output
// bytes are identical. Destination must be contiguous (linear element order).
kernel void kernel_heimdall_cpy_tiled_f32(
        constant ggml_metal_kargs_cpy & args,
        device const char * src0,
        device       char * dst,
        uint3  tgpig [[threadgroup_position_in_grid]],
        uint3  tpitg [[thread_position_in_threadgroup]]) {
    threadgroup float tile[32][33];
    const int x0 = tgpig.x*32;           // i00
    const int y0 = tgpig.y*32;           // i01
    const int i02 = tgpig.z % args.ne02;
    const int i03 = tgpig.z / args.ne02;
    device const char * base = src0 + i03*args.nb03 + i02*args.nb02;
    for (int b = tpitg.y; b < 32; b += 8) {
        const int i00 = x0 + b, i01 = y0 + tpitg.x;
        if (i00 < args.ne00 && i01 < args.ne01) {
            tile[b][tpitg.x] = *(device const float *)(base + i00*args.nb00 + i01*args.nb01);
        }
    }
    threadgroup_barrier(mem_flags::mem_threadgroup);
    device float * out = (device float *) dst + ((int64_t) i03*args.ne02 + i02)*args.ne01*args.ne00;
    for (int b = tpitg.y; b < 32; b += 8) {
        const int i01 = y0 + b, i00 = x0 + tpitg.x;
        if (i00 < args.ne00 && i01 < args.ne01) {
            out[(int64_t) i01*args.ne00 + i00] = tile[tpitg.x][b];
        }
    }
}
