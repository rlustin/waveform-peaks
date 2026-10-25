//! Constant-memory generation of audiowaveform-compatible peak data.
//!
//! Audio is pulled from a [`PcmSource`] one decoded block at a time, reduced
//! to `(min, max)` pairs by a [`PeakBuilder`], and pushed point by point into a
//! [`PeakSink`]. No stage holds more than one block, so memory does not grow
//! with the input: an hour-long upload uses the same memory as a second.
//!
//! Sources are [`RawPcmSource`] for raw PCM; with the default `opus` feature,
//! [`OggOpusSource`] and [`WebmOpusSource`] for Opus in Ogg, WebM and Matroska;
//! with the optional `aac` feature, `AdtsAacSource` for AAC in ADTS; with the
//! optional `mp3` feature, `MpegAudioSource` for MP1, MP2 and MP3 files; and
//! with any of them, `Mp4Source` for these codecs in MP4.
//!
//! For the same options the output matches BBC audiowaveform byte for byte on
//! PCM input. On Opus input with `Overflow::Wrap` it is within ±1 per value,
//! because the bundled libopus build differs from audiowaveform's in the last
//! bit (see `tests/oracle.rs`).
//!
//! [`generate_many`] decodes the input once for several outputs, such as
//! several zoom levels.
//!
//! Points are written as they complete, so a failed run leaves a partial file
//! behind. Write to a temporary path and rename it once [`generate`] or
//! [`generate_many`] succeeds.
//!
//! ```no_run
//! # #[cfg(feature = "opus")]
//! # fn main() -> Result<(), waveform_peaks::Error> {
//! use std::fs::File;
//! use waveform_peaks::{generate, DatWriter, Limits, OggOpusSource, Options};
//!
//! let mut source = OggOpusSource::new(File::open("upload.opus")?)?;
//! let output = File::create("upload.dat")?;
//! generate(&mut source, &Options::default(), &Limits::default(), DatWriter::new(output))?;
//! # Ok(())
//! # }
//! # #[cfg(not(feature = "opus"))]
//! # fn main() {}
//! ```
//!
//! # Async servers
//!
//! Decoding is CPU-bound, so the API is synchronous. In an async server, run
//! [`generate`] on a blocking thread and read the upload through a bridge. With
//! Tokio and `tokio-util`:
//!
//! ```ignore
//! use tokio::io::AsyncRead;
//! use tokio_util::io::SyncIoBridge;
//! use waveform_peaks::{generate, JsonWriter, Limits, OggOpusSource, Options};
//!
//! async fn peaks(
//!     upload: impl AsyncRead + Unpin + Send + 'static,
//! ) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
//!     let upload = SyncIoBridge::new(upload);
//!     let json = tokio::task::spawn_blocking(move || {
//!         let mut source = OggOpusSource::new(upload)?;
//!         let sink = JsonWriter::new(Vec::new());
//!         generate(&mut source, &Options::default(), &Limits::default(), sink)
//!     })
//!     .await??;
//!     Ok(json)
//! }
//! ```

#![cfg_attr(docsrs, feature(doc_cfg))]

mod builder;
mod error;
mod options;
mod sink;
pub mod source;

pub use builder::{Header, Peak, PeakBuilder};
pub use error::Error;
pub use options::{Bits, ChannelMode, Limits, Options};
pub use sink::{DatWriter, JsonWriter, MemorySink, PeakSink, Waveform};
#[cfg(feature = "aac")]
pub use source::AdtsAacSource;
#[cfg(feature = "aac")]
#[allow(deprecated)]
pub use source::Mp4AacSource;
#[cfg(feature = "mp3")]
pub use source::MpegAudioSource;
#[cfg(feature = "opus")]
pub use source::{DecodeRate, OggOpusSource, OpusOptions, Overflow, WebmOpusSource};
#[cfg(any(feature = "opus", feature = "aac", feature = "mp3"))]
pub use source::{Mp4Options, Mp4Source};
pub use source::{PcmSource, RawPcmSource};

