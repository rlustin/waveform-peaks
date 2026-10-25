//! Comparison against BBC audiowaveform, used as a black-box oracle.
//!
//! Each test feeds the same input to audiowaveform and to this crate, then
//! compares the outputs. No audiowaveform source is used.
//!
//! These tests need `audiowaveform` and `ffmpeg` (built with libopus and
//! libmp3lame) on `PATH`, or their paths in `AUDIOWAVEFORM` and `FFMPEG`. They
//! are ignored by default:
//!
//! ```sh
//! cargo test --test oracle --all-features -- --ignored
//! ```

mod common;

use std::{fs, io::Cursor, path::Path};

use common::{assert_dat_matches, options, run, tool};
use tempfile::TempDir;
use waveform_peaks::{
    Bits, ChannelMode, DatWriter, JsonWriter, Limits, Options, PcmSource, PeakSink, RawPcmSource,
    generate,
};

#[cfg(any(feature = "opus", feature = "aac", feature = "mp3"))]
fn ffmpeg(args: &[&str]) {
    run(
        &tool("FFMPEG", "ffmpeg"),
        [&["-v", "error", "-y"], args].concat(),
    );
}

fn audiowaveform(args: &[&str]) {
    run(
        &tool("AUDIOWAVEFORM", "audiowaveform"),
        [&["-q"], args].concat(),
    );
}

/// audiowaveform arguments for `options`.
fn option_args(options: &Options) -> Vec<String> {
    let mut args = vec![
        "-z".to_owned(),
        options.samples_per_pixel.to_string(),
        "-b".to_owned(),
        match options.bits {
            Bits::Eight => "8",
            Bits::Sixteen => "16",
        }
        .to_owned(),
    ];
    if options.channel_mode == ChannelMode::Split {
        args.push("--split-channels".to_owned());
    }
    args
}

fn ours<P: PcmSource, S: PeakSink>(source: &mut P, options: &Options, sink: S) -> S::Output {
    generate(source, options, &Limits::default(), sink).expect("generate")
}

fn ours_dat<P: PcmSource>(source: &mut P, options: &Options) -> Vec<u8> {
    ours(source, options, DatWriter::new(Cursor::new(Vec::new()))).into_inner()
}

fn reference(input: &Path, input_args: &[&str], options: &Options, output: &Path) -> Vec<u8> {
    let input = input.to_str().unwrap();
    let output_str = output.to_str().unwrap();
    let extra = option_args(options);
    let mut args = vec!["-i", input];
    args.extend_from_slice(input_args);
    args.extend(extra.iter().map(String::as_str));
    args.extend(["-o", output_str]);
    audiowaveform(&args);
    fs::read(output).unwrap()
}

#[test]
#[ignore = "needs audiowaveform on PATH"]
fn raw_pcm_matches_audiowaveform() {
    let dir = TempDir::new().unwrap();
    let mut cases = 0;
    for channels in [1u16, 2, 3] {
        for frames in [0usize, 1, 999, 48_013] {
            let samples = common::pcm(frames, channels as usize, frames as u64 + channels as u64);
            let bytes = common::le_bytes(&samples);
            let input = dir.path().join("input.raw");
            fs::write(&input, &bytes).unwrap();
            let channels_arg = channels.to_string();
            let raw_args = [
                "--input-format",
                "raw",
                "--raw-format",
                "s16le",
                "--raw-samplerate",
                "44100",
                "--raw-channels",
                &channels_arg,
            ];

            for samples_per_pixel in [2, 7, 256, 1000] {
                for bits in [Bits::Eight, Bits::Sixteen] {
                    for channel_mode in [ChannelMode::Mixdown, ChannelMode::Split] {
                        let options = options(samples_per_pixel, bits, channel_mode);
                        let expected =
                            reference(&input, &raw_args, &options, &dir.path().join("ref.dat"));
                        let mut source =
                            RawPcmSource::new(Cursor::new(&bytes), 44_100, channels).unwrap();
                        let actual = ours_dat(&mut source, &options);
                        let context = format!("{channels} ch, {frames} frames, {options:?}");
                        assert_dat_matches(&actual, &expected, 0, &context);
                        cases += 1;
                    }
                }
            }
        }
    }
    assert_eq!(cases, 192);
}

#[test]
#[ignore = "needs audiowaveform on PATH"]
fn raw_pcm_json_matches_audiowaveform() {
    let dir = TempDir::new().unwrap();
    let samples = common::pcm(10_007, 2, 11);
    let bytes = common::le_bytes(&samples);
    let input = dir.path().join("input.raw");
    fs::write(&input, &bytes).unwrap();
    let raw_args = [
        "--input-format",
        "raw",
        "--raw-format",
        "s16le",
        "--raw-samplerate",
        "16000",
        "--raw-channels",
        "2",
    ];

    for options in [
        Options::default(),
        options(100, Bits::Eight, ChannelMode::Split),
    ] {
        let expected = reference(&input, &raw_args, &options, &dir.path().join("ref.json"));
        let mut source = RawPcmSource::new(Cursor::new(&bytes), 16_000, 2).unwrap();
        let actual = ours(&mut source, &options, JsonWriter::new(Vec::new()));
        let expected: serde_json::Value = serde_json::from_slice(&expected).unwrap();
        let actual: serde_json::Value = serde_json::from_slice(&actual).unwrap();
        assert_eq!(actual, expected, "{options:?}");
    }
}

