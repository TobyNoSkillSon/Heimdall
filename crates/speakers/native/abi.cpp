#include <cstddef>
#include "nemo_speech/diar.h"
extern "C" size_t heimdall_diar_config_size() { return sizeof(nemo_speech_diar_model_config); }
extern "C" size_t heimdall_diar_config_preset_offset() { return offsetof(nemo_speech_diar_model_config,preset); }
extern "C" size_t heimdall_diar_segment_size() { return sizeof(nemo_speech_diar_segment); }
