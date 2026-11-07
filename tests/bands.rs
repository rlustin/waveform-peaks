//! Frequency band levels (`Options::bands`).

mod common;

use std::f64::consts::PI;
use std::io::Cursor;

use common::{le_bytes, options, pcm};
use waveform_peaks::{
    BandJsonWriter, Bands, Bits, ChannelMode, Crossover, DatWriter, Error, Header, JsonWriter,
    Limits, MemorySink, Options, Peak, PeakBuilder, PeakSink, RawPcmSource, Waveform, generate,
};

const RATE: u32 = 48000;

fn with_bands(mut options: Options) -> Options {
    options.bands = Some(Crossover::default());
    options
}

fn waveform(samples: &[i16], sample_rate: u32, channels: u16, options: &Options) -> Waveform {
    let mut builder = PeakBuilder::new(sample_rate, channels, options, MemorySink::new()).unwrap();
    builder.push_interleaved(samples).unwrap();
    builder.finish().unwrap()
}

fn sine(hz: f64, frames: usize, amplitude: f64) -> Vec<i16> {
    (0..frames)
        .map(|i| (amplitude * (2.0 * PI * hz * i as f64 / f64::from(RATE)).sin()).round() as i16)
        .collect()
}

/// The band levels of the last point, after the filters settle.
fn settled(samples: &[i16]) -> Bands {
    let options = with_bands(options(4800, Bits::Sixteen, ChannelMode::Mixdown));
    *waveform(samples, RATE, 1, &options).bands().last().unwrap()
}

#[test]
fn each_tone_lands_in_its_band() {
    let amplitude = 10000.0;
    // The RMS of a sine is its amplitude divided by √2.
    let rms = (amplitude / 2f64.sqrt()) as u16;
    for (target, hz) in [50.0, 630.0, 8000.0].into_iter().enumerate() {
        let bands = settled(&sine(hz, RATE as usize, amplitude));
        let levels = [bands.low, bands.mid, bands.high];
        for (index, level) in levels.into_iter().enumerate() {
            if index == target {
                assert!(level.abs_diff(rms) < rms / 50, "{hz} Hz: {bands:?}");
            } else {
                assert!(level < rms / 10, "{hz} Hz leaks: {bands:?}");
            }
        }
    }
}

#[test]
fn bands_do_not_change_peaks() {
    let samples = pcm(10_001, 2, 7);
    for mode in [ChannelMode::Mixdown, ChannelMode::Split] {
        for bits in [Bits::Eight, Bits::Sixteen] {
            let plain = options(300, bits, mode);
            let without = waveform(&samples, RATE, 2, &plain);
            let with = waveform(&samples, RATE, 2, &with_bands(plain));
            assert_eq!(without.data(), with.data());
            assert!(without.bands().is_empty());
            let channels = with.header().channels as usize;
            assert_eq!(with.bands().len(), with.length() as usize * channels);
        }
    }
}

#[test]
fn split_measures_each_channel() {
    let low = sine(50.0, RATE as usize, 10000.0);
    let high = sine(8000.0, RATE as usize, 10000.0);
    let samples: Vec<i16> = low.iter().zip(&high).flat_map(|(&l, &h)| [l, h]).collect();
    let options = with_bands(options(4800, Bits::Sixteen, ChannelMode::Split));
    let waveform = waveform(&samples, RATE, 2, &options);
    let [left, right] = waveform.bands()[waveform.bands().len() - 2..] else {
        unreachable!()
    };
    assert!(left.low > 10 * left.high, "{left:?}");
    assert!(right.high > 10 * right.low, "{right:?}");
}

#[test]
fn blocks_can_split_anywhere() {
    let samples = pcm(5000, 1, 3);
    let options = with_bands(options(256, Bits::Sixteen, ChannelMode::Mixdown));
    let whole = waveform(&samples, RATE, 1, &options);
    let mut builder = PeakBuilder::new(RATE, 1, &options, MemorySink::new()).unwrap();
    for block in samples.chunks(97) {
        builder.push_interleaved(block).unwrap();
    }
    assert_eq!(builder.finish().unwrap(), whole);
}

#[test]
fn trailing_partial_point_has_bands() {
    let options = with_bands(options(4, Bits::Sixteen, ChannelMode::Mixdown));
    let waveform = waveform(&[1000; 6], RATE, 1, &options);
    assert_eq!(waveform.length(), 2);
    assert_eq!(waveform.bands().len(), 2);
}

