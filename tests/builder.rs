//! Tool-free tests. Expected values come from probing audiowaveform 1.11.1
//! with raw PCM input; `tests/oracle.rs` checks the same rules against the
//! binary itself.

mod common;

use std::io::{self, Cursor};
use std::time::Duration;

use common::options;

use waveform_peaks::{
    Bits, ChannelMode, DatWriter, Error, Header, JsonWriter, Limits, MemorySink, Options,
    PcmSource, Peak, PeakBuilder, PeakSink, RawPcmSource, generate, generate_many,
};

fn peaks(samples: &[i16], channels: u16, options: &Options) -> Vec<i16> {
    let mut builder = PeakBuilder::new(8000, channels, options, MemorySink::new()).unwrap();
    builder.push_interleaved(samples).unwrap();
    builder.finish().unwrap().into_data()
}

/// Repeats each frame so a zoom of 2 reduces it to one point.
fn doubled(frames: &[i16], channels: usize) -> Vec<i16> {
    frames
        .chunks_exact(channels)
        .flat_map(|frame| frame.iter().chain(frame).copied())
        .collect()
}

#[test]
fn trailing_partial_block_is_emitted() {
    let data = peaks(
        &[1, -2, 3, 4, 5],
        1,
        &options(2, Bits::Sixteen, ChannelMode::Mixdown),
    );
    assert_eq!(data, [-2, 1, 3, 4, 5, 5]);
}

#[test]
fn mixdown_truncates_toward_zero() {
    let frames = [3, -4, -3, 4, 1, 2, -1, -2, 32767, 32767, -32768, -32768];
    let data = peaks(
        &doubled(&frames, 2),
        2,
        &options(2, Bits::Sixteen, ChannelMode::Mixdown),
    );
    assert_eq!(
        data,
        [0, 0, 0, 0, 1, 1, -1, -1, 32767, 32767, -32768, -32768]
    );
}

#[test]
fn eight_bit_truncates_toward_zero() {
    let frames = [-1, -255, -256, -257, 255, 256, 32767, -32768];
    let data = peaks(
        &doubled(&frames, 1),
        1,
        &options(2, Bits::Eight, ChannelMode::Mixdown),
    );
    assert_eq!(
        data,
        [0, 0, 0, 0, -1, -1, -1, -1, 0, 0, 1, 1, 127, 127, -128, -128]
    );
}

#[test]
fn split_keeps_channels_apart() {
    let frames = [3, -4, -3, 4];
    let data = peaks(
        &doubled(&frames, 2),
        2,
        &options(2, Bits::Sixteen, ChannelMode::Split),
    );
    assert_eq!(data, [3, 3, -4, -4, -3, -3, 4, 4]);
}

#[test]
fn blocks_can_split_anywhere() {
    let samples = common::pcm(10_001, 2, 7);
    let options = options(256, Bits::Sixteen, ChannelMode::Split);
    let whole = peaks(&samples, 2, &options);

    let mut builder = PeakBuilder::new(8000, 2, &options, MemorySink::new()).unwrap();
    for chunk in samples.chunks(2 * 37) {
        builder.push_interleaved(chunk).unwrap();
    }
    assert_eq!(builder.finish().unwrap().data(), whole);
}

#[test]
fn dat_version_follows_output_channels() {
    let dat = |channels: u16, mode: ChannelMode| {
        let mut builder = PeakBuilder::new(
            8000,
            channels,
            &options(2, Bits::Sixteen, mode),
            DatWriter::new(Cursor::new(Vec::new())),
        )
        .unwrap();
        builder
            .push_interleaved(&vec![0; channels as usize * 2])
            .unwrap();
        builder.finish().unwrap().into_inner()
    };
    assert_eq!(dat(1, ChannelMode::Split)[0], 1);
    assert_eq!(dat(2, ChannelMode::Mixdown)[0], 1);
    let split = dat(2, ChannelMode::Split);
    assert_eq!(split[0], 2);
    assert_eq!(&split[20..24], &2u32.to_le_bytes());
}

#[test]
fn dat_writer_patches_length_relative_to_start() {
    let mut output = Cursor::new(b"prefix".to_vec());
    output.set_position(6);
    let mut builder = PeakBuilder::new(
        8000,
        1,
        &options(2, Bits::Eight, ChannelMode::Mixdown),
        DatWriter::new(output),
    )
    .unwrap();
    builder.push_interleaved(&[100, -300, 512]).unwrap();
    let bytes = builder.finish().unwrap().into_inner();

    assert_eq!(&bytes[..6], b"prefix");
    let dat = &bytes[6..];
    assert_eq!(&dat[16..20], &2u32.to_le_bytes());
    assert_eq!(&dat[20..], &[(-1i8) as u8, 0, 2, 2]);
}

