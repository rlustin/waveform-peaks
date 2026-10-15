# Test fixtures

Short files generated for these tests, with no third-party audio:

- `sine-mono-44100.aac`: a mono sine tone at 44.1 kHz, encoded by FFmpeg.
- `chord-stereo-48000.aac`: a stereo chord at 48 kHz, encoded by FFmpeg.
- `he-aac-apple.m4a`: HE-AAC encoded by Apple's `afconvert`, which signals SBR
  only inside the frames.
- `chrome-opus-stereo.mp4` and `chrome-aac-stereo.mp4`: one second of a 440 Hz
  `OscillatorNode` recorded by headless Chrome 155's `MediaRecorder`, as
  `audio/mp4;codecs=opus` and `audio/mp4;codecs=mp4a.40.2`, with a 250 ms
  timeslice.

Similar files can be made with:

```sh
ffmpeg -f lavfi -i sine=frequency=440:duration=1 -c:a aac sine.aac
afconvert -f m4af -d aach input.wav he-aac.m4a
```
