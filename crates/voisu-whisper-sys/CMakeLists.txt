# Builds the vendored whisper.cpp with dynamically loaded ggml backends, plus
# the Voisu shim as a static library.
cmake_minimum_required(VERSION 3.14)
project(voisu_whisper C CXX)

# GGML_BACKEND_DL needs shared libraries: libwhisper, libggml and libggml-base
# are linked; each backend (libggml-cpu-<variant>.so, libggml-vulkan.so) is a
# module ggml dlopen()s from the worker's own directory.
set(BUILD_SHARED_LIBS ON CACHE BOOL "" FORCE)
set(CMAKE_POSITION_INDEPENDENT_CODE ON)
set(WHISPER_BUILD_TESTS OFF CACHE BOOL "" FORCE)
set(WHISPER_BUILD_EXAMPLES OFF CACHE BOOL "" FORCE)
set(WHISPER_BUILD_SERVER OFF CACHE BOOL "" FORCE)
set(WHISPER_CURL OFF CACHE BOOL "" FORCE)
set(WHISPER_SDL2 OFF CACHE BOOL "" FORCE)
set(WHISPER_ALL_WARNINGS OFF CACHE BOOL "" FORCE)
# The vendored tree is the tagged release, not a nightly.
set(WHISPER_BUILD_IS_DEV OFF CACHE BOOL "" FORCE)
set(GGML_BUILD_TESTS OFF CACHE BOOL "" FORCE)
set(GGML_BUILD_EXAMPLES OFF CACHE BOOL "" FORCE)
# ggml's own thread pool; avoids a libgomp runtime dependency.
set(GGML_OPENMP OFF CACHE BOOL "" FORCE)
# ggml stamps `git rev-parse` of the enclosing checkout (the Voisu repo, plus
# "-dirty") into ggml-version.h. An empty GIT_EXE stops ggml's find_program
# and git calls; the stamp names the vendored release instead, so builds are
# reproducible. whisper.cpp's own build info only reaches its CMake package
# files, never the binary, but is pinned the same way.
set(GIT_EXE "" CACHE FILEPATH "" FORCE)
set(GGML_BUILD_COMMIT "whisper.cpp-v1.9.5" CACHE STRING "" FORCE)
set(WHISPER_BUILD_COMMIT "v1.9.5" CACHE STRING "" FORCE)
set(WHISPER_BUILD_NUMBER 0 CACHE STRING "" FORCE)

add_subdirectory(whisper.cpp)

add_library(voisu_whisper_shim STATIC shim.c)
target_link_libraries(voisu_whisper_shim PRIVATE whisper ggml)
install(TARGETS voisu_whisper_shim ARCHIVE DESTINATION lib)
