// Fixed tensor parity against the pinned primitive graph, on CPU and Metal.
#include "ggml.h"
#include "ggml-backend.h"
#include "ggml-alloc.h"
#include "centered-conv.inc"
#include <vector>
#include <cstdio>
#include <cmath>
#include <cstring>
#include <stdexcept>
int main() {
    ggml_backend_load_all();
    for (const char * device : {"CPU", "Metal"}) {
        auto backend = ggml_backend_dev_init(ggml_backend_dev_by_type(std::strcmp(device,"CPU")==0 ? GGML_BACKEND_DEVICE_TYPE_CPU : GGML_BACKEND_DEVICE_TYPE_GPU), nullptr);
        if (!backend) throw std::runtime_error("missing test backend");
        for (const int h : {7, 1024, 2048}) for (const int n : {1, 2, 17, 127}) {
            auto ctx = ggml_init({32*1024*1024, nullptr, true});
            auto * a = ggml_new_tensor_2d(ctx, GGML_TYPE_F32, 3*h, n);
            auto * w = ggml_new_tensor_2d(ctx, GGML_TYPE_F32, 3, h);
            auto * l = ggml_new_tensor_2d(ctx, GGML_TYPE_F32, 1, n);
            auto * r = ggml_new_tensor_2d(ctx, GGML_TYPE_F32, 1, n);
            auto * li = ggml_new_tensor_1d(ctx, GGML_TYPE_I32, n);
            auto * ri = ggml_new_tensor_1d(ctx, GGML_TYPE_I32, n);
            auto * b = ggml_view_2d(ctx, a, h, n, a->nb[1], 0);
            auto * c = ggml_view_2d(ctx, a, h, n, a->nb[1], h*4);
            auto * x = ggml_view_2d(ctx, a, h, n, a->nb[1], 2*h*4);
            auto * bx = ggml_mul(ctx, b, x);
            auto * padded = ggml_pad_ext(ctx, bx, 0,0,1,1,0,0,0,0);
            auto * prv = ggml_view_2d(ctx, padded, h,n,padded->nb[1],0);
            auto * nxt = ggml_view_2d(ctx, padded, h,n,padded->nb[1],2*padded->nb[1]);
            auto * taps = ggml_cont(ctx, ggml_transpose(ctx,w));
            auto * t0 = ggml_view_1d(ctx,taps,h,0);
            auto * t1 = ggml_view_1d(ctx,taps,h,taps->nb[1]);
            auto * t2 = ggml_view_1d(ctx,taps,h,2*taps->nb[1]);
            auto * y = ggml_mul(ctx,bx,t1);
            y = ggml_add(ctx,y,ggml_mul(ctx,ggml_mul(ctx,prv,t0),l));
            y = ggml_add(ctx,y,ggml_mul(ctx,ggml_mul(ctx,nxt,t2),r));
            auto * stock = ggml_mul(ctx,c,y);
            auto * fused = heimdall_centered(ctx,a,w,l,r,li,ri);
            // Exercise the downstream barrier too; an isolated custom output cannot.
            auto * stock_consumer = ggml_sqr(ctx, stock);
            auto * fused_consumer = ggml_sqr(ctx, fused);
            auto * graph = ggml_new_graph(ctx);
            ggml_build_forward_expand(graph,stock_consumer);
            ggml_build_forward_expand(graph,fused_consumer);
            auto buffer = ggml_backend_alloc_ctx_tensors(ctx, backend);
            std::vector<float> av(3*h*n),wv(3*h),lv(n),rv(n);
            for (size_t i=0;i<av.size();++i) av[i] = std::sin(float(i)*.123f)*8.f;
            for (size_t i=0;i<wv.size();++i) wv[i] = std::cos(float(i)*.321f)*.8f;
            for(int i=0;i<n;++i) {lv[i]=i && i!=n/2;rv[i]=i+1<n && i!=n/2-1;}
            ggml_backend_tensor_set(a,av.data(),0,av.size()*4);
            ggml_backend_tensor_set(w,wv.data(),0,wv.size()*4);
            ggml_backend_tensor_set(l,lv.data(),0,lv.size()*4);
            ggml_backend_tensor_set(r,rv.data(),0,rv.size()*4);
            std::vector<int32_t> liv(n),riv(n);
            for(int i=0;i<n;++i) {liv[i]=i-1;riv[i]=i+1<n?i+1:-1;}
            ggml_backend_tensor_set(li,liv.data(),0,liv.size()*4);
            ggml_backend_tensor_set(ri,riv.data(),0,riv.size()*4);
            if(ggml_backend_graph_compute(backend,graph)!=GGML_STATUS_SUCCESS) throw std::runtime_error("graph failed");
            std::vector<float> s(h*n),f(h*n),cs(h*n),cf(h*n);
            ggml_backend_tensor_get(stock,s.data(),0,s.size()*4);
            ggml_backend_tensor_get(fused,f.data(),0,f.size()*4);
            ggml_backend_tensor_get(stock_consumer,cs.data(),0,cs.size()*4);
            ggml_backend_tensor_get(fused_consumer,cf.data(),0,cf.size()*4);
            float max=0;size_t changed=0;
            for(size_t i=0;i<s.size();++i) {max=std::fmax(max,std::abs(s[i]-f[i]));changed+=std::memcmp(&s[i],&f[i],4)!=0;}
            printf("%s h=%d n=%d changed=%zu/%zu max=%g\n",device,h,n,changed,s.size(),max);
            if(cs != cf) throw std::runtime_error("downstream barrier failed");
            if(changed) throw std::runtime_error("centred kernel not exact");
            ggml_backend_buffer_free(buffer);ggml_free(ctx);
        }
        ggml_backend_free(backend);
    }
}
