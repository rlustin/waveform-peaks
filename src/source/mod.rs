//! Decoded audio inputs.

#[cfg(feature = "aac")]
mod aac;
#[cfg(feature = "aac")]
mod adts_aac;
#[cfg(feature = "aac")]
mod mp4;
#[cfg(feature = "opus")]
mod ogg_opus;
#[cfg(feature = "opus")]
mod opus;
#[cfg(feature = "opus")]
mod webm_opus;

use std::fmt;
#[cfg(any(feature = "opus", feature = "aac"))]
use std::io::{self, BufReader};
use std::io::{ErrorKind, Read};

#[cfg(feature = "aac")]
pub use adts_aac::AdtsAacSource;
#[cfg(feature = "aac")]
#[allow(deprecated)]
pub use mp4::Mp4AacSource;
#[cfg(feature = "aac")]
pub use mp4::{Mp4Options, Mp4Source};
#[cfg(feature = "opus")]
pub use ogg_opus::OggOpusSource;
#[cfg(feature = "opus")]
pub use opus::{DecodeRate, OpusOptions, Overflow};
#[cfg(feature = "opus")]
pub use webm_opus::WebmOpusSource;

use crate::Error;

/// A pull-based source of interleaved 16-bit PCM.
///
/// Each call to [`next_block`](Self::next_block) returns the next block of
/// whole frames, borrowed from the source's own buffer, so a source holds at
/// most one block in memory.
pub trait PcmSource {
    /// Frames per second.
    fn sample_rate(&self) -> u32;

    /// Interleaved channel count.
    fn channels(&self) -> u16;

    /// Returns the next block, or `None` at the end of the input. A block may
    /// be empty only if more blocks follow.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when reading fails, [`Error::InvalidInput`] for
    /// malformed input, and [`Error::LimitExceeded`] when the input passes a
    /// bound set on the source.
    fn next_block(&mut self) -> Result<Option<&[i16]>, Error>;
}

/// Raw interleaved signed 16-bit little-endian PCM (audiowaveform
/// `--input-format raw --raw-format s16le`).
pub struct RawPcmSource<R: Read> {
    reader: R,
    sample_rate: u32,
    channels: u16,
    bytes: Vec<u8>,
    samples: Vec<i16>,
}

impl<R: Read> RawPcmSource<R> {
    const BLOCK_BYTES: usize = 64 * 1024;

    /// Reads PCM with the given layout from `reader`.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidOptions`] when `sample_rate` or `channels` is zero.
    pub fn new(reader: R, sample_rate: u32, channels: u16) -> Result<Self, Error> {
        if sample_rate == 0 || channels == 0 {
            return Err(Error::InvalidOptions(
                "raw PCM needs a positive sample rate and channel count",
            ));
        }
        let frame_bytes = channels as usize * 2;
        let block_bytes = (Self::BLOCK_BYTES / frame_bytes).max(1) * frame_bytes;
        Ok(RawPcmSource {
            reader,
            sample_rate,
            channels,
            bytes: vec![0; block_bytes],
            samples: Vec::with_capacity(block_bytes / 2),
        })
    }
}

impl<R: Read> fmt::Debug for RawPcmSource<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RawPcmSource")
            .field("sample_rate", &self.sample_rate)
            .field("channels", &self.channels)
            .finish_non_exhaustive()
    }
}

impl<R: Read> PcmSource for RawPcmSource<R> {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn next_block(&mut self) -> Result<Option<&[i16]>, Error> {
        let filled = read_full(&mut self.reader, &mut self.bytes)?;
        if filled == 0 {
            return Ok(None);
        }
        let frame_bytes = self.channels as usize * 2;
        if filled % frame_bytes != 0 {
            return Err(Error::InvalidInput(format!(
                "raw PCM ends with a partial frame ({} trailing bytes)",
                filled % frame_bytes
            )));
        }
        self.samples.clear();
        self.samples.extend(
            self.bytes[..filled]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&pair| i16::from_le_bytes(pair)),
        );
        Ok(Some(&self.samples))
    }
}

/// Reads until `buf` is full or the reader is exhausted. Returns the byte count.
pub(crate) fn read_full<R: Read>(reader: &mut R, buf: &mut [u8]) -> Result<usize, Error> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(err) if err.kind() == ErrorKind::Interrupted => {}
            Err(err) => return Err(err.into()),
        }
    }
    Ok(filled)
}

#[cfg(any(feature = "opus", feature = "aac"))]
pub(crate) fn invalid(message: &str) -> Error {
    Error::InvalidInput(message.to_owned())
}

/// Buffers a plain `Read` and counts the bytes consumed, so that containers
/// can tell where a parent of known size ends without `Seek`.
#[cfg(any(feature = "opus", feature = "aac"))]
pub(crate) struct CountingReader<R> {
    reader: BufReader<R>,
    position: u64,
    /// Error message for input that ends inside a structure.
    truncated: &'static str,
}

#[cfg(any(feature = "opus", feature = "aac"))]
impl<R: Read> CountingReader<R> {
    pub(crate) fn new(reader: R, truncated: &'static str) -> Self {
        CountingReader {
            reader: BufReader::new(reader),
            position: 0,
            truncated,
        }
    }

    /// Bytes consumed so far.
    pub(crate) fn position(&self) -> u64 {
        self.position
    }

    /// Reads until `buf` is full or the input ends. Returns the byte count.
    pub(crate) fn read_full(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
        let read = read_full(&mut self.reader, buf)?;
        self.position += read as u64;
        Ok(read)
    }

    pub(crate) fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), Error> {
        if self.read_full(buf)? < buf.len() {
            return Err(invalid(self.truncated));
        }
        Ok(())
    }

    pub(crate) fn skip(&mut self, len: u64) -> Result<(), Error> {
        let skipped = io::copy(&mut (&mut self.reader).take(len), &mut io::sink())?;
        self.position += skipped;
        if skipped < len {
            return Err(invalid(self.truncated));
        }
        Ok(())
    }

    #[cfg(feature = "aac")]
    pub(crate) fn skip_to_end(&mut self) -> Result<(), Error> {
        self.position += io::copy(&mut self.reader, &mut io::sink())?;
        Ok(())
    }
}
