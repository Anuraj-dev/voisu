#!/usr/bin/env bash
# Builds the Local worker (voisu-local-asr.exe and the whisper.cpp DLLs it
# loads from its own directory) in the root workspace and stages it in
# src-tauri/local-asr/, which tauri.local-asr.conf.json bundles (ADR 0014).
# The model is never bundled; Settings downloads it on demand.
set -euo pipefail
cd "$(dirname "$0")"
root=../..
cargo build --manifest-path "$root/Cargo.toml" -p voisu-local-asr --release --locked
release="$root/target/release"
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
ls -l "$dest"
