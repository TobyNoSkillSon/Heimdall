# Points mlx-c's FetchContent at local copies of MLX v0.32.2 and its dependencies (engine/target/deps-src),
# so building mlx-sys needs no network. Refresh with build/fetch-deps.sh if target/ is cleaned.
get_filename_component(_D1E_ROOT "${CMAKE_CURRENT_LIST_DIR}/.." ABSOLUTE)
set(_D1E_DEPS "${_D1E_ROOT}/target/deps-src")
set(FETCHCONTENT_SOURCE_DIR_MLX "${_D1E_DEPS}/mlx-src" CACHE PATH "" FORCE)
set(FETCHCONTENT_SOURCE_DIR_FMT "${_D1E_DEPS}/fmt-src" CACHE PATH "" FORCE)
set(FETCHCONTENT_SOURCE_DIR_JSON "${_D1E_DEPS}/json-src" CACHE PATH "" FORCE)
set(FETCHCONTENT_SOURCE_DIR_METAL_CPP "${_D1E_DEPS}/metal_cpp-src" CACHE PATH "" FORCE)
set(FETCHCONTENT_SOURCE_DIR_GGUFLIB "${_D1E_DEPS}/gguflib-src" CACHE PATH "" FORCE)
