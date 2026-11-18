# Builds the vendored whisper.cpp plus the Voisu shim as static libraries.
cmake_minimum_required(VERSION 3.14)
project(voisu_whisper C CXX)

set(BUILD_SHARED_LIBS OFF CACHE BOOL "" FORCE)
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

add_subdirectory(whisper.cpp)

add_library(voisu_whisper_shim STATIC shim.c)
target_link_libraries(voisu_whisper_shim PRIVATE whisper)
install(TARGETS voisu_whisper_shim ARCHIVE DESTINATION lib)
