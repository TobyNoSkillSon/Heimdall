#!/bin/sh
# Agent-only reproducible native build; no network at application runtime.
set -eu
project=$(CDPATH= cd -- "$(dirname -- "$0")/../../.." && pwd)
source_dir="$project/crates/decide/ref/llama.cpp"
revision=62a6f74bab748b1755f4cd8dfc0871caf865c6c8
if [ ! -d "$source_dir/.git" ]; then
  git init "$source_dir"
  git -C "$source_dir" remote add origin https://github.com/ggml-org/llama.cpp.git
  git -C "$source_dir" fetch --depth 1 origin "$revision"
  git -C "$source_dir" checkout --detach FETCH_HEAD
fi
[ "$(git -C "$source_dir" rev-parse HEAD)" = "$revision" ]
native_build="${CARGO_TARGET_DIR:-$project/target/decide}/native"
case "$native_build" in /*) ;; *) native_build="$project/$native_build" ;; esac
cmake -S "$project/crates/decide/native" -B "$native_build" -DCMAKE_BUILD_TYPE=Release -DLLAMA_SOURCE="$source_dir" -DGGML_METAL=ON -DGGML_ACCELERATE=ON
cmake --build "$native_build" --target heimdall_decide -j 4
runtime_dir="${HEIMDALL_D1_RUNTIME_DIR:-$project/models/decide/runtime}"
mkdir -p "$runtime_dir"
cp "$native_build/libheimdall_decide.dylib" "$runtime_dir/libheimdall_decide.dylib.tmp"
mv -f "$runtime_dir/libheimdall_decide.dylib.tmp" "$runtime_dir/libheimdall_decide.dylib"
