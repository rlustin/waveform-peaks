# waveform-peaks

Generates BBC audiowaveform-compatible peak data (`.dat` and JSON) from
streamed audio, with constant memory use.

## Usage

```rust
let mut source = OggOpusSource::new(File::open("upload.opus")?)?;
let output = File::create("upload.dat")?;
generate(&mut source, &Options::default(), &Limits::default(), DatWriter::new(output))?;
```

`generate` checks each block against `Limits` (channel count and duration)
when the block arrives.

`generate` writes each point as soon as the point is complete. If the run
fails, the output file is incomplete. Thus, write to a temporary path, and
rename the file only when `generate` succeeds.

`DatWriter` needs an output that implements `Seek`, because it writes the
length field of the header last. `JsonWriter` writes `length` after `data`, so
any `Write` output is sufficient.

Decoding is CPU-bound, so the API is synchronous. In an async server, run
`generate` with `spawn_blocking`, and read the upload through the
`SyncIoBridge` of `tokio-util`. The crate documentation has an example.

## Supported input

| Source | Container | Codec | Feature |
|---|---|---|---|
| `OggOpusSource` | Ogg | Opus | `opus` (default) |
| `WebmOpusSource` | WebM, Matroska | Opus | `opus` (default) |
| `Mp4Source` | MP4 | Opus | `opus` (default) |
| `Mp4Source` | MP4 | AAC-LC | `aac` |
| `AdtsAacSource` | ADTS | AAC-LC | `aac` |
| `Mp4Source` | MP4 | MP1, MP2, MP3 | `mp3` |
| `MpegAudioSource` | MPEG audio (`.mp3`, `.mp2`) | MP1, MP2, MP3 | `mp3` |
| `RawPcmSource` | none | Interleaved signed 16-bit little-endian PCM | none |

Each source reads from a plain `Read`. Thus a source can decode an upload body
while the upload continues.

`WebmOpusSource` also reads browser `MediaRecorder` recordings, which have
clusters of unknown size. `Mp4Source::new` reads fragmented MP4, which Safari
and Chrome record, and MP4 that has the `moov` box first. The tests include
real recordings from Chrome 155 and Safari 27: Opus in WebM, and Opus and AAC
in MP4.

FFmpeg and phones write the media data before the `moov` box by default. To
read this layout, use `Mp4Source::new_seekable` on an input that implements
`Seek`, for example an upload saved to disk. `new_seekable` also reads the two
other layouts.

## Memory use

Peak memory use for a one-hour stereo Opus file, in Ogg or WebM:

| Zoom | waveform-peaks | audiowaveform 1.11.1 |
|---|---|---|
| 256 | 2 MB | 15 MB |
| 32 | 2 MB | 64 MB |

audiowaveform keeps all of the output in memory, so its memory use increases
when the zoom decreases.

## Features

The default `opus` feature decodes Opus from Ogg, WebM and MP4 with libopus.
The `opus` crate compiles libopus from source, so the build needs `cmake` and a
C compiler. With no features, only raw PCM input is available:

```toml
waveform-peaks = { version = "0.1", default-features = false }
```

The optional `aac` feature decodes AAC-LC from MP4 and ADTS with Symphonia's
pure-Rust decoder. Symphonia has the MPL-2.0 license (see License), so this
feature is off by default:

```toml
waveform-peaks = { version = "0.1", features = ["aac"] }
```

The optional `mp3` feature decodes MPEG audio from MP4 and from MPEG audio
streams with Symphonia's pure-Rust decoder. MPEG audio here means MPEG-1,
MPEG-2 and MPEG-2.5 audio, layers I to III (MP1, MP2 and MP3). This feature
also uses Symphonia, so it is off by default:

```toml
waveform-peaks = { version = "0.1", features = ["mp3"] }
```

## Compatibility with audiowaveform

The rules below come from black-box tests of audiowaveform 1.11.1. They do not
come from the audiowaveform source code, which has the GPL-3.0 license.

