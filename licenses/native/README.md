# Native libraries of twr's own voices

twr built with the `neural` feature of `twr-cli` (the default; [read-aloud.md](../../docs/read-aloud.md#twrs-own-voices))
links sherpa-onnx's prebuilt static libraries (`sherpa-onnx-sys` 2.0.1 downloads them when
it builds). They are C and C++ code, which cargo-about doesn't see, so their licences are
kept here:

- [`components.json`](components.json) lists each library, its version, licence and source,
  and the text files that hold its licence and notices.
- [`section.mjs`](section.mjs) renders the list as a section of `THIRD-PARTY-LICENSES.html`.
  The release workflow adds it for the macOS archives, which are built with the feature. The
  desktop app's `clients/scripts/notices.mjs` adds it too. The static Linux binaries are
  built without the feature and contain none of these libraries.

The libraries are those `sherpa-onnx-sys` links (`SHERPA_ONNX_STATIC_LIBS` in its build
script): sherpa-onnx's own, ONNX Runtime, kaldi-native-fbank (with KISS FFT), kaldi-decoder
(with kaldifst, OpenFst and Eigen), simple-sentencepiece and utf8proc, and the header-only
libraries sherpa-onnx builds in (nlohmann/json, hclust-cpp). Versions are those sherpa-onnx
2.0.1's CMake files pin.

When `sherpa-onnx` is updated, check its `cmake/*.cmake` (and kaldi-decoder's and
kaldi-native-fbank's) for changed versions or new libraries, and update the texts and
`components.json` with it.

Not distributed with twr, and so not listed:

- **espeak-ng** (GPL-3.0): twr runs the program you installed, never links it.
- **The voice models**: `twr voices install` downloads them from sherpa-onnx's releases, with
  their own licence files, which twr keeps in each model's directory
  (`<data>/speech/<model>/LICENSE`). Kokoro is Apache-2.0, Piper's voices MIT (per their model
  cards), and Supertonic 3's model OpenRAIL-M, whose use restrictions (no impersonation, among
  others) bind whoever uses it.
