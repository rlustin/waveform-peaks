# Changelog

All notable changes to this crate are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crate
follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `generate_many` makes peaks for several outputs, such as several zoom
  levels, in one pass over the input, so the input is decoded only once.

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

[Unreleased]: https://github.com/rlustin/waveform-peaks/compare/v0.1.4...HEAD
[0.1.4]: https://github.com/rlustin/waveform-peaks/releases/tag/v0.1.4
[0.1.3]: https://github.com/rlustin/waveform-peaks/releases/tag/v0.1.3
[0.1.2]: https://github.com/rlustin/waveform-peaks/releases/tag/v0.1.2
[0.1.1]: https://github.com/rlustin/waveform-peaks/releases/tag/v0.1.1
[0.1.0]: https://github.com/rlustin/waveform-peaks/releases/tag/v0.1.0