| Rule | Behaviour |
|---|---|
| Mixdown | Average of each frame, integer division truncating toward zero |
| 8-bit output | 16-bit peak divided by 256, truncating toward zero |
| `.dat` version | 1 for one output channel, 2 otherwise; JSON is always version 2 |
| Trailing partial block | Written as a point |
| Zoom | At least 2 |
| `--amplitude-scale` | Changes images only, not `.dat` or JSON, so the API does not have it |
| Opus decode rate | The smallest of 8, 12, 16, 24 and 48 kHz that is at or above the `OpusHead` input rate; 48 kHz at most |
| Opus samples | Float output × 32767, rounded half to even; values out of range wrap (`Overflow::Wrap`) |
| Opus framing | Pre-skip trimmed, end trimmed to the last granule position, output gain applied |
| MP3 framing | Frame with a Xing or Info tag not decoded; encoder delay + 529 frames trimmed if the LAME extension names `LAME` or `Lavc`; end padding kept |
| MPEG audio samples | Float output × 32768, rounded down; values at or below -1.0 clip to -32767 |
| Cut MPEG audio stream | Incomplete last frame dropped |

audiowaveform does not read Opus in WebM or MP4. Thus the tests compare these
sources with `OggOpusSource` on the same packets:

- `WebmOpusSource` gets the pre-skip from `CodecDelay` and the end trimming
  from `DiscardPadding`. Its output is the same as the output of
  `OggOpusSource`.
- `Mp4Source` trims by the edit list, as the Opus in ISOBMFF specification
  tells. If there is no edit list, `Mp4Source` trims by the `dOps` pre-skip.
- FFmpeg writes the edit list length in milliseconds. Thus the end of an
  FFmpeg MP4 file can differ from its Ogg source by up to 1 ms. A fragmented
  MP4 file from FFmpeg also keeps the end padding (less than one packet).

audiowaveform does not read AAC. Thus the tests compare AAC output with the
FFmpeg decoder, with a tolerance of ±1 per sample:

- `Mp4Source` trims the encoder priming and padding by the MP4 edit list. If
  there is no edit list, `Mp4Source` uses the Apple `iTunSMPB` tag. FFmpeg
  trims only the priming.
- An ADTS stream records neither value, so `AdtsAacSource` trims nothing.
- Symphonia decodes only AAC-LC. For HE-AAC input, Symphonia decodes only the
  AAC-LC core. The output has half the sample rate, and does not have the
  upper part of the spectrum.

audiowaveform decodes MPEG audio with libmad, which uses fixed-point
arithmetic. Symphonia uses floating-point arithmetic. Thus the values from
`MpegAudioSource` can differ by ±1 from the audiowaveform output. The length
of the output is the same.

audiowaveform does not read MPEG audio in MP4. Thus the tests compare
`Mp4Source` with the FFmpeg decoder, with a tolerance of ±1 per sample:

- `Mp4Source` trims by the MP4 edit list. FFmpeg trims only the start.
- FFmpeg writes an edit list for MP2 in MP4. This edit list ends 481 frames
  before the end of the input.
- An MPEG audio stream does not record the MP2 encoder delay. Thus
  `MpegAudioSource` trims nothing from MP2, as audiowaveform does.

The default is `Overflow::Saturate`, because with wrapping, clipped audio
shows as less than full scale. To match audiowaveform, use `Overflow::Wrap`.
With the bundled libopus, the output is then within ±1 per value of the
audiowaveform output (see Oracle tests).

## Development

```sh
cargo build
cargo test                             # tests without external tools
cargo test --all-features              # with all features
cargo test --no-default-features --features aac
cargo test --no-default-features --features mp3
cargo test --test oracle --all-features -- --ignored  # comparison with audiowaveform
cargo clippy --all-targets -- -D warnings
cargo clippy --no-default-features --all-targets -- -D warnings
cargo clippy --no-default-features --features aac --all-targets -- -D warnings
cargo clippy --no-default-features --features mp3 --all-targets -- -D warnings
cargo clippy --all-features --all-targets -- -D warnings
cargo fmt
cargo deny check                       # dependency licenses and advisories
cargo +nightly fuzz run ogg_opus       # also webm_opus, mp4, adts_aac, mpeg_audio
```

GitHub Actions runs all of these commands on each push and pull request.
The tests run on Linux, macOS and Windows. CI also runs the tests on
Rust 1.88, and runs each fuzz target for one minute. Tests and Clippy run
on five feature combinations (default, none, `aac` only, `mp3` only, and
all), so that each combination stays clean.

