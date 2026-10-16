# Test fixtures

Short files generated for these tests, with no third-party audio:

- `sine-mono-44100.aac`: a mono sine tone at 44.1 kHz, encoded by FFmpeg.
- `chord-stereo-48000.aac`: a stereo chord at 48 kHz, encoded by FFmpeg.
- `sine-mono-44100.mp3`: a mono sine tone at 44.1 kHz, encoded by FFmpeg with
  libmp3lame, with an Info tag whose LAME extension records the encoder delay.
- `chord-stereo-48000.mp2`: a stereo chord at 48 kHz, encoded by FFmpeg's MP2
  encoder.
- `he-aac-apple.m4a`: HE-AAC encoded by Apple's `afconvert`, which signals SBR
  only inside the frames.
- `chrome-opus-stereo.mp4` and `chrome-aac-stereo.mp4`: one second of a 440 Hz
  `OscillatorNode` recorded by headless Chrome 155's `MediaRecorder`, as
  `audio/mp4;codecs=opus` and `audio/mp4;codecs=mp4a.40.2`, with a 250 ms
  timeslice.

Similar files can be made with:

```sh
ffmpeg -f lavfi -i sine=frequency=440:duration=1 -c:a aac sine.aac
ffmpeg -f lavfi -i sine=frequency=440:duration=1 -c:a libmp3lame -id3v2_version 0 sine.mp3
ffmpeg -f lavfi -i sine=frequency=440:duration=1 -ac 2 -c:a mp2 chord.mp2
afconvert -f m4af -d aach input.wav he-aac.m4a
```
