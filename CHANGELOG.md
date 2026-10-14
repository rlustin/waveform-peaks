# Changelog

All notable changes to this crate are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crate
follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- `Mp4AacSource` is renamed `Mp4Source`, ahead of reading other codecs than
  AAC from MP4. The old name remains as a deprecated alias.

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

[Unreleased]: https://github.com/rlustin/waveform-peaks/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/rlustin/waveform-peaks/releases/tag/v0.1.1
[0.1.0]: https://github.com/rlustin/waveform-peaks/releases/tag/v0.1.0
