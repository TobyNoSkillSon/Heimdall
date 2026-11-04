// Offline contention fixture: one native model, six concurrent streams.
#include "diar/diarizer.h"
#include <atomic>
#include <cmath>
#include <thread>
extern "C" void heimdall_nemotron_test_inject(size_t n);
extern "C" int heimdall_nemotron_contention_fixture(const char* path) {
    try {
        auto model = nemo_speech::asr::Diarizer::load(0,path,nemo_speech::asr::DiarGeometry::preset("v3-streaming"));
        std::vector<float> audio(16000);
        for(size_t i=0;i<audio.size();++i) audio[i]=std::sin(i*.03)*.1;
        auto baseline = model->streaming_diarize();
        baseline->push(audio.data(),audio.size(),16000); baseline->finish();
        auto reference=baseline->frame_probabilities();
        std::atomic<int> ready{0},errors{0};
        std::vector<std::thread> threads;
        for(int i=0;i<6;++i) threads.emplace_back([&,i] {
            ++ready;
            while(ready.load()!=6) std::this_thread::yield();
            try {
                auto stream=model->streaming_diarize();
                if(i==2) heimdall_nemotron_test_inject(1);
                stream->push(audio.data(),audio.size(),16000); stream->finish();
                const auto& values=stream->frame_probabilities();
                if(values.size()!=reference.size()) { ++errors; return; }
                for(size_t j=0;j<values.size();++j) if(!std::isfinite(values[j]) || std::abs(values[j]-reference[j])>0.0001) { ++errors; break; }
            } catch(...) { ++errors; }
        });
        for(auto& thread:threads) thread.join();
        return errors.load();
    } catch(...) { return 99; }
}