#[test]
fn memory_sink_round_trips_to_dat() {
    let samples = common::pcm(5000, 2, 3);
    let options = options(100, Bits::Sixteen, ChannelMode::Split);
    let streamed = {
        let mut builder =
            PeakBuilder::new(8000, 2, &options, DatWriter::new(Cursor::new(Vec::new()))).unwrap();
        builder.push_interleaved(&samples).unwrap();
        builder.finish().unwrap().into_inner()
    };
    let mut builder = PeakBuilder::new(8000, 2, &options, MemorySink::new()).unwrap();
    builder.push_interleaved(&samples).unwrap();
    assert_eq!(builder.finish().unwrap().to_dat(), streamed);
}

#[test]
fn json_writer_matches_audiowaveform_fields() {
    let mut builder = PeakBuilder::new(
        44100,
        2,
        &options(2, Bits::Eight, ChannelMode::Split),
        JsonWriter::new(Vec::new()),
    )
    .unwrap();
    builder.push_interleaved(&[300, -300, 300, -300]).unwrap();
    let json: serde_json::Value = serde_json::from_slice(&builder.finish().unwrap()).unwrap();
    assert_eq!(
        json,
        serde_json::json!({
            "version": 2, "channels": 2, "sample_rate": 44100, "samples_per_pixel": 2,
            "bits": 8, "length": 1, "data": [1, 1, -1, -1]
        })
    );
}

#[test]
fn empty_input_writes_header_only() {
    let mut source = RawPcmSource::new(Cursor::new(Vec::new()), 8000, 1).unwrap();
    let output = DatWriter::new(Cursor::new(Vec::new()));
    let bytes = generate(&mut source, &Options::default(), &Limits::default(), output)
        .unwrap()
        .into_inner();
    assert_eq!(bytes.len(), 20);
    assert_eq!(&bytes[16..20], &0u32.to_le_bytes());
}

#[test]
fn zoom_below_two_is_rejected() {
    let result = PeakBuilder::new(
        8000,
        1,
        &options(1, Bits::Sixteen, ChannelMode::Mixdown),
        MemorySink::new(),
    );
    assert!(matches!(result, Err(Error::InvalidOptions(_))));
}

#[test]
fn partial_frame_is_rejected() {
    let mut builder = PeakBuilder::new(8000, 2, &Options::default(), MemorySink::new()).unwrap();
    assert!(matches!(
        builder.push_interleaved(&[1, 2, 3]),
        Err(Error::InvalidInput(_))
    ));

    let mut source = RawPcmSource::new(Cursor::new(vec![0u8; 6]), 8000, 2).unwrap();
    let result = generate(
        &mut source,
        &Options::default(),
        &Limits::default(),
        MemorySink::new(),
    );
    assert!(matches!(result, Err(Error::InvalidInput(_))));
}

#[test]
fn limits_stop_oversized_input() {
    let one_second = common::le_bytes(&vec![0; 8000]);
    let mut limits = Limits::default();
    limits.max_channels = 2;
    limits.max_duration = Duration::from_millis(999);
    let run = |sample_rate, channels| {
        let mut source =
            RawPcmSource::new(Cursor::new(one_second.clone()), sample_rate, channels).unwrap();
        generate(&mut source, &Options::default(), &limits, MemorySink::new())
    };

    // The same bytes are one second as 8 kHz mono and as 4 kHz stereo.
    for (sample_rate, channels) in [(8000, 1), (4000, 2)] {
        let result = run(sample_rate, channels);
        assert!(
            matches!(
                result,
                Err(Error::LimitExceeded("duration above max_duration"))
            ),
            "{sample_rate} Hz, {channels} ch: {result:?}"
        );
    }
    let result = run(8000 / 3, 3);
    assert!(
        matches!(
            result,
            Err(Error::LimitExceeded("channel count above max_channels"))
        ),
        "{result:?}"
    );

    // The channel count is checked before the options.
    let mut source = RawPcmSource::new(Cursor::new(Vec::new()), 8000, 3).unwrap();
    let zoom_one = options(1, Bits::Sixteen, ChannelMode::Mixdown);
    let result = generate(&mut source, &zoom_one, &limits, MemorySink::new());
    assert!(
        matches!(
            result,
            Err(Error::LimitExceeded("channel count above max_channels"))
        ),
        "{result:?}"
    );
}