#[test]
fn bad_crossover_is_rejected() {
    for (low_hz, high_hz) in [(0, 2000), (2000, 2000), (3000, 2000)] {
        let mut options = Options::default();
        let mut crossover = Crossover::default();
        crossover.low_hz = low_hz;
        crossover.high_hz = high_hz;
        options.bands = Some(crossover);
        let result = PeakBuilder::new(RATE, 1, &options, MemorySink::new());
        assert!(matches!(result, Err(Error::InvalidOptions(_))));
    }
}

#[test]
fn band_json_writer_keeps_peaks_byte_for_byte() {
    let samples = pcm(3001, 2, 11);
    let plain = options(256, Bits::Sixteen, ChannelMode::Split);
    let source = || RawPcmSource::new(Cursor::new(le_bytes(&samples)), RATE, 2).unwrap();
    let expected = generate(
        &mut source(),
        &plain,
        &Limits::default(),
        DatWriter::new(Cursor::new(Vec::new())),
    )
    .unwrap();
    let sink = BandJsonWriter::new(DatWriter::new(Cursor::new(Vec::new())), Vec::new());
    let (dat, json) =
        generate(&mut source(), &with_bands(plain), &Limits::default(), sink).unwrap();
    assert_eq!(dat.into_inner(), expected.into_inner());

    let json: serde_json::Value = serde_json::from_slice(&json).unwrap();
    let length = json["length"].as_u64().unwrap();
    assert_eq!(length, 12);
    assert_eq!(json["version"], 1);
    assert_eq!(json["channels"], 2);
    assert_eq!(json["sample_rate"], RATE);
    assert_eq!(json["samples_per_pixel"], 256);
    assert_eq!(json["crossover_hz"], serde_json::json!([200, 2000]));
    assert_eq!(
        json["data"].as_array().unwrap().len() as u64,
        length * 2 * 3
    );
}

