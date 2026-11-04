cmake_minimum_required(VERSION 3.26)
project(heimdall_speakers_native LANGUAGES C CXX)
set(CMAKE_CXX_STANDARD 17)
set(CMAKE_POSITION_INDEPENDENT_CODE ON)
set(BUILD_SHARED_LIBS OFF CACHE BOOL "" FORCE)
set(GGML_LLAMAFILE ON CACHE BOOL "" FORCE)
set(VENDOR ${CMAKE_CURRENT_SOURCE_DIR}/../vendor)
add_subdirectory(${VENDOR}/ggml ggml EXCLUDE_FROM_ALL)
add_subdirectory(${VENDOR}/opus opus EXCLUDE_FROM_ALL)
set(NEMO ${VENDOR}/nemo-speech)
file(GLOB SRC
    ${NEMO}/src/asr/features/*.cpp
    ${NEMO}/src/asr/encoder/*.cpp
    ${NEMO}/src/asr/diar/*.cpp
    ${NEMO}/src/common/*.cpp
    ${NEMO}/src/runtime/ggml/*.cpp)
add_library(heimdall_nemo STATIC ${SRC} diar_api.cpp abi.cpp projection_fixture.cpp fault_fixture.cpp)
target_compile_definitions(heimdall_nemo PRIVATE NEMO_SPEECH_ASR_BUILD NEMO_SPEECH_VERSION_STR="0.2.0-heimdall-pinned")
target_include_directories(heimdall_nemo PRIVATE
    ${CMAKE_CURRENT_SOURCE_DIR}
    ${NEMO}/include ${NEMO}/src/asr ${NEMO}/src/common
    ${NEMO}/src/runtime/ggml)
foreach(DIR features encoder decoders vad diar postproc pnc)
    target_include_directories(heimdall_nemo PRIVATE ${NEMO}/src/asr/${DIR})
endforeach()
target_link_libraries(heimdall_nemo PRIVATE ggml ggml-cpu ggml-base opus)
install(TARGETS heimdall_nemo ggml ggml-cpu ggml-base ARCHIVE DESTINATION lib)
if(TARGET ggml-metal)
    install(TARGETS ggml-metal ARCHIVE DESTINATION lib)
endif()

install(TARGETS opus ARCHIVE DESTINATION lib)