/// Decodes all of `source`, with its sample rate and channel count.
#[cfg(any(feature = "opus", feature = "aac", feature = "mp3"))]
fn decoded<P: PcmSource>(source: P) -> (Vec<i16>, u32, u16) {
    let (rate, channels) = (source.sample_rate(), source.channels());
    (common::samples(source).unwrap(), rate, channels)
}

/// Checks that FFmpeg wrote the media data before the moov box.
#[cfg(any(feature = "aac", feature = "mp3"))]
fn assert_media_first(path: &Path) {
    let file = fs::read(path).unwrap();
    let find = |kind: &[u8]| file.windows(4).position(|w| w == kind).unwrap();
    assert!(find(b"mdat") < find(b"moov"), "{}", path.display());
}

#[cfg(feature = "opus")]
mod opus_input {
    use super::*;
    use waveform_peaks::{
        Mp4Options, Mp4Source, OggOpusSource, OpusOptions, Overflow, WebmOpusSource,
    };

    /// Largest accepted per-value difference for Opus input.
    ///
    /// The `opus` crate builds its own copy of libopus, whose float output
    /// differs in the last bit from the system libopus audiowaveform links.
    /// After scaling, a few samples in ten thousand land 1 apart, and which
    /// ones can change from one machine to the next. Decoding the
    /// same files with Homebrew's libopus and this crate's conversion gives
    /// identical output, so the conversion itself is exact.
    const OPUS_TOLERANCE: i32 = 1;

    struct Fixture {
        name: &'static str,
        /// ffmpeg lavfi source graph.
        source: &'static str,
        channels: &'static str,
        /// Optional `OpusHead` rewrite: input sample rate, output gain (Q7.8 dB).
        patch: Option<(Option<u32>, Option<i16>)>,
    }

    const FIXTURES: &[Fixture] = &[
        Fixture {
            name: "mono-48k",
            source: "sine=frequency=440:sample_rate=48000:duration=1.5",
            channels: "1",
            patch: None,
        },
        Fixture {
            // An odd length exercises end trimming from the last granule position.
            name: "stereo-48k-odd-length",
            source: "aevalsrc=exprs=0.5*sin(440*2*PI*t)|0.3*sin(660*2*PI*t)+0.1*sin(5000*2*PI*t):s=48000:d=1.234",
            channels: "2",
            patch: None,
        },
        Fixture {
            // OpusHead records 16 kHz, so both decoders run at 16 kHz.
            name: "noise-16k",
            source: "anoisesrc=d=2.1:c=pink:r=16000:a=0.5:seed=42",
            channels: "1",
            patch: None,
        },
        Fixture {
            name: "chord-44k1-stereo",
            source: "aevalsrc=exprs=0.3*sin(261.6*2*PI*t)+0.3*sin(329.6*2*PI*t)|0.3*sin(392*2*PI*t)*sin(0.5*2*PI*t):s=44100:d=3.3",
            channels: "2",
            patch: None,
        },
        Fixture {
            // opusenc records the original rate; 22050 must decode at 24 kHz.
            name: "head-22050",
            source: "sine=frequency=440:sample_rate=48000:duration=1.5",
            channels: "1",
            patch: Some((Some(22_050), None)),
        },
        Fixture {
            // Decodes at 16 kHz with an end position that is not a whole
            // number of 16 kHz frames, which a 16 kHz source never gives.
            name: "head-16000-odd-length",
            source: "sine=frequency=440:sample_rate=48000:duration=1.2341",
            channels: "1",
            patch: Some((Some(16_000), None)),
        },
        Fixture {
            name: "output-gain-minus-6db",
            source: "sine=frequency=440:sample_rate=48000:duration=1.5",
            channels: "1",
            patch: Some((None, Some(-1536))),
        },
        Fixture {
            // Full-scale square wave: decoding overshoots [-1, 1] and
            // audiowaveform wraps the overshoot.
            name: "clipping-square",
            source: "aevalsrc=exprs=sgn(sin(440*2*PI*t)):s=48000:d=1",
            channels: "1",
            patch: None,
        },
    ];

    fn encode(dir: &Path, fixture: &Fixture) -> std::path::PathBuf {
        let path = dir.join(format!("{}.opus", fixture.name));
        let path_str = path.to_str().unwrap();
        ffmpeg(&[
            "-f",
            "lavfi",
            "-i",
            fixture.source,
            "-ac",
            fixture.channels,
            "-c:a",
            "libopus",
            "-b:a",
            "64k",
            path_str,
        ]);
        if let Some((rate, gain)) = fixture.patch {
            let patched = common::patch_opus_head(&fs::read(&path).unwrap(), rate, gain);
            fs::write(&path, patched).unwrap();
        }
        path
    }

    fn wrap() -> OpusOptions {
        let mut options = OpusOptions::default();
        options.overflow = Overflow::Wrap;
        options
    }

