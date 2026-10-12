//! Comparison against BBC audiowaveform, used as a black-box oracle.
//!
//! Each test feeds the same input to audiowaveform and to this crate, then
//! compares the outputs. No audiowaveform source is used.
//!
//! These tests need `audiowaveform` and `ffmpeg` (built with libopus) on
//! `PATH`, or their paths in `AUDIOWAVEFORM` and `FFMPEG`. They are ignored by
//! default:
//!
//! ```sh
//! cargo test --test oracle -- --ignored
//! ```

mod common;

use std::{fs, io::Cursor, path::Path};

use common::{assert_dat_matches, options, run, tool};
use tempfile::TempDir;
use waveform_peaks::{
    Bits, ChannelMode, DatWriter, JsonWriter, Limits, Options, PcmSource, PeakSink, RawPcmSource,
    generate,
};

#[cfg(any(feature = "opus", feature = "aac"))]
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

#[cfg(feature = "opus")]
mod opus_input {
    use super::*;
    use waveform_peaks::{OggOpusSource, OpusOptions, Overflow, WebmOpusSource};

    /// Largest accepted per-value difference for Opus input.
    ///
    /// The `opus` crate builds its own copy of libopus, whose float output
    /// differs in the last bit from the Homebrew libopus audiowaveform links.
    /// After scaling, a few samples in ten thousand land 1 apart. Decoding the
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
        let expected: serde_json::Value = serde_json::from_slice(&expected).unwrap();
        let actual: serde_json::Value = serde_json::from_slice(&actual).unwrap();
        assert_eq!(actual, expected);
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
    use waveform_peaks::{AdtsAacSource, RawPcmSource};

    /// Largest accepted per-sample difference. Both decoders work in floating
    /// point and round a few samples in ten thousand differently.
    const AAC_TOLERANCE: i32 = 1;

    struct Fixture {
        name: &'static str,
        source: &'static str,
        channels: u16,
        rate: u32,
    }

    const FIXTURES: &[Fixture] = &[
        Fixture {
            name: "mono-44k1",
            source: "sine=frequency=440:sample_rate=44100:duration=1.013",
            channels: 1,
            rate: 44_100,
        },
        Fixture {
            name: "stereo-48k-odd-length",
            source: "aevalsrc=exprs=0.5*sin(440*2*PI*t)|0.3*sin(660*2*PI*t)+0.1*sin(5000*2*PI*t):s=48000:d=1.2345",
            channels: 2,
            rate: 48_000,
        },
        Fixture {
            name: "noise-16k",
            source: "anoisesrc=d=2.1:c=pink:r=16000:a=0.5:seed=42",
            channels: 1,
            rate: 16_000,
        },
        Fixture {
            name: "clipping-square",
            source: "aevalsrc=exprs=sgn(sin(440*2*PI*t)):s=48000:d=1",
            channels: 1,
            rate: 48_000,
        },
    ];

    fn encode(dir: &Path, fixture: &Fixture) -> std::path::PathBuf {
        let path = dir.join(format!("{}.aac", fixture.name));
        let channels = fixture.channels.to_string();
        ffmpeg(&[
            "-f",
            "lavfi",
            "-i",
            fixture.source,
            "-ac",
            channels.as_str(),
            "-c:a",
            "aac",
            "-b:a",
            "96k",
            "-f",
            "adts",
            path.to_str().unwrap(),
        ]);
        path
    }

    fn decode_ours(path: &Path) -> (Vec<i16>, u32, u16) {
        let mut source = AdtsAacSource::new(fs::File::open(path).unwrap()).unwrap();
        let mut samples = Vec::new();
        while let Some(block) = source.next_block().unwrap() {
            samples.extend(block);
        }
        (samples, source.sample_rate(), source.channels())
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
    #[ignore = "needs ffmpeg on PATH"]
    fn aac_matches_ffmpeg() {
        let dir = TempDir::new().unwrap();
        for fixture in FIXTURES {
            let path = encode(dir.path(), fixture);
            let context = fixture.name;
            let (ours, rate, channels) = decode_ours(&path);
            assert_eq!(
                (rate, channels),
                (fixture.rate, fixture.channels),
                "{context}"
            );
            let theirs = decode_ffmpeg(dir.path(), &path);
            // ADTS records no priming or padding: both decode every frame.
            assert_eq!(ours.len(), theirs.len(), "{context}");
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
                context,
            );
        }
    }
}
