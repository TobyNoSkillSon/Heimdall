// FP32, no contraction: same rounded intermediates as the separate ggml nodes.
// li/ri: neighbour rows (-1: none), see centered-conv.inc.
kernel void kernel_heimdall_centered(
    constant ulong4 & args [[buffer(0)]],
    device const float * a [[buffer(1)]],
    device const float * w [[buffer(2)]],
    device const float * left [[buffer(3)]],
    device const float * right [[buffer(4)]],
    device float * out [[buffer(5)]],
    device const int * li [[buffer(6)]],
    device const int * ri [[buffer(7)]],
    uint i [[thread_position_in_grid]]) {
    const ulong h=args.x, n=args.y, stride=args.z;
    if (i >= h*n) return;
    const ulong t=i/h, d=i%h, p=t*stride+d;
    const float bx=a[p]*a[p+2*h];
    float prv=0.0f, nxt=0.0f;
    if (li[t]>=0) { const ulong q=(ulong)li[t]*stride+d; prv=a[q]*a[q+2*h]; }
    if (ri[t]>=0) { const ulong q=(ulong)ri[t]*stride+d; nxt=a[q]*a[q+2*h]; }
    const float middle=bx*w[3*d+1];
    const float lp=prv*w[3*d], rp=nxt*w[3*d+2];
    const float l=lp*left[t], r=rp*right[t];
    const float first=middle+l;
    const float y=first+r;
    out[i]=a[p+h]*y;
}