    #[test]
    #[ignore = "needs audiowaveform and ffmpeg on PATH"]
    fn ogg_opus_matches_audiowaveform() {
        let dir = TempDir::new().unwrap();
        for fixture in FIXTURES {
            let input = encode(dir.path(), fixture);
            for options in [
                Options::default(),
                options(1000, Bits::Eight, ChannelMode::Split),
                options(2, Bits::Sixteen, ChannelMode::Mixdown),
            ] {
                let expected = reference(&input, &[], &options, &dir.path().join("ref.dat"));
                let mut source =
                    OggOpusSource::with_options(fs::File::open(&input).unwrap(), wrap()).unwrap();
                let actual = ours_dat(&mut source, &options);
                let context = format!("{}, {options:?}", fixture.name);
                assert_dat_matches(&actual, &expected, OPUS_TOLERANCE, &context);
            }
        }
    }

    #[test]
    #[ignore = "needs audiowaveform and ffmpeg on PATH"]
    fn ogg_opus_json_matches_audiowaveform() {
        let dir = TempDir::new().unwrap();
        let input = encode(dir.path(), &FIXTURES[1]);
        let options = options(256, Bits::Sixteen, ChannelMode::Split);
        let expected = reference(&input, &[], &options, &dir.path().join("ref.json"));
        let mut source =
            OggOpusSource::with_options(fs::File::open(&input).unwrap(), wrap()).unwrap();
        let actual = ours(&mut source, &options, JsonWriter::new(Vec::new()));
        let mut expected: serde_json::Value = serde_json::from_slice(&expected).unwrap();
        let mut actual: serde_json::Value = serde_json::from_slice(&actual).unwrap();
        let take_data = |json: &mut serde_json::Value| -> Vec<i32> {
            serde_json::from_value(json["data"].take()).unwrap()
        };
        let (our_data, their_data) = (take_data(&mut actual), take_data(&mut expected));
        let context = format!("{}, {options:?}", FIXTURES[1].name);
        assert_eq!(actual, expected, "{context}");
        common::assert_values_match(&our_data, &their_data, OPUS_TOLERANCE, &context);
    }

    /// Remuxes `ogg` to WebM without touching the packets. `live` writes a
    /// segment of unknown size, as recorders do.
    fn remux_webm(ogg: &Path, live: bool) -> std::path::PathBuf {
        let path = ogg.with_extension(if live { "live.webm" } else { "webm" });
        let live = if live { "1" } else { "0" };
        let (ogg, out) = (ogg.to_str().unwrap(), path.to_str().unwrap());
        ffmpeg(&["-i", ogg, "-c", "copy", "-live", live, out]);
        path
    }

    /// audiowaveform reads no WebM, so the reference is its output for the Ogg
    /// file the WebM file was remuxed from. ffmpeg carries the Ogg end
    /// trimming over as `DiscardPadding`, but not the other way round.
    #[test]
    #[ignore = "needs audiowaveform and ffmpeg on PATH"]
    fn webm_opus_matches_audiowaveform_on_the_same_packets() {
        let dir = TempDir::new().unwrap();
        for fixture in FIXTURES {
            let ogg = encode(dir.path(), fixture);
            for live in [false, true] {
                let webm = remux_webm(&ogg, live);
                for options in [
                    Options::default(),
                    options(2, Bits::Sixteen, ChannelMode::Split),
                ] {
                    let context = format!("{}, live {live}, {options:?}", fixture.name);
                    let mut from_webm =
                        WebmOpusSource::with_options(fs::File::open(&webm).unwrap(), wrap())
                            .unwrap();
                    let mut from_ogg =
                        OggOpusSource::with_options(fs::File::open(&ogg).unwrap(), wrap()).unwrap();
                    let actual = ours_dat(&mut from_webm, &options);
                    assert_eq!(actual, ours_dat(&mut from_ogg, &options), "{context}");
                    let expected = reference(&ogg, &[], &options, &dir.path().join("ref.dat"));
                    assert_dat_matches(&actual, &expected, OPUS_TOLERANCE, &context);
                }
            }
        }
    }

    /// Remuxes `ogg` to MP4 without touching the packets, with the moov box
    /// first or fragmented as recorders write.
    fn remux_mp4(ogg: &Path, fragmented: bool) -> std::path::PathBuf {
        let path = ogg.with_extension(if fragmented { "frag.mp4" } else { "mp4" });
        let flags = if fragmented {
            "frag_keyframe+empty_moov+default_base_moof"
        } else {
            "+faststart"
        };
        let (ogg, out) = (ogg.to_str().unwrap(), path.to_str().unwrap());
        ffmpeg(&["-i", ogg, "-c", "copy", "-movflags", flags, out]);
        path
    }

