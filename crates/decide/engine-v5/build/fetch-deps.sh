#!/bin/zsh
# Puts MLX v0.32.2 and its build dependencies in target/deps-src, where build/offline.cmake points mlx-sys's cmake.
# Copies a local build's sources when one exists (no network), else clones the pinned tags from GitHub.
set -e
cd "$(dirname "$0")/.."
DEPS=target/deps-src
mkdir -p $DEPS
LOCAL=$(ls -d /Users/user/Work/Projects/*/target/*/*/build/mlx-sys-*/out/build/_deps 2>/dev/null | while read d; do
  [ "$(git -C $d/mlx-src describe --tags 2>/dev/null)" = "v0.32.2" ] && echo $d && break; done)
for n in mlx fmt json metal_cpp gguflib; do
  [ -d $DEPS/$n-src ] && continue
  if [ -n "$LOCAL" ] && [ -d "$LOCAL/$n-src" ]; then cp -cR "$LOCAL/$n-src" $DEPS/$n-src; continue; fi
  case $n in
    mlx) git clone --depth 1 --branch v0.32.2 https://github.com/ml-explore/mlx.git $DEPS/mlx-src ;;
    fmt) git clone --depth 1 --branch 12.1.0 https://github.com/fmtlib/fmt.git $DEPS/fmt-src ;;
    gguflib) git clone https://github.com/antirez/gguf-tools/ $DEPS/gguflib-src && git -C $DEPS/gguflib-src checkout 8fa6eb65236618e28fd7710a0fba565f7faa1848 ;;
    json) mkdir -p $DEPS/json-src && curl -L https://github.com/nlohmann/json/releases/download/v3.11.3/json.tar.xz | tar -xJ -C $DEPS/json-src --strip-components 1 ;;
    metal_cpp) echo "metal_cpp: copy from an MLX build's _deps/metal_cpp-src (URL in mlx-src/CMakeLists.txt METAL_CPP_URL)"; exit 1 ;;
  esac
done
ls $DEPS
