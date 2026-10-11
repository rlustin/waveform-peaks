//! Constant-memory generation of audiowaveform-compatible peak data.
//!
//! Audio is pulled from a [`PcmSource`] one decoded block at a time, reduced
//! to `(min, max)` pairs by a [`PeakBuilder`], and pushed point by point into a
//! [`PeakSink`]. No stage holds more than one block, so memory does not grow
//! with the input: an hour-long upload uses the same memory as a second.
//!
//! For the same options the output matches BBC audiowaveform byte for byte on
//! PCM input. On Opus input with `Overflow::Wrap` it is within ±1 per value,
//! because the bundled libopus build differs from audiowaveform's in the last
//! bit (see `tests/oracle.rs`).
//!
//! Points are written as they complete, so a failed run leaves a partial file
//! behind. Write to a temporary path and rename it once [`generate`] succeeds.
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
#[cfg(feature = "opus")]
pub use source::{DecodeRate, OggOpusSource, OpusOptions, Overflow};
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
pub fn generate<P: PcmSource, S: PeakSink>(
    source: &mut P,
    options: &Options,
    limits: &Limits,
    sink: S,
) -> Result<S::Output, Error> {
    let channels = source.channels();
    if channels > limits.max_channels {
        return Err(Error::LimitExceeded("channel count above max_channels"));
    }
    let sample_rate = source.sample_rate();
    // Float-to-int casts saturate, so Duration::MAX means no limit.
    let max_frames = (limits.max_duration.as_secs_f64() * sample_rate as f64) as u64;

    let mut builder = PeakBuilder::new(sample_rate, channels, options, sink)?;
    let mut frames: u64 = 0;
    while let Some(block) = source.next_block()? {
        frames += (block.len() / channels as usize) as u64;
        if frames > max_frames {
            return Err(Error::LimitExceeded("duration above max_duration"));
        }
        builder.push_interleaved(block)?;
    }
    builder.finish()
}