    /// ffmpeg writes the pre-skip as the edit list's media time, and the
    /// length in milliseconds, so the end can be up to a millisecond off the
    /// Ogg granule position. Fragmented files get no edit list: the `dOps`
    /// pre-skip is trimmed, but not the end padding, which is under one packet.
    #[test]
    #[ignore = "needs ffmpeg on PATH"]
    fn mp4_opus_matches_ogg_on_the_same_packets() {
        let dir = TempDir::new().unwrap();
        for fixture in FIXTURES {
            let ogg = encode(dir.path(), fixture);
            let (reference, rate, channels) = decoded(
                OggOpusSource::with_options(fs::File::open(&ogg).unwrap(), wrap()).unwrap(),
            );
            for fragmented in [false, true] {
                let context = format!("{}, fragmented {fragmented}", fixture.name);
                let mp4 = remux_mp4(&ogg, fragmented);
                let mut options = Mp4Options::default();
                options.opus = wrap();
                let (ours, our_rate, our_channels) = decoded(
                    Mp4Source::with_options(fs::File::open(&mp4).unwrap(), options).unwrap(),
                );
                assert_eq!((our_rate, our_channels), (rate, channels), "{context}");
                let shared = ours.len().min(reference.len());
                assert!(
                    ours[..shared] == reference[..shared],
                    "{context}: samples differ"
                );
                let frames = |samples: &[i16]| (samples.len() / channels as usize) as i64;
                let extra = frames(&ours) - frames(&reference);
                let allowed = if fragmented {
                    0..=rate as i64 / 50
                } else {
                    -(rate as i64) / 1000..=rate as i64 / 1000
                };
                assert!(allowed.contains(&extra), "{context}: {extra} frames off");
            }
        }
    }

    /// Saturation (the default) only changes values where the decoder
    /// overshoots full scale.
    #[test]
    #[ignore = "needs ffmpeg on PATH"]
    fn saturate_differs_from_wrap_only_when_clipping() {
        let dir = TempDir::new().unwrap();
        for fixture in FIXTURES {
            let input = encode(dir.path(), fixture);
            let options = Options::default();
            let mut wrapped =
                OggOpusSource::with_options(fs::File::open(&input).unwrap(), wrap()).unwrap();
            let mut saturated = OggOpusSource::new(fs::File::open(&input).unwrap()).unwrap();
            let (wrapped, saturated) = (
                ours_dat(&mut wrapped, &options),
                ours_dat(&mut saturated, &options),
            );
            if fixture.name == "clipping-square" {
                assert_ne!(wrapped, saturated);
                let peak = common::parse_dat(&saturated)
                    .values
                    .iter()
                    .map(|v| v.abs())
                    .max()
                    .unwrap();
                assert_eq!(peak, 32768, "saturated output reaches full scale");
            } else {
                assert_eq!(wrapped, saturated, "{}", fixture.name);
            }
        }
    }
}

/// audiowaveform reads no AAC, so the reference is FFmpeg's AAC decoder. Its
/// float output is converted here as this crate converts Symphonia's, so the
/// comparison covers decoding and trimming, not FFmpeg's 16-bit conversion.
#[cfg(feature = "aac")]
mod aac_input {
    use super::*;
    use std::process::Command;
    use waveform_peaks::{AdtsAacSource, Mp4Source, RawPcmSource};

    /// Largest accepted per-sample difference. Both decoders work in floating
    /// point and round a few samples in ten thousand differently.
    const AAC_TOLERANCE: i32 = 1;

    struct Fixture {
        name: &'static str,
        source: &'static str,
        channels: u16,
        rate: u32,
        seconds: f64,
    }

    const FIXTURES: &[Fixture] = &[
        Fixture {
            name: "mono-44k1",
            source: "sine=frequency=440:sample_rate=44100:duration=1.013",
            channels: 1,
            rate: 44_100,
            seconds: 1.013,
        },
        Fixture {
            name: "stereo-48k-odd-length",
            source: "aevalsrc=exprs=0.5*sin(440*2*PI*t)|0.3*sin(660*2*PI*t)+0.1*sin(5000*2*PI*t):s=48000:d=1.2345",
            channels: 2,
            rate: 48_000,
            seconds: 1.2345,
        },
        Fixture {
            name: "noise-16k",
            source: "anoisesrc=d=2.1:c=pink:r=16000:a=0.5:seed=42",
            channels: 1,
            rate: 16_000,
            seconds: 2.1,
        },
        Fixture {
            name: "clipping-square",
            source: "aevalsrc=exprs=sgn(sin(440*2*PI*t)):s=48000:d=1",
            channels: 1,
            rate: 48_000,
            seconds: 1.0,
        },
    ];

    #[derive(Debug, Clone, Copy, PartialEq)]
    enum Layout {
        Adts,
        FastStart,
        /// FFmpeg's default, with the media data before the moov box.
        MediaFirst,
        Fragmented,
        /// Apple's encoder, as Safari records with: iTunSMPB, no edit list.
        Apple,
    }

