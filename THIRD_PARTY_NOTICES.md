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
