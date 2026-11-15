# Changelog

All notable changes to this crate are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crate
follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `Limits::max_sample_rate` bounds the sample rate that the input declares.
  The default is 1,048,575 Hz, the highest rate that a FLAC file can declare.

### Changed

- `generate` and `generate_many` refuse a sample rate above
  `Limits::max_sample_rate` with `Error::LimitExceeded`. With the default
  `Limits`, a WAV file or a `RawPcmSource` above 1,048,575 Hz did decode
  before. Raise the limit to accept such input.

### Fixed

- A file that declared a very high sample rate raised the number of frames
  that `Limits::max_duration` allows, up to 97,000 times for WAV and Ogg
  Vorbis input, so that the limit did not bound the work. The frames are now
  at most `Limits::max_duration` times `Limits::max_sample_rate`.
- `OggVorbisSource` kept memory in proportion to the input, far above
  `VorbisOptions::max_packet_bytes`, for a crafted stream that starts a
  large packet on each page and drops it on the next page.

## [0.1.7] - 2026-11-15

### Added

- `OggVorbisSource` reads Vorbis in Ogg, behind the new `vorbis` feature. It
  decodes with lewton, a pure-Rust decoder with the MIT and Apache-2.0
  licenses. With `Overflow::Wrap`, its output is within ±1 per value of the
  audiowaveform output, with the same length, also for files that FFmpeg
  cuts without re-encoding and for truncated files. Like libogg, it skips
  bytes outside valid pages and continues after a missing page. Silence of
  the same length replaces a damaged packet that lewton does not decode.
  `VorbisOptions` bounds the packet size, the codebook size and the frames
  held back to end the output where audiowaveform ends it.

## [0.1.6] - 2026-11-08

### Added

- `WavSource` reads WAV and RF64 files: 8-bit unsigned, 16-, 24- and 32-bit
  signed and 32- and 64-bit float PCM. It needs no feature and no
  dependency. With `Overflow::Wrap`, its output is the same as the
  audiowaveform output, except for float samples far beyond full scale.
- `FlacSource` reads FLAC files with 8-, 16- and 24-bit samples, behind the
  new `flac` feature. It decodes with claxon, a pure-Rust decoder with the
  Apache-2.0 license. For files that are not damaged and that claxon
  decodes, its output is the same as the audiowaveform output, also for
  files that FFmpeg writes to a pipe and for truncated files. After damage,
  it continues at the next frame that decodes and gives silence for the
  lost frames, so that the waveform stays in time with the audio.

### Changed

- `Overflow` is available without the `opus` feature, because
  `WavOptions` also uses it.

## [0.1.5] - 2026-11-07

### Added

- `generate_many` makes peaks for several outputs, such as several zoom
  levels, in one pass over the input, so the input is decoded only once.
- `Options::bands` also measures the level of a low, a mid and a high
  frequency band for each point, to colour a waveform by its spectrum.
  `BandJsonWriter` writes these levels to a separate JSON file and passes
  the peaks to another sink, so the peak file does not change.

### Changed

- `Mp4Source::new_seekable` seeks past the samples of other tracks and other
  boxes, when they take 64 KiB or more, instead of reading through them, so
  the video of a phone recording is not read. It reads the input length when
  it starts, so the file must not grow while it is read: data past that
  length is a truncated box. On an input that cannot seek, such as a pipe,
  it still reads a file with the `moov` box first.
- `MpegAudioSource` reads damaged files as audiowaveform does, instead of
  returning `Error::InvalidInput`. It skips data between frames and before
  the first frame, such as junk or an ID3 tag, and drops a frame that does
  not decode. Frames after an ID3v1 tag in the middle of a file now decode.
  As with audiowaveform, input in another format is now read to the end
  before the error, instead of failing on its first bytes.

### Fixed

- `MpegAudioSource` drops the layer III frames that audiowaveform drops at
  the start of a stream cut from a longer one, because their main data is in
  frames that are not there. The output had one or two frames more, which
  shifted every peak.

## [0.1.4] - 2026-10-22

### Added

- `Mp4Source::new_seekable` and `Mp4Source::seekable_with_options` read MP4
  with the media data before the `moov` box, as FFmpeg and phones write by
  default. They need a `Read + Seek` input, such as an upload saved to disk,
  and still read the media data only once.

### Changed

- `Mp4Source::new` returns `Error::InvalidInput` instead of
  `Error::Unsupported` for an `mdat` box that continues to the end of the
  file before the `moov` box. No `moov` box can follow it, so a remux or a
  seekable input does not help.

## [0.1.3] - 2026-10-16

### Added

- MP1, MP2 and MP3 input through Symphonia, behind the new `mp3` feature.
  `MpegAudioSource` reads MPEG audio streams (`.mp3`, `.mp2`) as audiowaveform
  does: it skips a Xing or Info tag frame, trims the LAME encoder delay, clips
  at -32767, and drops an incomplete last frame. `Mp4Source` reads MPEG audio
  tracks, including the QuickTime `.mp3` and `.mp2` sample entries.

## [0.1.2] - 2026-10-14

### Added

- Opus input from MP4, fragmented or with the `moov` box first, as Chrome
  records. Decoder settings are in `Mp4Options::opus`.

### Changed

- `Mp4AacSource` is renamed `Mp4Source`, now that it reads Opus as well as
  AAC, and is available with either the `opus` or the `aac` feature. The old
  name remains as a deprecated alias.
- `Mp4Source` stops reading at the end of the edit list: a file cut or
  damaged after the audio it shows decodes, and the rest of the input is
  not read.
- With both features, `Mp4Source` decodes the first AAC or Opus track: an
  Opus track ahead of an AAC one, which 0.1.1 passed over, is now chosen.
- An AAC track that Symphonia cannot decode, such as AAC Main, is passed
  over for the next audio track instead of failing the file.

## [0.1.1] - 2026-10-14

### Fixed

- MP4 files with their sample tables before the sample description, as
  Chrome's `MediaRecorder` writes, failed with "missing stsz box".

## [0.1.0] - 2026-10-12

### Added

- Streaming peak generation: `generate`, `PeakBuilder`, and `Options` and
  `Limits` for zoom, bit depth, channel mode and input bounds.
- Sinks for the audiowaveform `.dat` and JSON formats, and an in-memory sink.
- Raw 16-bit PCM input.
- Ogg Opus input through libopus, behind the default `opus` feature.
- Opus input from WebM and Matroska, including browser `MediaRecorder`
  recordings.
- AAC input from MP4 (fragmented or with the `moov` box first) and ADTS,
  behind the optional `aac` feature.

[0.1.7]: https://github.com/rlustin/waveform-peaks/releases/tag/v0.1.7
[0.1.6]: https://github.com/rlustin/waveform-peaks/releases/tag/v0.1.6
[0.1.5]: https://github.com/rlustin/waveform-peaks/releases/tag/v0.1.5
[0.1.4]: https://github.com/rlustin/waveform-peaks/releases/tag/v0.1.4
[0.1.3]: https://github.com/rlustin/waveform-peaks/releases/tag/v0.1.3
[0.1.2]: https://github.com/rlustin/waveform-peaks/releases/tag/v0.1.2
[0.1.1]: https://github.com/rlustin/waveform-peaks/releases/tag/v0.1.1
[0.1.0]: https://github.com/rlustin/waveform-peaks/releases/tag/v0.1.0