    /// Encodes `fixture`, or `None` when the layout needs a missing tool.
    fn encode(dir: &Path, fixture: &Fixture, layout: Layout) -> Option<std::path::PathBuf> {
        let extension = if layout == Layout::Adts { "aac" } else { "m4a" };
        let path = dir.join(format!("{}-{layout:?}.{extension}", fixture.name));
        let out = path.to_str().unwrap();
        let channels = fixture.channels.to_string();
        let input = [
            "-f",
            "lavfi",
            "-i",
            fixture.source,
            "-ac",
            channels.as_str(),
        ];
        let aac = ["-c:a", "aac", "-b:a", "96k"];
        match layout {
            Layout::Adts => ffmpeg(&[&input[..], &aac, &["-f", "adts", out]].concat()),
            Layout::FastStart => {
                ffmpeg(&[&input[..], &aac, &["-movflags", "+faststart", out]].concat())
            }
            Layout::MediaFirst => {
                ffmpeg(&[&input[..], &aac, &[out]].concat());
                assert_media_first(&path);
            }
            Layout::Fragmented => ffmpeg(
                &[
                    &input[..],
                    &aac,
                    &["-movflags", "frag_keyframe+empty_moov+default_base_moof"],
                    &["-frag_duration", "300000", out],
                ]
                .concat(),
            ),
            Layout::Apple => {
                let afconvert = tool("AFCONVERT", "afconvert");
                if Command::new(&afconvert).arg("-h").output().is_err() {
                    return None;
                }
                let wav = dir.join(format!("{}.wav", fixture.name));
                let wav = wav.to_str().unwrap();
                ffmpeg(&[&input[..], &[wav]].concat());
                run(&afconvert, ["-f", "m4af", "-d", "aac", wav, out]);
            }
        }
        Some(path)
    }

    fn decode_ours(path: &Path, layout: Layout) -> (Vec<i16>, u32, u16) {
        let file = fs::File::open(path).unwrap();
        match layout {
            Layout::Adts => decoded(AdtsAacSource::new(file).unwrap()),
            Layout::MediaFirst => decoded(Mp4Source::new_seekable(file).unwrap()),
            _ => decoded(Mp4Source::new(file).unwrap()),
        }
    }

    fn decode_ffmpeg(dir: &Path, path: &Path) -> Vec<i16> {
        let raw = dir.join("reference.f32");
        ffmpeg(&[
            "-i",
            path.to_str().unwrap(),
            "-f",
            "f32le",
            raw.to_str().unwrap(),
        ]);
        fs::read(&raw)
            .unwrap()
            .as_chunks::<4>()
            .0
            .iter()
            .map(|&bytes| (f32::from_le_bytes(bytes) * 32768.0).round_ties_even() as i16)
            .collect()
    }

    fn peaks(samples: &[i16], rate: u32, channels: u16) -> Vec<u8> {
        let bytes: Vec<u8> = samples
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect();
        let mut source = RawPcmSource::new(Cursor::new(bytes), rate, channels).unwrap();
        let options = options(64, Bits::Sixteen, ChannelMode::Split);
        ours(
            &mut source,
            &options,
            DatWriter::new(Cursor::new(Vec::new())),
        )
        .into_inner()
    }

    #[test]
    #[ignore = "needs ffmpeg on PATH (and afconvert for Apple's encoder)"]
    fn aac_matches_ffmpeg() {
        let dir = TempDir::new().unwrap();
        for fixture in FIXTURES {
            for layout in [
                Layout::Adts,
                Layout::FastStart,
                Layout::MediaFirst,
                Layout::Fragmented,
                Layout::Apple,
            ] {
                let Some(path) = encode(dir.path(), fixture, layout) else {
                    eprintln!("skipping {layout:?}: afconvert not found");
                    continue;
                };
                let context = format!("{}, {layout:?}", fixture.name);
                let (ours, rate, channels) = decode_ours(&path, layout);
                assert_eq!(
                    (rate, channels),
                    (fixture.rate, fixture.channels),
                    "{context}"
                );
                let theirs = decode_ffmpeg(dir.path(), &path);
                let frames = ours.len() / channels as usize;
                match layout {
                    // Nothing to trim: both decode every frame.
                    Layout::Adts | Layout::Fragmented => {
                        assert_eq!(ours.len(), theirs.len(), "{context}")
                    }
                    // FFmpeg trims the priming but keeps the end padding,
                    // which this crate drops. FFmpeg's edit list stores the
                    // length in milliseconds.
                    Layout::FastStart | Layout::MediaFirst | Layout::Apple => {
                        let expected = fixture.seconds * rate as f64;
                        let slack = rate as f64 / 1000.0;
                        assert!(
                            (frames as f64 - expected).abs() <= slack,
                            "{context}: {frames} frames"
                        );
                        assert!(ours.len() <= theirs.len(), "{context}");
                    }
                }
                let theirs = &theirs[..ours.len()];
                let worst = ours
                    .iter()
                    .zip(theirs)
                    .map(|(&a, &b)| (a as i32 - b as i32).abs())
                    .max()
                    .unwrap_or(0);
                assert!(
                    worst <= AAC_TOLERANCE,
                    "{context}: samples differ by {worst}"
                );
                assert_dat_matches(
                    &peaks(&ours, rate, channels),
                    &peaks(theirs, rate, channels),
                    AAC_TOLERANCE,
                    &context,
                );
            }
        }
    }
}

#[cfg(feature = "mp3")]
mod mpeg_audio_input {
    use super::*;
    use waveform_peaks::{Mp4Source, MpegAudioSource};