`cargo deny check` needs [cargo-deny](https://github.com/EmbarkStudios/cargo-deny).
Fuzzing needs a nightly toolchain and
[cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz). To seed
`fuzz/corpus/<target>`, use small files. Put one byte at the start of each
file: the target reads this byte as options.

CI and longer runs use `-rss_limit_mb=2048 -malloc_limit_mb=64`. The RSS limit
is high because memory use under the sanitizer increases past 512 MB without a
leak. The allocation limit still stops any single allocation of more than
64 MB. For runs longer than one minute, do not use `-fork`: the temporary
directories of `-fork` fill the disk.

### Oracle tests

The oracle tests need `audiowaveform`, and `ffmpeg` with libopus and
libmp3lame, on `PATH`.
Alternatively, set `AUDIOWAVEFORM` and `FFMPEG` to the paths of the two
programs. CI installs `audiowaveform` from the
[audiowaveform PPA](https://launchpad.net/~chris-needham/+archive/ubuntu/ppa)
and `ffmpeg` from Ubuntu. The tests compare:

- raw PCM, byte for byte, in 192 combinations of channels, length, zoom, bits,
  and mixdown or split;
- JSON output, field by field;
- Ogg Opus fixtures that ffmpeg encodes when the tests run: mono, stereo with
  an odd length, 16 kHz input, 44.1 kHz input, a changed `OpusHead` rate and
  gain, and a square wave that clips;
- the same fixtures remuxed to WebM, with known and unknown segment sizes. The
  output must be equal to the Ogg output of this crate, and to the
  audiowaveform output within the Opus tolerance;
- the same fixtures remuxed to fast-start and fragmented MP4. The output must
  be equal to the Ogg output of this crate, except for the FFmpeg end trimming;
- AAC that FFmpeg encodes as ADTS, and as MP4 in three layouts: fast start,
  fragmented, and media data before the `moov` box. The tests also use AAC from
  Apple `afconvert` if it is available. The reference is the FFmpeg decoder;
- MP3 and MP2 that FFmpeg encodes, with and without an Info tag: MPEG-1,
  MPEG-2 and MPEG-2.5, constant and variable bit rates, and a square wave that
  clips. The reference is audiowaveform, with a tolerance of ±1 per value;
- the same MPEG audio fixtures in the same three MP4 layouts, and in
  QuickTime. The reference is the FFmpeg decoder, with a tolerance of ±1 per
  sample.

Opus comparisons accept a difference of ±1 per value. The `opus` crate bundles
its own libopus build. The float output of this build is different in the last
bit from the Homebrew libopus. With the Homebrew libopus and the conversion of
this crate, the output is the same as the audiowaveform output.

## Limitations

- Surround Opus (channel mapping family 1) is not supported: it needs the
  multistream decoder.
- For chained Ogg files, `OggOpusSource` decodes only the first link.
- `OggOpusSource` rejects an Ogg file if its first page belongs to a Skeleton
  stream, because the source decodes only the stream of the first page.
- Other formats (WAV, FLAC, Vorbis) are not supported. Symphonia can
  decode them, but each of its format crates needs an MPL-2.0 exception in
  `deny.toml`.
- Symphonia does not decode the SBR and parametric stereo layers of HE-AAC.
- `MpegAudioSource` rejects data between frames. libmad skips this data.
- `MpegAudioSource` and `Mp4Source` reject free-format bit rates.
- `Mp4Source` rejects an MP4 sample that holds more than one MPEG audio
  frame.
- AAC in Matroska or WebM is not supported. Chrome and Safari do not record
  this combination: they record AAC in MP4 only.
- `WebmOpusSource` skips compressed and encrypted tracks. For a file with
  several segments, `WebmOpusSource` decodes only the first segment.
- One run makes peaks at one zoom level only.
- Opus decoding always needs libopus: there is no pure-Rust Opus decoder
  option.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this crate by you, as defined in the Apache-2.0 license, shall
be dual licensed as above, without any additional terms or conditions.

`deny.toml` accepts only permissive licenses for dependencies. The exception is
the Symphonia crates, which the optional `aac` and `mp3` features use. These
crates have the MPL-2.0 license, a file-level copyleft that applies only to the
Symphonia files:

- A binary that includes these crates must tell where their source code is
  available.
- If you change these files, you must share the changes under MPL-2.0.

The Opus decoder builds libopus (BSD-3-Clause) into the binary. If you
distribute a binary, include the libopus copyright notice and the copyright
notice of the `ogg` crate.

audiowaveform (GPL-3.0) is not part of the crate. Only the ignored oracle tests
use audiowaveform, and they run it as a separate program.
