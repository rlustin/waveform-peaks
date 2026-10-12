# Test fixtures

Short files generated for these tests, with no third-party audio:

- `sine-mono-44100.aac`: a mono sine tone at 44.1 kHz, encoded by FFmpeg.
- `chord-stereo-48000.aac`: a stereo chord at 48 kHz, encoded by FFmpeg.

Similar files can be made with:

```sh
ffmpeg -f lavfi -i sine=frequency=440:duration=1 -c:a aac sine.aac
```
