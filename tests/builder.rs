//! Tool-free tests. Expected values come from probing audiowaveform 1.11.1
//! with raw PCM input; `tests/oracle.rs` checks the same rules against the
//! binary itself.

mod common;

use std::io::Cursor;
use std::time::Duration;

use common::options;

use waveform_peaks::{
    Bits, ChannelMode, DatWriter, Error, JsonWriter, Limits, MemorySink, Options, PeakBuilder,
    RawPcmSource, generate,
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
}
