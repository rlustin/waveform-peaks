//! Decoded audio inputs.

#[cfg(feature = "aac")]
mod aac;
#[cfg(feature = "aac")]
mod adts_aac;
#[cfg(any(feature = "opus", feature = "aac", feature = "mp3"))]
mod mp4;
#[cfg(feature = "mp3")]
mod mpa;
#[cfg(feature = "mp3")]
mod mpeg_audio;
#[cfg(feature = "opus")]
mod ogg_opus;
#[cfg(feature = "opus")]
mod opus;
#[cfg(any(feature = "aac", feature = "mp3"))]
mod symphonia;
#[cfg(feature = "opus")]
mod webm_opus;

use std::fmt;
#[cfg(any(feature = "opus", feature = "aac", feature = "mp3"))]
use std::io::{self, BufReader, Seek, SeekFrom};
use std::io::{ErrorKind, Read};

#[cfg(feature = "aac")]
pub use adts_aac::AdtsAacSource;
#[cfg(feature = "aac")]
#[allow(deprecated)]
pub use mp4::Mp4AacSource;
#[cfg(any(feature = "opus", feature = "aac", feature = "mp3"))]
pub use mp4::{Mp4Options, Mp4Source};
#[cfg(feature = "mp3")]
pub use mpeg_audio::MpegAudioSource;
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

#[cfg(any(feature = "aac", feature = "mp3"))]
/// Skips ID3v2 tags, which some encoders put before the first frame of an
/// ADTS or MPEG audio stream. Returns the bytes read past them, the start of
/// the first frame header, and their count.
pub(crate) fn skip_id3v2<R: Read>(reader: &mut R) -> Result<([u8; 3], usize), Error> {
    loop {
        let mut start = [0u8; 3];
        let read = read_full(reader, &mut start)?;
        if &start[..read] != b"ID3" {
            return Ok((start, read));
        }
        let mut header = [0u8; 10];
        header[..3].copy_from_slice(b"ID3");
        if read_full(reader, &mut header[3..])? < header.len() - 3 {
            return Err(invalid("truncated ID3v2 tag"));
        }
        // A syncsafe size: 7 bits per byte. A footer adds 10 bytes.
        let size = header[6..10]
            .iter()
            .fold(0u64, |size, &byte| size << 7 | (byte & 0x7F) as u64);
        let footer = if header[5] & 0x10 != 0 { 10 } else { 0 };
        let len = size + footer;
        let skipped = io::copy(&mut reader.by_ref().take(len), &mut io::sink())?;
        if skipped < len {
            return Err(invalid("truncated ID3v2 tag"));
        }
    }
}

#[cfg(any(feature = "opus", feature = "aac", feature = "mp3"))]
pub(crate) fn invalid(message: &str) -> Error {
    Error::InvalidInput(message.to_owned())
}

/// Buffers a plain `Read` and counts the bytes consumed, so that containers
/// can tell where a parent of known size ends without `Seek`.
#[cfg(any(feature = "opus", feature = "aac", feature = "mp3"))]
pub(crate) struct CountingReader<R> {
    reader: BufReader<R>,
    position: u64,
    /// Error message for input that ends inside a structure.
    truncated: &'static str,
    /// Set by [`seek_to_skip`](Self::seek_to_skip).
    seeker: Option<Seeker<R>>,
}

/// Shorter skips read through: a reader over the network may make a request
/// for each seek.
#[cfg(any(feature = "opus", feature = "aac", feature = "mp3"))]
const MIN_SEEK: u64 = 64 * 1024;

/// Lets a reader of any type skip by seeking, once it is known to be `Seek`.
#[cfg(any(feature = "opus", feature = "aac", feature = "mp3"))]
struct Seeker<R> {
    /// Input length, counted like [`CountingReader::position`].
    len: u64,
    /// Where the input starts in the underlying reader.
    start: u64,
    /// Whether reading was seen to move the underlying reader.
    checked: bool,
    seek_relative: fn(&mut BufReader<R>, i64) -> io::Result<()>,
    stream_position: fn(&mut R) -> io::Result<u64>,
}

#[cfg(any(feature = "opus", feature = "aac", feature = "mp3"))]
impl<R: Read> CountingReader<R> {
    pub(crate) fn new(reader: R, truncated: &'static str) -> Self {
        CountingReader {
            reader: BufReader::new(reader),
            position: 0,
            truncated,
            seeker: None,
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
        if len >= MIN_SEEK
            && let Some((input_len, seek_relative)) = self
                .checked_seeker()
                .map(|seeker| (seeker.len, seeker.seek_relative))
        {
            // Past the end, reading through would fail too.
            if len > input_len.saturating_sub(self.position) {
                return Err(invalid(self.truncated));
            }
            let offset = i64::try_from(len).map_err(|_| invalid("offset out of range"))?;
            seek_relative(&mut self.reader, offset)?;
            self.position += len;
            return Ok(());
        }
        let skipped = io::copy(&mut (&mut self.reader).take(len), &mut io::sink())?;
        self.position += skipped;
        if skipped < len {
            return Err(invalid(self.truncated));
        }
        Ok(())
    }

    pub(crate) fn skip_to_end(&mut self) -> Result<(), Error> {
        if let Some(len) = self.checked_seeker().map(|seeker| seeker.len) {
            return self.skip(len.saturating_sub(self.position));
        }
        self.position += io::copy(&mut self.reader, &mut io::sink())?;
        Ok(())
    }

    /// The seeker, once the underlying reader is seen to be at the end of
    /// what was read from it. A Windows pipe seeks without an error, but
    /// neither seeking nor reading moves it, and its end is the data waiting
    /// in it: it is then read through.
    fn checked_seeker(&mut self) -> Option<&Seeker<R>> {
        let seeker = self.seeker.as_mut()?;
        if !seeker.checked {
            let read = seeker.start + self.position + self.reader.buffer().len() as u64;
            if (seeker.stream_position)(self.reader.get_mut()).ok() != Some(read) {
                self.seeker = None;
                return None;
            }
            seeker.checked = true;
        }
        self.seeker.as_ref()
    }
}

#[cfg(any(feature = "opus", feature = "aac", feature = "mp3"))]
impl<R: Read + Seek> CountingReader<R> {
    /// Moves to `position`, counted like [`position`](Self::position). Seeks
    /// relative to the current offset, so the input may start anywhere in
    /// the underlying reader.
    pub(crate) fn seek_to(&mut self, position: u64) -> Result<(), Error> {
        let offset = i64::try_from(position as i128 - self.position as i128)
            .map_err(|_| invalid("offset out of range"))?;
        self.reader.seek_relative(offset)?;
        self.position = position;
        Ok(())
    }

    /// Makes [`skip`](Self::skip) and [`skip_to_end`](Self::skip_to_end) seek
    /// past long spans instead of reading through. The input must not grow
    /// afterwards.
    pub(crate) fn seek_to_skip(&mut self) -> Result<(), Error> {
        // A Unix pipe opened as a file is `Seek` but cannot seek: it still
        // reads a fast-start file through.
        let Ok(current) = self.reader.stream_position() else {
            return Ok(());
        };
        let Ok(end) = self.reader.seek(SeekFrom::End(0)) else {
            return Ok(());
        };
        self.reader.seek(SeekFrom::Start(current))?;
        self.seeker = Some(Seeker {
            len: self.position + end.saturating_sub(current),
            start: current.saturating_sub(self.position),
            checked: false,
            seek_relative: BufReader::seek_relative,
            stream_position: R::stream_position,
        });
        Ok(())
    }
}
