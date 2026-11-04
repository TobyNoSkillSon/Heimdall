// The live d1 omni Conformer uses 512-point FFTs. Accelerate's CPU SIMD DFT
// keeps that exact grid and unscaled forward convention. No window, padding,
// pre-emphasis, mel filtering, normalization or sample is changed.
#include <Accelerate/Accelerate.h>
#include <array>
struct heimdall_fft512 {
    vDSP_DFT_Setup plan = vDSP_DFT_zop_CreateSetup(nullptr, 512, vDSP_DFT_FORWARD);
    std::array<float,512> imaginary{}, real_output{}, imaginary_output{};
    ~heimdall_fft512() { if(plan) vDSP_DFT_DestroySetup(plan); }
};
static bool heimdall_audio_fft(float * in, int n, float * out) {
    if(n != 512) return false;
    // The preprocessor runs four transient CPU threads. Each plan and all
    // mutable workspace belong to one thread and are freed when it exits.
    thread_local heimdall_fft512 fft;
    if(!fft.plan) return false;
    vDSP_DFT_Execute(fft.plan,in,fft.imaginary.data(),fft.real_output.data(),fft.imaginary_output.data());
    for(int i=0;i<512;++i) {out[2*i]=fft.real_output[i];out[2*i+1]=fft.imaginary_output[i];}
    return true;
}
