# waveform-peaks

Constant-memory generation of BBC audiowaveform-compatible peak data (`.dat`
and JSON) from streamed audio.

## Design

```text
PcmSource ──next_block()──▶ PeakBuilder ──point()──▶ PeakSink
(OggOpusSource,             (min/max per             (DatWriter, JsonWriter,
 WebmOpusSource,             output channel)          MemorySink)
 Mp4AacSource,
 AdtsAacSource,
 RawPcmSource)
```

- **`PcmSource`** returns one decoded block at a time, borrowed from its own
  buffer. `OggOpusSource` and `WebmOpusSource` read with plain `Read`, so an
  upload body can be decoded as it arrives, and so do the AAC readers.
  `WebmOpusSource` also reads Matroska, and browser `MediaRecorder` recordings,
  whose clusters have no size. `Mp4AacSource` reads fragmented MP4, as Safari
  records, and MP4 with the `moov` box first.
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

On a one-hour stereo Opus file, in Ogg or WebM, peak memory is 2 MB at zoom 256
and at zoom 32, against 15 MB and 64 MB for audiowaveform 1.11.1, which keeps
the output in memory.

## Features

The default `opus` feature decodes Opus from Ogg and WebM with libopus, which
the `opus` crate compiles from source: building it needs `cmake` and a C
compiler. Without it, only raw PCM input is available:

```toml
waveform-peaks = { version = "0.1", default-features = false }
```

The optional `aac` feature decodes AAC-LC from MP4 and ADTS with Symphonia's
pure-Rust decoder. Symphonia is MPL-2.0 (see License), so the feature is off
by default:

```toml
waveform-peaks = { version = "0.1", features = ["aac"] }
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

audiowaveform does not read WebM. `WebmOpusSource` takes the pre-skip from
`CodecDelay` and the end trimming from `DiscardPadding`, and on the same packets
gives exactly the output of `OggOpusSource`.

audiowaveform does not read AAC either, so AAC output is compared with FFmpeg's
decoder instead, within ±1 per sample. The encoder priming and padding are
trimmed using the MP4 edit list, or Apple's `iTunSMPB` tag without one; FFmpeg
trims the priming only. ADTS records neither, so nothing is trimmed. Symphonia
decodes AAC-LC only: HE-AAC decodes as its AAC-LC core, at half the sample rate
and without the top of the spectrum.

`Overflow::Saturate` is the default because wrapping makes clipped audio read
below full scale. Use `Overflow::Wrap` to match audiowaveform: within ±1 per
value with the bundled libopus (see Oracle tests).

## Development

```sh
cargo build
cargo test                             # tool-free tests
cargo test --all-features              # with AAC
cargo test --test oracle -- --ignored  # comparison with audiowaveform
cargo clippy --all-targets -- -D warnings
cargo clippy --no-default-features --all-targets -- -D warnings
cargo clippy --all-features --all-targets -- -D warnings
cargo fmt
cargo deny check                       # dependency licenses and advisories
cargo +nightly fuzz run ogg_opus       # also webm_opus, mp4_aac, adts_aac
```

GitHub Actions runs all of these except the oracle tests on every push and pull
request, with the tests also on Rust 1.88 and fuzzing limited to one minute per
target. Clippy runs three times so every feature combination stays clean. `cargo
deny check` needs [cargo-deny](https://github.com/EmbarkStudios/cargo-deny).
Fuzzing needs a nightly toolchain and
[cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz). Seed
`fuzz/corpus/<target>` with small files, each prefixed with one byte that the
target reads as options.

CI and longer runs use `-rss_limit_mb=2048 -malloc_limit_mb=64`: memory use
under the sanitizer grows past 512 MB without a leak, while any single
allocation over 64 MB still fails. For runs longer than CI's minute, also skip
`-fork`, whose temporary directories fill the disk.

### Oracle tests

The oracle tests need `audiowaveform` and `ffmpeg` with libopus on `PATH`, or
set `AUDIOWAVEFORM` and `FFMPEG`. They compare:

- raw PCM in 192 combinations of channels, length, zoom, bits, and mixdown or
  split, byte for byte;
- JSON output field by field;
- Ogg Opus fixtures encoded by ffmpeg at test time: mono, stereo with an odd
  length, a 16 kHz input, a 44.1 kHz input, patched `OpusHead` rate and gain,
  and a clipping square wave;
- the same fixtures remuxed to WebM, with known and unknown segment sizes:
  exactly equal to this crate's Ogg output, and to audiowaveform's within the
  Opus tolerance;
- AAC encoded by FFmpeg as ADTS, fast-start MP4 and fragmented MP4, and by
  Apple's `afconvert` when available, against FFmpeg's decoder.

Opus comparisons allow ±1 per value. The `opus` crate bundles its own libopus
build, whose float output differs in the last bit from Homebrew's. Decoding
with Homebrew's libopus and this crate's conversion matches audiowaveform
exactly.

## Limitations

- Channel mapping family 1 (surround Opus) needs the multistream decoder.
- Chained Ogg files: only the first link is decoded.
- Ogg Skeleton: a file whose first page belongs to a Skeleton stream is
  rejected, since only the stream of the first page is decoded.
- Other formats (WAV, FLAC, MP3, Vorbis) through Symphonia, whose format
  crates would each need an MPL-2.0 exception in `deny.toml`.
- MP4 with its media data before the `moov` box, as FFmpeg and phones write by
  default: it needs a `Read + Seek` constructor for files spooled to disk.
- HE-AAC's SBR and parametric stereo layers, which Symphonia does not decode.
- AAC in Matroska or WebM, and Opus in MP4 (Chrome can record both).
- Matroska beyond what recorders write: compressed or encrypted tracks, and
  files with several segments (only the first is decoded).
- Several zoom levels in one pass.
- A pure-Rust Opus decoder behind a feature flag.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this crate by you, as defined in the Apache-2.0 license, shall
be dual licensed as above, without any additional terms or conditions.

Dependencies are limited to permissive licenses by `deny.toml`, except for
Symphonia's crates under the optional `aac` feature. They are MPL-2.0, a
file-level copyleft that covers Symphonia's own files only: a binary that
includes them must say where their source is available, and changes to those
files must be shared under MPL-2.0. The Opus decoder builds libopus
(BSD-3-Clause) into the binary: distributing a binary means shipping its
copyright notice, along with that of the `ogg` crate. audiowaveform (GPL-3.0)
is only run as a separate program by the ignored oracle tests and is not part
of the crate.
