# Changelog

All notable changes to this crate are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crate
follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- MP1, MP2 and MP3 input through Symphonia, behind the new `mp3` feature.
  `MpegAudioSource` reads MPEG audio streams (`.mp3`, `.mp2`) and trims them
  as audiowaveform does: it skips a Xing or Info tag frame, and trims the
  LAME encoder delay. `Mp4Source` reads MPEG audio tracks, including the
  QuickTime `.mp3` and `.mp2` sample entries.

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

[Unreleased]: https://github.com/rlustin/waveform-peaks/compare/v0.1.2...HEAD
[0.1.2]: https://github.com/rlustin/waveform-peaks/releases/tag/v0.1.2
[0.1.1]: https://github.com/rlustin/waveform-peaks/releases/tag/v0.1.1
[0.1.0]: https://github.com/rlustin/waveform-peaks/releases/tag/v0.1.0
