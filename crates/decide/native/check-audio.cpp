// Run this executable in separate baseline/candidate processes (never load two
// native d1 builds in one process). Dump the complete front-end tensor for parity.
#include "mtmd-audio.h"
#include <cstdio>
#include <cmath>
#include <fstream>
#include <chrono>
int main(int argc,char ** argv) {
    if(argc!=2)return 2;
    clip_hparams hp;
    hp.n_mel_bins=128;hp.audio_n_fft=512;hp.audio_window_len=400;hp.audio_hop_len=160;hp.audio_sample_rate=16000;
    mtmd_audio_preprocessor_conformer frontend(hp);frontend.initialize();
    std::ofstream out(argv[1],std::ios::binary);
    auto start=std::chrono::steady_clock::now();
    for(int n : {1600,48000,176640,480000}) {
        std::vector<float> x(n);
        for(int i=0;i<n;++i)x[i]=.3f*std::sin(float(i)*.01f)+.1f*std::sin(float(i)*.191f);
        x[n/2]=.9f; // exercise a sharp event as well as low-frequency content
        std::vector<mtmd_audio_mel> mel;
        if(!frontend.preprocess(x.data(),x.size(),mel))return 3;
        for(const auto & m : mel)out.write(reinterpret_cast<const char *>(m.data.data()),m.data.size()*4);
    }
    printf("CPU frontend fixtures ms=%g\n",std::chrono::duration<double,std::milli>(std::chrono::steady_clock::now()-start).count());
}
