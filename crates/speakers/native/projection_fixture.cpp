// Fixed no-model operation probe through the current GGML Metal entry point.
#include "ggml.h"
#include "ggml-backend.h"
#include "ggml-cpu.h"
#ifdef __APPLE__
#include "ggml-metal.h"
#endif
#include <cmath>
#include <vector>
extern "C" int heimdall_projection_fixture(int rows, int cols, int inner, int type, float * out) {
#ifdef __APPLE__
    auto backend=ggml_backend_metal_init();
    if (!backend) return 1;
    auto init=ggml_init_params{2*1024*1024,nullptr,true};
    auto wc=ggml_init(init),ctx=ggml_init(init);
    auto w=ggml_new_tensor_2d(wc,(ggml_type)type,inner,cols);
    ggml_set_name(w,"fixture.weight");
    auto wb=ggml_backend_alloc_ctx_tensors(wc,backend);
    auto x=ggml_new_tensor_2d(ctx,GGML_TYPE_F32,inner,rows);
    auto y=ggml_mul_mat(ctx,w,x);
    auto graph=ggml_new_graph(ctx);ggml_build_forward_expand(graph,y);
    auto buffer=ggml_backend_alloc_ctx_tensors(ctx,backend);
    std::vector<float> weights(inner*cols),input(inner*rows);
    for (size_t i=0;i<weights.size();i++) weights[i]=std::cos(i*.47)*.125;
    for (size_t i=0;i<input.size();i++) input[i]=std::sin(i*.91)*.75;
    std::vector<uint8_t> packed(ggml_nbytes(w));
    if (type==GGML_TYPE_Q8_0) ggml_quantize_chunk(GGML_TYPE_Q8_0,weights.data(),packed.data(),0,cols,inner,nullptr);
    else ggml_fp32_to_fp16_row(weights.data(),(ggml_fp16_t*)packed.data(),weights.size());
    ggml_backend_tensor_set(w,packed.data(),0,packed.size());
    ggml_backend_tensor_set(x,input.data(),0,input.size()*4);
    auto status=ggml_backend_graph_compute(backend,graph);
    if (!ggml_backend_metal_synchronize_checked(backend)) status=GGML_STATUS_FAILED;
    if(status==GGML_STATUS_SUCCESS)ggml_backend_tensor_get(y,out,0,rows*cols*4);
    ggml_backend_buffer_free(buffer);ggml_backend_buffer_free(wb);
    ggml_free(ctx);ggml_free(wc);ggml_backend_free(backend);
    return status==GGML_STATUS_SUCCESS ? 0 : 2;
#else
    return 3;
#endif
}