/// Drains `source` into `sink` and returns the sink's output.
///
/// # Errors
///
/// - [`Error::LimitExceeded`] when the source has more channels or more audio
///   than `limits` allow. The duration check runs as blocks arrive, so an
///   oversized input stops early.
/// - Errors from the source, the builder, and the sink pass through. The sink
///   has received the points decoded before the error and is not finished: a
///   `.dat` file keeps a zero length in its header.
pub fn generate<P: PcmSource + ?Sized, S: PeakSink>(
    source: &mut P,
    options: &Options,
    limits: &Limits,
    sink: S,
) -> Result<S::Output, Error> {
    let mut outputs = generate_many(source, limits, [(*options, sink)])?;
    Ok(outputs.pop().expect("one output per sink"))
}

/// Drains `source` once into several sinks, each with its own options, and
/// returns their outputs in the same order.
///
/// Decoding is most of the cost of [`generate`], so this is cheaper than one
/// run per zoom level. Memory grows with the number of outputs, not with the
/// input. For sinks of different types, such as [`DatWriter`] and
/// [`JsonWriter`], wrap them in an enum that implements [`PeakSink`].
///
/// ```no_run
/// # #[cfg(feature = "opus")]
/// # fn main() -> Result<(), waveform_peaks::Error> {
/// use std::fs::File;
/// use waveform_peaks::{generate_many, DatWriter, Limits, OggOpusSource, Options};
///
/// let mut source = OggOpusSource::new(File::open("upload.opus")?)?;
/// let mut outputs = Vec::new();
/// for zoom in [256, 4096] {
///     let mut options = Options::default();
///     options.samples_per_pixel = zoom;
///     let output = File::create(format!("upload-{zoom}.dat"))?;
///     outputs.push((options, DatWriter::new(output)));
/// }
/// generate_many(&mut source, &Limits::default(), outputs)?;
/// # Ok(())
/// # }
/// # #[cfg(not(feature = "opus"))]
/// # fn main() {}
/// ```
///
/// With no outputs, returns an empty `Vec` without reading the source.
///
/// # Errors
///
/// As for [`generate`]. Every option is checked before any sink receives its
/// header. Sinks are finished in order: when finishing one fails, the sinks
/// before it are finished, the sinks after it are not, and all their outputs
/// are dropped. Thus a `.dat` file can be complete although the call fails.
pub fn generate_many<P, S, I>(
    source: &mut P,
    limits: &Limits,
    outputs: I,
) -> Result<Vec<S::Output>, Error>
where
    P: PcmSource + ?Sized,
    S: PeakSink,
    I: IntoIterator<Item = (Options, S)>,
{
    let outputs: Vec<(Options, S)> = outputs.into_iter().collect();
    if outputs.is_empty() {
        return Ok(Vec::new());
    }
    let channels = source.channels();
    if channels > limits.max_channels {
        return Err(Error::LimitExceeded("channel count above max_channels"));
    }
    for (options, _) in &outputs {
        options.check()?;
    }
    let sample_rate = source.sample_rate();
    // Float-to-int casts saturate, so Duration::MAX means no limit.
    let max_frames = (limits.max_duration.as_secs_f64() * sample_rate as f64) as u64;

    let mut builders = outputs
        .into_iter()
        .map(|(options, sink)| PeakBuilder::new(sample_rate, channels, &options, sink))
        .collect::<Result<Vec<_>, _>>()?;
    let mut frames: u64 = 0;
    while let Some(block) = source.next_block()? {
        frames += (block.len() / channels as usize) as u64;
        if frames > max_frames {
            return Err(Error::LimitExceeded("duration above max_duration"));
        }
        for builder in &mut builders {
            builder.push_interleaved(block)?;
        }
    }
    builders.into_iter().map(PeakBuilder::finish).collect()
}