#[test]
fn generate_many_matches_one_run_per_output() {
    // Three blocks of RawPcmSource, the last one partial. Zooms 3 and 1000
    // do not divide a block, so their points span blocks.
    let bytes = common::le_bytes(&common::pcm(40_007, 2, 7));
    let all = [
        options(3, Bits::Sixteen, ChannelMode::Mixdown),
        options(1000, Bits::Eight, ChannelMode::Split),
        options(4096, Bits::Sixteen, ChannelMode::Split),
    ];
    let source = || RawPcmSource::new(Cursor::new(bytes.clone()), 8000, 2).unwrap();

    let outputs = all.map(|options| (options, MemorySink::new()));
    let many = generate_many(&mut source(), &Limits::default(), outputs).unwrap();

    assert_eq!(many.len(), all.len());
    for (waveform, options) in many.iter().zip(&all) {
        let one = generate(
            &mut source(),
            options,
            &Limits::default(),
            MemorySink::new(),
        );
        assert_eq!(waveform, &one.unwrap(), "{options:?}");
    }
}

#[test]
fn generate_many_checks_every_option_before_any_header() {
    let mut source = RawPcmSource::new(Cursor::new(vec![0u8; 3]), 8000, 2).unwrap();
    let (mut first, mut second) = (Vec::new(), Vec::new());
    let outputs = [
        (Options::default(), JsonWriter::new(&mut first)),
        (
            options(1, Bits::Sixteen, ChannelMode::Mixdown),
            JsonWriter::new(&mut second),
        ),
    ];
    let result = generate_many(&mut source, &Limits::default(), outputs);
    assert!(
        matches!(result, Err(Error::InvalidOptions(_))),
        "{result:?}"
    );
    assert!(first.is_empty() && second.is_empty());
}

/// Reports what it is told and panics if read.
struct Unread {
    sample_rate: u32,
    channels: u16,
}

impl PcmSource for Unread {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn next_block(&mut self) -> Result<Option<&[i16]>, Error> {
        panic!("source read");
    }
}

#[test]
fn generate_many_without_outputs_reads_nothing() {
    let mut source = Unread {
        sample_rate: 8000,
        channels: 0,
    };
    let outputs: [(Options, MemorySink); 0] = [];
    let result = generate_many(&mut source, &Limits::default(), outputs);
    assert!(matches!(result.as_deref(), Ok([])), "{result:?}");
}

#[test]
fn source_without_channels_or_rate_is_rejected() {
    for (sample_rate, channels) in [(8000, 0), (0, 1)] {
        let mut source = Unread {
            sample_rate,
            channels,
        };
        let result = generate(
            &mut source,
            &Options::default(),
            &Limits::default(),
            MemorySink::new(),
        );
        assert!(
            matches!(result, Err(Error::InvalidOptions(_))),
            "{sample_rate} Hz, {channels} ch: {result:?}"
        );
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Fail {
    Never,
    OnPoint,
    OnFinish,
}

/// A JSON sink that can fail on its first point or when finishing.
struct Failing<'a> {
    json: JsonWriter<&'a mut Vec<u8>>,
    fail: Fail,
}

impl PeakSink for Failing<'_> {
    type Output = ();

    fn begin(&mut self, header: &Header) -> Result<(), Error> {
        self.json.begin(header)
    }

    fn point(&mut self, peaks: &[Peak]) -> Result<(), Error> {
        match self.fail {
            Fail::OnPoint => Err(Error::Io(io::Error::other("point"))),
            _ => self.json.point(peaks),
        }
    }

    fn finish(self, length: u32) -> Result<(), Error> {
        match self.fail {
            Fail::OnFinish => Err(Error::Io(io::Error::other("finish"))),
            _ => self.json.finish(length).map(drop),
        }
    }
}

#[test]
fn generate_many_passes_sink_errors_through() {
    let bytes = common::le_bytes(&common::pcm(1000, 1, 3));
    let finished = |json: Vec<u8>| String::from_utf8(json).unwrap().ends_with("\"length\":4}");
    for fail in [Fail::OnPoint, Fail::OnFinish] {
        for failing_first in [false, true] {
            let mut source = RawPcmSource::new(Cursor::new(bytes.clone()), 8000, 1).unwrap();
            let (mut failing, mut other) = (Vec::new(), Vec::new());
            let failing_sink = Failing {
                json: JsonWriter::new(&mut failing),
                fail,
            };
            let other_sink = Failing {
                json: JsonWriter::new(&mut other),
                fail: Fail::Never,
            };
            let sinks = if failing_first {
                [failing_sink, other_sink]
            } else {
                [other_sink, failing_sink]
            };
            let outputs = sinks.map(|sink| (Options::default(), sink));
            let result = generate_many(&mut source, &Limits::default(), outputs);
            let context = format!("{fail:?}, failing first: {failing_first}");
            assert!(matches!(result, Err(Error::Io(_))), "{context}: {result:?}");
            // Only a sink that fails when finishing lets the sinks before it
            // finish.
            let expected = fail == Fail::OnFinish && !failing_first;
            assert_eq!(finished(other), expected, "{context}");
            assert!(!finished(failing), "{context}");
        }
    }
}