#[test]
fn band_json_writer_matches_memory_sink() {
    let samples = pcm(2000, 1, 5);
    let options = with_bands(options(300, Bits::Sixteen, ChannelMode::Mixdown));
    let expected = waveform(&samples, RATE, 1, &options);
    let sink = BandJsonWriter::new(JsonWriter::new(Vec::new()), Vec::new());
    let mut builder = PeakBuilder::new(RATE, 1, &options, sink).unwrap();
    builder.push_interleaved(&samples).unwrap();
    let (_, json) = builder.finish().unwrap();
    let json: serde_json::Value = serde_json::from_slice(&json).unwrap();
    let data: Vec<u16> = json["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_u64().unwrap() as u16)
        .collect();
    let levels: Vec<u16> = expected
        .bands()
        .iter()
        .flat_map(|b| [b.low, b.mid, b.high])
        .collect();
    assert_eq!(data, levels);
}

#[test]
fn band_json_writer_without_bands_writes_nothing() {
    let sink = BandJsonWriter::new(JsonWriter::new(Vec::new()), Vec::new());
    let result = PeakBuilder::new(RATE, 1, &Options::default(), sink);
    assert!(matches!(result, Err(Error::InvalidOptions(_))));
}

/// Forwards every method, as the `PeakSink` documentation asks.
struct Forwarding<S>(S);

/// Forwards only the methods without a default.
struct Partial<S>(S);

impl<S: PeakSink> PeakSink for Forwarding<S> {
    type Output = S::Output;

    fn begin(&mut self, header: &Header) -> Result<(), Error> {
        self.0.begin(header)
    }

    fn point(&mut self, peaks: &[Peak]) -> Result<(), Error> {
        self.0.point(peaks)
    }

    fn point_with_bands(&mut self, peaks: &[Peak], bands: &[Bands]) -> Result<(), Error> {
        self.0.point_with_bands(peaks, bands)
    }

    fn finish(self, length: u32) -> Result<S::Output, Error> {
        self.0.finish(length)
    }
}

impl<S: PeakSink> PeakSink for Partial<S> {
    type Output = S::Output;

    fn begin(&mut self, header: &Header) -> Result<(), Error> {
        self.0.begin(header)
    }

    fn point(&mut self, peaks: &[Peak]) -> Result<(), Error> {
        self.0.point(peaks)
    }

    fn finish(self, length: u32) -> Result<S::Output, Error> {
        self.0.finish(length)
    }
}

fn run<S: PeakSink>(samples: &[i16], options: &Options, sink: S) -> Result<S::Output, Error> {
    let mut builder = PeakBuilder::new(RATE, 1, options, sink)?;
    builder.push_interleaved(samples)?;
    builder.finish()
}

#[test]
fn wrapping_sinks_must_forward_bands() {
    let samples = pcm(1000, 1, 19);
    let options = with_bands(options(100, Bits::Sixteen, ChannelMode::Mixdown));
    let expected = waveform(&samples, RATE, 1, &options);

    let forwarded = run(&samples, &options, Forwarding(MemorySink::new())).unwrap();
    assert_eq!(forwarded, expected);
    let band_sink = BandJsonWriter::new(JsonWriter::new(Vec::new()), Vec::new());
    let (_, json) = run(&samples, &options, Forwarding(band_sink)).unwrap();
    let json: serde_json::Value = serde_json::from_slice(&json).unwrap();
    assert_eq!(json["length"], expected.length());

    // Without forwarding, the default drops the levels.
    let dropped = run(&samples, &options, Partial(MemorySink::new())).unwrap();
    assert_eq!(dropped.data(), expected.data());
    assert!(dropped.bands().is_empty());
    let band_sink = BandJsonWriter::new(JsonWriter::new(Vec::new()), Vec::new());
    let error = run(&samples, &options, Partial(band_sink)).unwrap_err();
    assert!(
        matches!(error, Error::InvalidOptions(message) if message.contains("point_with_bands")),
        "{error}"
    );
}

// Hostile inputs.

#[test]
fn full_scale_nyquist_saturates_instead_of_wrapping() {
    let samples: Vec<i16> = (0..RATE)
        .map(|i| if i % 2 == 0 { i16::MIN } else { i16::MAX })
        .collect();
    let bands = settled(&samples);
    assert!(bands.high >= 32767, "{bands:?}");
    assert!(bands.low < 10 && bands.mid < 100, "{bands:?}");
}

#[test]
fn crossover_above_nyquist_silences_the_band() {
    // At 8 kHz, Nyquist is 4 kHz: a 5 kHz high crossover leaves no high band.
    let mut options = with_bands(options(800, Bits::Sixteen, ChannelMode::Mixdown));
    options.bands.as_mut().unwrap().high_hz = 5000;
    let samples = pcm(8000, 1, 9);
    let bands = waveform(&samples, 8000, 1, &options).bands().to_vec();
    assert!(bands.iter().all(|b| b.high == 0));
    assert!(bands.iter().any(|b| b.mid > 0));

    // At 1 Hz, every crossover is past Nyquist: all of the signal is low.
    let bands = waveform(&samples, 1, 1, &with_bands(options))
        .bands()
        .to_vec();
    assert!(bands.iter().all(|b| b.mid == 0 && b.high == 0));
    assert!(bands.iter().any(|b| b.low > 0));
}

#[test]
fn extreme_rates_and_zooms_stay_finite() {
    let samples = pcm(100_000, 1, 13);
    for (sample_rate, zoom) in [(u32::MAX, 256), (1, 2), (RATE, u32::MAX), (RATE, 2)] {
        let mut options = with_bands(options(zoom, Bits::Sixteen, ChannelMode::Mixdown));
        options.bands = Some({
            let mut crossover = Crossover::default();
            crossover.low_hz = 1;
            crossover.high_hz = u32::MAX;
            crossover
        });
        let waveform = waveform(&samples, sample_rate, 1, &options);
        assert_eq!(waveform.bands().len(), waveform.length() as usize);
        // Levels are the RMS of a filtered 16-bit signal, so a level far
        // above full scale means the filter is unstable.
        assert!(
            waveform
                .bands()
                .iter()
                .all(|b| b.low < 40000 && b.mid < 40000),
            "{sample_rate} Hz, zoom {zoom}"
        );
    }
}

#[test]
fn silence_after_signal_settles_to_zero() {
    let mut samples = pcm(RATE as usize, 1, 17);
    samples.resize(3 * RATE as usize, 0);
    let options = with_bands(options(4800, Bits::Sixteen, ChannelMode::Mixdown));
    let waveform = waveform(&samples, RATE, 1, &options);
    assert_eq!(*waveform.bands().last().unwrap(), Bands::default());
}