    /// Largest accepted per-value difference. libmad, which audiowaveform
    /// decodes with, works in fixed point and Symphonia in floating point.
    const MPA_TOLERANCE: i32 = 1;

    struct Fixture {
        name: &'static str,
        source: &'static str,
        channels: u16,
        rate: u32,
        seconds: f64,
        /// FFmpeg codec arguments.
        codec: &'static [&'static str],
    }

    const FIXTURES: &[Fixture] = &[
        Fixture {
            name: "mp3-mono-44k1",
            source: "sine=frequency=440:sample_rate=44100:duration=1.013",
            channels: 1,
            rate: 44_100,
            seconds: 1.013,
            codec: &["-c:a", "libmp3lame", "-b:a", "128k"],
        },
        Fixture {
            name: "mp3-vbr-stereo-48k",
            source: "aevalsrc=exprs=0.5*sin(440*2*PI*t)|0.3*sin(660*2*PI*t)+0.1*sin(5000*2*PI*t):s=48000:d=1.2345",
            channels: 2,
            rate: 48_000,
            seconds: 1.2345,
            codec: &["-c:a", "libmp3lame", "-q:a", "2"],
        },
        Fixture {
            name: "mp3-mpeg2-stereo-22k05",
            source: "anoisesrc=d=2.1:c=pink:r=22050:a=0.5:seed=42",
            channels: 2,
            rate: 22_050,
            seconds: 2.1,
            codec: &["-c:a", "libmp3lame", "-b:a", "64k"],
        },
        Fixture {
            name: "mp3-mpeg2.5-mono-8k",
            source: "sine=frequency=440:sample_rate=8000:duration=1.5",
            channels: 1,
            rate: 8_000,
            seconds: 1.5,
            codec: &["-c:a", "libmp3lame", "-b:a", "16k"],
        },
        Fixture {
            name: "mp3-clipping-square",
            source: "aevalsrc=exprs=sgn(sin(440*2*PI*t)):s=44100:d=1",
            channels: 1,
            rate: 44_100,
            seconds: 1.0,
            codec: &["-c:a", "libmp3lame", "-b:a", "320k"],
        },
        Fixture {
            // Overshoots full scale, where libmad clips at -32767.
            name: "mp3-overdriven-square",
            source: "aevalsrc=exprs=1.2*sgn(sin(440*2*PI*t)):s=44100:d=1",
            channels: 1,
            rate: 44_100,
            seconds: 1.0,
            codec: &["-c:a", "libmp3lame", "-b:a", "320k"],
        },
        Fixture {
            name: "mp2-stereo-48k",
            source: "aevalsrc=exprs=0.5*sin(440*2*PI*t)|0.3*sin(660*2*PI*t):s=48000:d=1.2345",
            channels: 2,
            rate: 48_000,
            seconds: 1.2345,
            codec: &["-c:a", "mp2", "-b:a", "192k"],
        },
        Fixture {
            name: "mp2-mono-32k",
            source: "anoisesrc=d=1.1:c=pink:r=32000:a=0.5:seed=7",
            channels: 1,
            rate: 32_000,
            seconds: 1.1,
            codec: &["-c:a", "mp2", "-b:a", "96k"],
        },
    ];

    fn encode(
        dir: &Path,
        fixture: &Fixture,
        extra: &[&str],
        extension: &str,
    ) -> std::path::PathBuf {
        let path = dir.join(format!("{}{}.{extension}", fixture.name, extra.join("")));
        let channels = fixture.channels.to_string();
        let input = [
            "-f",
            "lavfi",
            "-i",
            fixture.source,
            "-ac",
            channels.as_str(),
        ];
        ffmpeg(&[&input[..], fixture.codec, extra, &[path.to_str().unwrap()]].concat());
        path
    }

    #[test]
    #[ignore = "needs audiowaveform and ffmpeg on PATH"]
    fn mpeg_audio_matches_audiowaveform() {
        let dir = TempDir::new().unwrap();
        let option_sets = [
            options(64, Bits::Sixteen, ChannelMode::Split),
            options(256, Bits::Sixteen, ChannelMode::Mixdown),
            options(32, Bits::Eight, ChannelMode::Mixdown),
        ];
        for fixture in FIXTURES {
            let extension = if fixture.name.starts_with("mp2") {
                "mp2"
            } else {
                "mp3"
            };
            // Without a Xing or Info tag, nothing is trimmed.
            for extra in [&[][..], &["-write_xing", "0"]] {
                let path = encode(dir.path(), fixture, extra, extension);
                for options in &option_sets {
                    let context = format!("{}{extra:?}, {options:?}", fixture.name);
                    let mut source = MpegAudioSource::new(fs::File::open(&path).unwrap()).unwrap();
                    assert_eq!(
                        (source.sample_rate(), source.channels()),
                        (fixture.rate, fixture.channels),
                        "{context}"
                    );
                    let ours = ours_dat(&mut source, options);
                    let theirs = reference(
                        &path,
                        &["--input-format", "mp3"],
                        options,
                        &dir.path().join("reference.dat"),
                    );
                    assert_dat_matches(&ours, &theirs, MPA_TOLERANCE, &context);
                    // The tolerance would hide a clipping difference.
                    let extremes = |dat: &[u8]| {
                        let values = common::parse_dat(dat).values;
                        (values.iter().min().copied(), values.iter().max().copied())
                    };
                    assert_eq!(extremes(&ours), extremes(&theirs), "{context}");
                }
            }
        }
    }

    /// The samples of a 16-bit WAV file.
    fn wav_samples(path: &Path) -> Vec<i16> {
        let bytes = fs::read(path).unwrap();
        let mut at = 12;
        loop {
            let len = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
            if &bytes[at..at + 4] == b"data" {
                return bytes[at + 8..at + 8 + len]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|&pair| i16::from_le_bytes(pair))
                    .collect();
            }
            at += 8 + len + len % 2;
        }
    }

    /// Damaged files: libmad skips data between frames and drops the frames
    /// that need a bit reservoir it has not read. After skipped data, its
    /// synthesis filter has not seen the dropped frames, so up to one frame
    /// of output differs when frames are dropped. After a frame that does not
    /// decode, Symphonia empties its reservoir and libmad does not, so the
    /// frames that can use its data differ: up to 511 bytes of main data,
    /// and one frame more for the synthesis filter.
    #[test]
    #[ignore = "needs audiowaveform and ffmpeg on PATH"]
    fn damaged_mpeg_audio_matches_audiowaveform() {
        let dir = TempDir::new().unwrap();
        let reservoir_off = ["-reservoir", "0"];
        let encodings = [
            ("mp3-mono-44k1", &[][..]),
            ("mp3-mono-44k1", &reservoir_off),
            ("mp3-vbr-stereo-48k", &[]),
            ("mp3-vbr-stereo-48k", &reservoir_off),
            ("mp2-stereo-48k", &[]),
        ];
        for (name, extra) in encodings {
            let fixture = FIXTURES.iter().find(|f| f.name == name).unwrap();
            let extension = &name[..3];
            let path = encode(
                dir.path(),
                fixture,
                &[extra, &["-id3v2_version", "0"]].concat(),
                extension,
            );
            let frames = common::mpa_frames(&fs::read(&path).unwrap());
            let (third, half) = (frames.len() / 3, frames.len() / 2);
            let fake_header = [&[0; 3][..], &frames[1][..4], &[0; 20]].concat();
            let mut id3v2 = b"ID3\x04\0\0\0\0\0\x20".to_vec();
            id3v2.extend([0xFF; 32]);
            let cut = &frames[half][..frames[half].len() / 2];
            let mut variants = vec![
                (
                    "junk",
                    [
                        frames[..third].concat(),
                        vec![0; 10],
                        frames[third..].concat(),
                    ]
                    .concat(),
                ),
                (
                    "fake header",
                    [
                        frames[..half].concat(),
                        fake_header,
                        frames[half..].concat(),
                    ]
                    .concat(),
                ),
                (
                    "ID3v2 tag",
                    [frames[..half].concat(), id3v2, frames[half..].concat()].concat(),
                ),
                ("leading junk", [vec![0x55; 300], frames.concat()].concat()),
                ("cut start", frames[third..].concat()),
                ("trailing junk", [frames.concat(), vec![0xFF; 100]].concat()),
            ];
            // libmad drops a layer III frame whose Huffman data runs past its
            // end, and Symphonia decodes it, so a cut frame is compared in
            // layer II only (see the README limitations).
            if extension == "mp3" {
                // big_values above 288 in the first granule of the middle
                // frame, after main_data_begin, the private bits, scfsi and
                // part2_3_length.
                let mono = fixture.channels == 1;
                let at = 9 + if mono { 5 + 4 } else { 3 + 8 } + 12;
                let mut corrupt = frames.clone();
                assert_eq!(corrupt[half][1] & 1, 1, "{name}: no CRC");
                for bit in at..at + 9 {
                    corrupt[half][4 + bit / 8] |= 0x80 >> (bit % 8);
                }
                variants.push(("frame that does not decode", corrupt.concat()));
            }
            if extension == "mp2" {
                variants.push((
                    "cut frame",
                    [&frames[..half].concat(), cut, &frames[half + 1..].concat()].concat(),
                ));
            }
            for (damage, file) in variants {
                let context = format!("{name}{extra:?}, {damage}");
                let input = dir.path().join(format!("damaged.{extension}"));
                fs::write(&input, &file).unwrap();
                let output = dir.path().join("damaged.wav");
                audiowaveform(&[
                    "-i",
                    input.to_str().unwrap(),
                    "--input-format",
                    "mp3",
                    "-o",
                    output.to_str().unwrap(),
                ]);
                let theirs = wav_samples(&output);
                let ours =
                    common::samples(MpegAudioSource::new(fs::File::open(&input).unwrap()).unwrap())
                        .unwrap();
                assert_eq!(ours.len(), theirs.len(), "{context}: sample count");
                let channels = fixture.channels as usize;
                let differing = ours
                    .chunks(channels)
                    .zip(theirs.chunks(channels))
                    .filter(|(a, b)| {
                        a.iter()
                            .zip(*b)
                            .any(|(&a, &b)| (a as i32 - b as i32).abs() > MPA_TOLERANCE)
                    })
                    .count();
                let reservoir = name.starts_with("mp3") && extra.is_empty();
                let allowed = match (reservoir, damage) {
                    (false, _) => 0,
                    (true, "frame that does not decode") => {
                        let side = if fixture.channels == 1 {
                            4 + 17
                        } else {
                            4 + 32
                        };
                        let mut main_data = 0;
                        let reached = frames[half + 1..]
                            .iter()
                            .take_while(|frame| {
                                let before = main_data;
                                main_data += frame.len() - side;
                                before < 511
                            })
                            .count();
                        (reached + 1) * 1152
                    }
                    (true, _) => 1152,
                };
                assert!(differing <= allowed, "{context}: {differing} frames differ");
            }
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq)]
    enum Layout {
        FastStart,
        /// FFmpeg's default, with the media data before the moov box.
        MediaFirst,
        Fragmented,
        /// QuickTime, with a `.mp3` or `.mp2` sample entry.
        Mov,
    }

    fn encode_mp4(dir: &Path, fixture: &Fixture, layout: Layout) -> std::path::PathBuf {
        let (extension, flags): (_, &[&str]) = match layout {
            Layout::FastStart => ("mp4", &["-movflags", "+faststart"]),
            Layout::MediaFirst => ("mp4", &[]),
            Layout::Fragmented => (
                "mp4",
                &[
                    "-movflags",
                    "frag_keyframe+empty_moov+default_base_moof",
                    "-frag_duration",
                    "300000",
                ],
            ),
            Layout::Mov => ("mov", &["-movflags", "+faststart"]),
        };
        let path = dir.join(format!("{}-{layout:?}.{extension}", fixture.name));
        let channels = fixture.channels.to_string();
        let input = [
            "-f",
            "lavfi",
            "-i",
            fixture.source,
            "-ac",
            channels.as_str(),
        ];
        // MPEG-2.5 rates are not standard in MP4.
        let strict = ["-strict", "-1"];
        ffmpeg(
            &[
                &input[..],
                fixture.codec,
                flags,
                &strict,
                &[path.to_str().unwrap()],
            ]
            .concat(),
        );
        if layout == Layout::MediaFirst {
            assert_media_first(&path);
        }
        path
    }

    /// FFmpeg's float decoder, scaled as this crate scales Symphonia's output.
    fn decode_ffmpeg(dir: &Path, path: &Path) -> Vec<i16> {
        let raw = dir.join("reference.f32");
        ffmpeg(&[
            "-i",
            path.to_str().unwrap(),
            "-f",
            "f32le",
            raw.to_str().unwrap(),
        ]);
        fs::read(&raw)
            .unwrap()
            .as_chunks::<4>()
            .0
            .iter()
            .map(|&bytes| (f32::from_le_bytes(bytes) * 32768.0).floor() as i16)
            .collect()
    }

    #[test]
    #[ignore = "needs ffmpeg on PATH"]
    fn mp4_matches_ffmpeg() {
        let dir = TempDir::new().unwrap();
        for fixture in FIXTURES {
            for layout in [
                Layout::FastStart,
                Layout::MediaFirst,
                Layout::Fragmented,
                Layout::Mov,
            ] {
                let context = format!("{}, {layout:?}", fixture.name);
                let path = encode_mp4(dir.path(), fixture, layout);
                let file = fs::File::open(&path).unwrap();
                let source = if layout == Layout::MediaFirst {
                    Mp4Source::new_seekable(file)
                } else {
                    Mp4Source::new(file)
                };
                let (ours, rate, channels) = decoded(source.unwrap());
                assert_eq!(
                    (rate, channels),
                    (fixture.rate, fixture.channels),
                    "{context}"
                );
                let theirs = decode_ffmpeg(dir.path(), &path);
                match layout {
                    // No edit list: both decode every frame.
                    Layout::Fragmented => assert_eq!(ours.len(), theirs.len(), "{context}"),
                    // Both trim the start by the edit list, but FFmpeg keeps
                    // what follows its end.
                    // FFmpeg's edit list stores the duration in milliseconds.
                    // For MP2 it ends the encoder delay, 481 frames, before
                    // the end of the input.
                    Layout::FastStart | Layout::MediaFirst | Layout::Mov => {
                        let frames = (ours.len() / channels as usize) as f64;
                        let delay = if fixture.name.starts_with("mp2") {
                            481.0
                        } else {
                            0.0
                        };
                        let expected = fixture.seconds * rate as f64 - delay;
                        assert!(
                            (frames - expected).abs() <= rate as f64 / 1000.0,
                            "{context}: {frames} frames"
                        );
                        assert!(ours.len() <= theirs.len(), "{context}");
                    }
                }
                let theirs = &theirs[..ours.len()];
                let worst = ours
                    .iter()
                    .zip(theirs)
                    .map(|(&a, &b)| (a as i32 - b as i32).abs())
                    .max()
                    .unwrap_or(0);
                assert!(
                    worst <= MPA_TOLERANCE,
                    "{context}: samples differ by {worst}"
                );
            }
        }
    }
}
