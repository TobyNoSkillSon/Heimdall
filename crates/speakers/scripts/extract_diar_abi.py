#!/usr/bin/env python3
"""Split the pinned, unchanged standalone diarization C ABI out of upstream's ASR TU.
Avoid pulling SentencePiece's private protobuf into an app that statically links ONNX Runtime.
"""
from pathlib import Path
root=Path(__file__).resolve().parents[1]
s=(root/'vendor/nemo-speech/src/asr/c_api.cpp').read_text()
includes=s[:s.index('#include "recognizer.h"')]
structs=s[s.index('struct nemo_speech_diar_model {'):s.index('namespace {')]
helpers=s[s.index('thread_local std::string g_last_error;'):s.index('std::string\nstr_or_empty')]
guard=s[s.index('// Converts exceptions to status codes'):s.index('\n}  // namespace\n\nextern "C"')]
api=s[s.index('const char*\nnemo_speech_asr_last_error(void)'):]
output=includes+'\nnamespace asr_core = nemo_speech::asr;\n'+structs+'namespace {\n'+helpers+guard+'\n} // namespace\nextern "C" {\n'+api
p=root/'native/diar_api.cpp'
if '--check' in __import__('sys').argv:
 assert p.read_text()==output,'standalone ABI differs from its pinned extraction'
 print('standalone C ABI matches pinned upstream extraction')
else:p.write_text(output)
