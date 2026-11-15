#!/bin/zsh
# Puts MLX v0.32.2 and its build dependencies in target/deps-src, where build/offline.cmake points mlx-sys's cmake.
# Copies a local build's sources when one exists (no network), else clones the pinned tags from GitHub and downloads
# metal-cpp from the URL MLX's CMakeLists.txt pins.
set -e
cd "$(dirname "$0")/.."
DEPS=target/deps-src
mkdir -p $DEPS
# Local builds are looked for in the repositories beside this one (HEIMDALL_PROJECTS overrides; the folder that holds
# this checkout, so every worktree of it and its sibling projects are searched).
PROJECTS=${HEIMDALL_PROJECTS:-$(cd ../../../.. && pwd)}
LOCAL=""
for d in $PROJECTS/*/target/*/*/build/mlx-sys-*/out/build/_deps(N); do
  [ "$(git -C $d/mlx-src describe --tags 2>/dev/null)" = "v0.32.2" ] && LOCAL=$d && break
done
for n in mlx fmt json metal_cpp gguflib; do
  [ -d $DEPS/$n-src ] && continue
  if [ -n "$LOCAL" ] && [ -d "$LOCAL/$n-src" ]; then cp -cR "$LOCAL/$n-src" $DEPS/$n-src; continue; fi
  case $n in
    mlx) git clone --depth 1 --branch v0.32.2 https://github.com/ml-explore/mlx.git $DEPS/mlx-src ;;
    fmt) git clone --depth 1 --branch 12.1.0 https://github.com/fmtlib/fmt.git $DEPS/fmt-src ;;
    gguflib) git clone https://github.com/antirez/gguf-tools/ $DEPS/gguflib-src && git -C $DEPS/gguflib-src checkout 8fa6eb65236618e28fd7710a0fba565f7faa1848 ;;
    json) mkdir -p $DEPS/json-src && curl -L https://github.com/nlohmann/json/releases/download/v3.11.3/json.tar.xz | tar -xJ -C $DEPS/json-src --strip-components 1 ;;
    metal_cpp)
      # The archive MLX's own CMakeLists.txt names (METAL_CPP_URL), unpacked without its top folder as FetchContent would.
      url=$(grep -o 'https://developer.apple.com/metal/cpp/files/metal-cpp[^ )"]*\.zip' $DEPS/mlx-src/CMakeLists.txt | head -1)
      [ -n "$url" ] || { echo "metal_cpp: no METAL_CPP_URL in $DEPS/mlx-src/CMakeLists.txt" >&2; exit 1; }
      tmp=$(mktemp -d)
      curl -fL -o $tmp/metal-cpp.zip "$url"
      unzip -q $tmp/metal-cpp.zip -x '__MACOSX/*' -d $tmp/unpacked
      top=($tmp/unpacked/*(/N))
      [ ${#top} -eq 1 ] || { echo "metal_cpp: unexpected layout in $url" >&2; exit 1; }
      mv $top[1] $DEPS/metal_cpp-src
      rm -rf $tmp ;;
  esac
done
ls $DEPS
