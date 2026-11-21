#!/usr/bin/env bash
# Builds the Local worker (voisu-local-asr.exe and the whisper.cpp DLLs it
# loads from its own directory) in the root workspace and stages it in
# src-tauri/local-asr/, which tauri.local-asr.conf.json bundles (ADR 0014).
# The model is never bundled; Settings downloads it on demand.
#
# The worker is built with ggml's Vulkan backend, so ggml-vulkan.dll ships
# beside the CPU modules and the worker uses a GPU when the PC has one. The
# build needs the Vulkan SDK (VULKAN_SDK and glslc on PATH; CI installs it
# with install-vulkan-sdk.ps1).
#
# The worker and the DLLs link the static C runtime (`+crt-static`, and /MT
# through voisu-whisper-sys/build.rs), so a PC without the Visual C++
# Redistributable runs them. The flag is set for the target only: with
# `--target`, build scripts keep the host's default runtime.
set -euo pipefail
cd "$(dirname "$0")"
root=../..
target=x86_64-pc-windows-msvc
RUSTFLAGS="${RUSTFLAGS:-} -C target-feature=+crt-static" \
  cargo build --manifest-path "$root/Cargo.toml" -p voisu-local-asr --features vulkan --release --locked --target "$target"
release="$root/target/$target/release"
dest=src-tauri/local-asr
rm -rf "$dest"
mkdir -p "$dest"
cp "$release/voisu-local-asr.exe" "$dest/"
while IFS= read -r name; do
  if [ -n "$name" ]; then
    cp "$release/$name" "$dest/"
  fi
done < "$release/voisu-whisper-runtime.txt"
test -s "$dest/whisper.dll"
test -s "$dest/ggml-vulkan.dll"
# whisper.cpp and ggml are MIT: their notice ships with their DLLs.
cp "$root/crates/voisu-whisper-sys/whisper.cpp/LICENSE" "$dest/LICENSE-whisper.cpp.txt"
ls -l "$dest"
