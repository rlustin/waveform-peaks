# waveform-peaks

Constant-memory generation of BBC audiowaveform-compatible peak data (`.dat`
and JSON) from streamed audio. Draft.

## Design

```text
PcmSource ──next_block()──▶ PeakBuilder ──point()──▶ PeakSink
(OggOpusSource,             (min/max per             (DatWriter, JsonWriter,
 RawPcmSource)               output channel)          MemorySink)
```

- **`PcmSource`** returns one decoded block at a time, borrowed from its own
  buffer. `OggOpusSource` reads with plain `Read`, so an upload body can be
  decoded as it arrives.
- **`PeakBuilder`** keeps one `(min, max)` accumulator per output channel.
- **`PeakSink`** receives each point as it completes. `DatWriter` patches the
  length field at the end (needs `Seek`). `JsonWriter` writes `length` after
  `data`, so it needs no `Seek`.
- **`generate`** connects the three and enforces `Limits` (channel count,
  duration) as blocks arrive.

Points reach the output as they complete, so a failed run leaves a partial
file. Write to a temporary path and rename it on success.

```rust
let mut source = OggOpusSource::new(File::open("upload.opus")?)?;
let output = File::create("upload.dat")?;
generate(&mut source, &Options::default(), &Limits::default(), DatWriter::new(output))?;
```

The API is synchronous because decoding is CPU-bound. In an async server, run
`generate` with `spawn_blocking` and read the upload through `tokio-util`'s
`SyncIoBridge`; the crate docs have an example.

On a one-hour stereo Opus file, peak memory is 2 MB at zoom 256 and at zoom 32,
against 15 MB and 64 MB for audiowaveform 1.11.1, which keeps the output in
memory.

## Features

The default `opus` feature decodes Ogg Opus with libopus, which the `opus`
crate compiles from source: building it needs `cmake` and a C compiler. Without
it, only raw PCM input is available:

```toml
waveform-peaks = { version = "0.1", default-features = false }
```

## Compatibility with audiowaveform

These rules come from black-box probing of audiowaveform 1.11.1, not from its
source (it is GPL-3.0):

| Rule | Behaviour |
|---|---|
| Mixdown | Average of each frame, integer division truncating toward zero |
| 8-bit output | 16-bit peak divided by 256, truncating toward zero |
| `.dat` version | 1 for one output channel, 2 otherwise; JSON is always version 2 |
| Trailing partial block | Emitted as a point |
| Zoom | At least 2 |
| `--amplitude-scale` | No effect on `.dat`/JSON (images only), so not in the API |
| Opus decode rate | Smallest of 8/12/16/24/48 kHz at or above the `OpusHead` input rate, capped at 48 kHz |
| Opus samples | Float decode × 32767, rounded half to even; overshoot wraps (`Overflow::Wrap`) |
| Opus framing | Pre-skip dropped, end trimmed to the last granule position, output gain applied |

`Overflow::Saturate` is the default because wrapping makes clipped audio read
below full scale. Use `Overflow::Wrap` to match audiowaveform: within ±1 per
value with the bundled libopus (see Oracle tests).

## Development

```sh
cargo build
cargo test                             # tool-free tests
cargo test --test oracle -- --ignored  # comparison with audiowaveform
cargo clippy --all-targets -- -D warnings
cargo clippy --no-default-features --all-targets -- -D warnings
cargo fmt
cargo deny check                       # dependency licenses and advisories
```

Clippy runs twice so builds without the `opus` feature stay clean. `cargo deny
check` needs [cargo-deny](https://github.com/EmbarkStudios/cargo-deny).

### Oracle tests

The oracle tests need `audiowaveform` and `ffmpeg` with libopus on `PATH`, or
set `AUDIOWAVEFORM` and `FFMPEG`. They compare:

- raw PCM in 192 combinations of channels, length, zoom, bits, and mixdown or
  split, byte for byte;
- JSON output field by field;
- Ogg Opus fixtures encoded by ffmpeg at test time: mono, stereo with an odd
  length, a 16 kHz input, a 44.1 kHz input, patched `OpusHead` rate and gain,
  and a clipping square wave.

Opus comparisons allow ±1 per value. The `opus` crate bundles its own libopus
build, whose float output differs in the last bit from Homebrew's. Decoding
with Homebrew's libopus and this crate's conversion matches audiowaveform
exactly.

## Limitations

- Channel mapping family 1 (surround Opus) needs the multistream decoder.
- Chained Ogg files: only the first link is decoded.
- Ogg Skeleton: a file whose first page belongs to a Skeleton stream is
  rejected, since only the stream of the first page is decoded.
- Other formats (WAV, FLAC, MP3, Vorbis) through Symphonia. It is MPL-2.0,
  which `deny.toml` does not allow yet.
- Several zoom levels in one pass.
- A pure-Rust Opus decoder behind a feature flag.
- Fuzzing `OggOpusSource` on malformed uploads.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this crate by you, as defined in the Apache-2.0 license, shall
be dual licensed as above, without any additional terms or conditions.

Dependencies are limited to permissive licenses by `deny.toml`. The Ogg Opus
decoder builds libopus (BSD-3-Clause) into the binary: distributing a binary
means shipping its copyright notice, along with that of the `ogg` crate.
audiowaveform (GPL-3.0) is only run as a separate program by the ignored
oracle tests and is not part of the crate.
