# Third-party notices

Voisu is MIT licensed (see `LICENSE`). Code, algorithms or data adapted from
other projects keep their attribution here and in the adapting source. Each
keeps its own license.

## VoxType

- Used in: `crates/voisu-core/src/speech_gate.rs` (the adaptive energy
  voice-activity rule: 20 ms RMS frames, noise-floor threshold with the
  `.min(thr)` cap, three-frame minimum run, from `src/vad/energy.rs`)
- Used in: `crates/voisu-core/src/vocabulary/phonetic.rs` (phonetic
  Dictionary matching, adapted from `src/text/vocabulary.rs`)
- Source: <https://github.com/peteonrails/voxtype>
- Commit: `fbedd62`
- License: MIT

```text
MIT License

Copyright (c) 2025 Peter Jackson

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## wikipedia-word-frequency

- Used in: `crates/voisu-core/src/vocabulary/english-words.txt` (the 30,000
  most frequent words, lowercase ASCII, apostrophes removed)
- Source: <https://github.com/IlyaSemenov/wikipedia-word-frequency>,
  `results/enwiki-2023-04-13.txt`
- Commit: `d11c4fab211156e96ab5943850dd6f98604707fb`
- License: MIT

```text
MIT License

Copyright (c) 2015 Ilya Semenov

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## Silero VAD

- Used in: `crates/voisu-app/assets/silero_vad_v6.onnx` (the Silero VAD v6
  network, embedded unmodified) and `crates/voisu-app/src/local_vad.rs` (its
  inference graph, rewritten in Rust)
- Source: <https://github.com/snakers4/silero-vad>
- License: MIT

```text
MIT License

Copyright (c) 2020-present Silero Team

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## faster-whisper

- Used in: `crates/voisu-app/assets/silero_vad_v6.onnx` (faster-whisper's ONNX
  export of Silero VAD v6, `faster_whisper/assets/silero_vad_v6.onnx` from the
  `faster-whisper==1.2.1` wheel, sha256
  `4cbf549b8326f60f80f2536d9eefeb450a9abe83365a098031c89719f1be17d2`)
- Source: <https://github.com/SYSTRAN/faster-whisper>
- Version: `1.2.1`
- License: MIT

```text
MIT License

Copyright (c) 2023 SYSTRAN

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## whisper.cpp and ggml

- Used in: `crates/voisu-whisper-sys/whisper.cpp/` (vendored, trimmed by
  `crates/voisu-whisper-sys/vendor.sh`), built into `libwhisper.so.1`,
  `libggml.so.0`, `libggml-base.so.0` and the `libggml-cpu-*.so` and
  `libggml-vulkan.so` backend modules that ship beside the
  `voisu-local-asr` worker in `/usr/libexec/voisu/`. ggml is part of the same
  tree and carries the same license.
- Source: <https://github.com/ggml-org/whisper.cpp>, release tarball
  `v1.9.5` (sha256
  `ff1a9053feb509ff9d7729703355541ae9690073a6b1c40eb692c962e0dc1720`)
- License: MIT

```text
MIT License

Copyright (c) 2023-2026 The ggml authors

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## Test audio

- `crates/voisu-app/tests/fixtures/jfk-3s.wav`: the first three seconds of
  whisper.cpp's `samples/jfk.wav` (commit
  `c44b60b8053bbf2a5c1e014f11323fb3f2485177`), from John F. Kennedy's 1961
  inaugural address, a US government work in the public domain. Test-only;
  not shipped in the binaries.
