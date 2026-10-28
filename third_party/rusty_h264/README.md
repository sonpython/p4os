# rusty_h264 (vendored)

Pure-Rust H.264 decoder used by the YouTube player (`ytplay/`).

| | |
|---|---|
| Upstream | https://github.com/remade-with-rust/rusty_h264 (crates.io `rusty_h264-decoder` and `rusty_h264-common`) |
| Version | 0.16.1, pinned |
| Licence | BSD-2-Clause, Copyright (c) 2026, Mata Network: see [LICENSE](LICENSE) |

Why vendored: the crate is young (first release 2026-06) and changes every
few weeks, so p4os keeps a known-good copy that was checked bit-exact against
ffmpeg on YouTube 144p/240p streams, on the host and on the board
(`yt bench`, frame CRC).

Changes from the published crates:

- `src/` is unchanged except `common/src/lib.rs`, where the optional
  `global-alloc` allocator (from `rusty_alloc-api`) is removed.
- The manifests are rewritten without examples, tests, dev-dependencies and
  the optional `rusty_h264-accel` (x86-64/aarch64 SIMD) and `rusty_alloc-api`
  dependencies. `common/build.rs` never enables the SIMD cfgs, so the decoder
  always uses its portable scalar path (the only one RISC-V can use anyway).
- Default features are off: p4os builds it `no_std` with `libm`.
