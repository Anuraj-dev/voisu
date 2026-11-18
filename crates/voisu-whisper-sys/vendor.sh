#!/usr/bin/env bash
# Refresh the vendored whisper.cpp tree from the pinned release tarball.
#
# The tree is committed so every build (cargo, RPM/COPR, deb, CI) works
# offline. Only the parts the worker builds are kept: whisper, ggml core and
# the CPU, Vulkan, Metal and BLAS backends. Bump TAG and SHA256 together.
set -euo pipefail

TAG=v1.9.5
SHA256=ff1a9053feb509ff9d7729703355541ae9690073a6b1c40eb692c962e0dc1720
URL="https://github.com/ggml-org/whisper.cpp/archive/refs/tags/${TAG}.tar.gz"

here="$(cd "$(dirname "$0")" && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

curl --fail --location --silent --show-error --output "$work/src.tar.gz" "$URL"
echo "${SHA256}  $work/src.tar.gz" | sha256sum --check --quiet
tar -xzf "$work/src.tar.gz" -C "$work"
src="$work/whisper.cpp-${TAG#v}"

dest="$here/whisper.cpp"
rm -rf "$dest"
mkdir -p "$dest/src" "$dest/ggml/src"
cp -a "$src/LICENSE" "$src/CMakeLists.txt" "$src/cmake" "$src/include" "$dest/"
cp -a "$src/src/CMakeLists.txt" "$src/src/whisper.cpp" "$src/src/whisper-arch.h" \
    "$src/src/parakeet.cpp" "$src/src/parakeet-arch.h" "$src/src/aneforge" "$dest/src/"
cp -a "$src/ggml/CMakeLists.txt" "$src/ggml/cmake" "$src/ggml/include" "$dest/ggml/"
find "$src/ggml/src" -maxdepth 1 -type f -exec cp -a {} "$dest/ggml/src/" \;
for backend in ggml-cpu ggml-vulkan ggml-metal ggml-blas; do
    cp -a "$src/ggml/src/$backend" "$dest/ggml/src/"
done
printf '%s\n' "whisper.cpp ${TAG} from ${URL}" "sha256 ${SHA256}" \
    "Trimmed by vendor.sh; see that script for what is kept." > "$dest/VENDORED"
