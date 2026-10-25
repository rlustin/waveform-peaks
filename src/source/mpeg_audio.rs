//! MPEG audio streams, the framing of `.mp3` and `.mp2` files, through
//! Symphonia.

use std::fmt;
use std::io::Read;

use super::mpa::{DECODER_DELAY, HEADER_BYTES, Header, MpaDecoder};
use super::{read_full, skip_id3v2};
use crate::{Error, PcmSource};

/// Largest frame: MPEG-2.5 layer II at 160 kbit/s and 8 kHz, with padding.
const MAX_FRAME_BYTES: usize = 2881;

/// Streams PCM out of an MPEG audio file (`.mp3`, `.mp2`, `.mp1`), skipping a
/// leading ID3v2 tag.
///
/// Reads with plain `Read`, so an upload body can be decoded as it arrives.
/// Reads are buffered internally. Memory is two frames and one decoded frame,
/// whatever the input length.
///
/// Trims as audiowaveform does: a first frame holding a Xing or Info tag is
/// not decoded, and when its LAME extension was written by LAME or FFmpeg
/// (`Lavc`), the encoder and decoder delay is dropped from the start. The end
/// padding is kept.
///
/// Handles MPEG-1, 2 and 2.5, layers I to III. Free-format bit rates are
/// [`Error::Unsupported`].
///
/// Damaged input is read as libmad, the decoder of audiowaveform, reads it.
/// Data between frames, such as junk or a tag, is skipped: decoding resumes
/// at the next frame header that another header follows. A frame that does
/// not decode is dropped. At the start and after skipped data, a layer III
/// frame is dropped while its main data starts in earlier frames that were
/// not read. A cut file is not an error: an incomplete last frame is dropped.
///
/// As libmad does, frames are looked for through the whole input, so input
/// in another format is read to the end before [`Error::InvalidInput`].
pub struct MpegAudioSource<R: Read> {
    frames: Frames<R>,
    decoder: MpaDecoder,
    /// Bytes of main data read since the start or the last skipped data,
    /// until a layer III frame can be decoded.
    reservoir: Option<usize>,
    /// The frame at the start of `frames`, when it is read but not decoded.
    pending: Option<Header>,
}

impl<R: Read> MpegAudioSource<R> {
    /// Reads up to the first audio frame.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidInput`] when the input holds no complete MPEG audio
    ///   frame.
    /// - [`Error::Unsupported`] for a free-format bit rate.
    pub fn new(mut reader: R) -> Result<Self, Error> {
        let (start, read) = skip_id3v2(&mut reader)?;
        let mut frames = Frames::new(reader, &start[..read]);
        let (header, _) = frames
            .next(None)?
            .ok_or_else(|| Error::InvalidInput("no complete MPEG audio frame".into()))?;
        let mut source = MpegAudioSource {
            decoder: MpaDecoder::new(header)?,
            frames,
            reservoir: Some(0),
            pending: Some(header),
        };
        let frame = source.frames.frame(header.frame_bytes());
        if let Some(delay) = header.info_tag(frame) {
            if let Some(delay) = delay {
                source.decoder.trim(delay + DECODER_DELAY, None);
            }
            source.frames.consume(header.frame_bytes());
            source.pending = None;
            source.reservoir = None;
        }
        Ok(source)
    }

    /// Decodes the frame at the start of `frames`. Returns whether samples
    /// were kept.
    fn decode(&mut self, header: Header) -> Result<bool, Error> {
        let len = header.frame_bytes();
        let frame = self.frames.frame(len);
        let begin = header.main_data_begin(frame);
        let decoded = match self.reservoir {
            Some(read) if begin > read => {
                // Decoded all the same, so that the reservoir holds its data.
                self.reservoir = Some(read.saturating_add(header.main_data_bytes()));
                self.decoder.skip(frame).map(|()| false)
            }
            _ => {
                self.reservoir = None;
                self.decoder.decode(frame)
            }
        };
        self.frames.consume(len);
        match decoded {
            // libmad keeps the data of a frame it rejects in its reservoir.
            Err(Error::InvalidInput(_)) => Ok(false),
            decoded => decoded,
        }
    }
}

impl<R: Read> fmt::Debug for MpegAudioSource<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MpegAudioSource")
            .field("decoder", &self.decoder)
            .finish_non_exhaustive()
    }
}

impl<R: Read> PcmSource for MpegAudioSource<R> {
    fn sample_rate(&self) -> u32 {
        self.decoder.sample_rate()
    }

    fn channels(&self) -> u16 {
        self.decoder.channels()
    }

    fn next_block(&mut self) -> Result<Option<&[i16]>, Error> {
        loop {
            let header = match self.pending.take() {
                Some(header) => header,
                None => {
                    let Some((header, skipped)) = self.frames.next(Some(self.decoder.header()))?
                    else {
                        return Ok(None);
                    };
                    if skipped {
                        self.reservoir = Some(0);
                    }
                    header
                }
            };
            if self.decode(header)? {
                return Ok(Some(self.decoder.output()));
            }
        }
    }
}

/// Finds the frames of an MPEG audio stream in a window of the input.
struct Frames<R> {
    reader: R,
    buf: Box<[u8]>,
    start: usize,
    end: usize,
}

impl<R: Read> Frames<R> {
    /// `prefix` holds the first bytes, already read from `reader`.
    fn new(reader: R, prefix: &[u8]) -> Self {
        let mut buf = vec![0; 2 * (MAX_FRAME_BYTES + HEADER_BYTES)].into_boxed_slice();
        buf[..prefix.len()].copy_from_slice(prefix);
        Frames {
            reader,
            buf,
            start: 0,
            end: prefix.len(),
        }
    }

    /// Finds the next frame of `stream`, or of any stream for `None`, and
    /// returns its header and whether data was skipped before it. The frame
    /// is then at the start of [`frame`](Self::frame). `None` at the end of
    /// the input, including inside a frame.
    ///
    /// The header at the current position is trusted. Past it, a header is
    /// taken only when the header of a frame of the same stream follows it.
    /// libmad checks only for a sync word there; a whole header keeps junk
    /// from passing for a frame.
    fn next(&mut self, stream: Option<Header>) -> Result<Option<(Header, bool)>, Error> {
        let mut skipped = false;
        loop {
            if !self.fill(HEADER_BYTES)? {
                return Ok(None);
            }
            let header = match self.header_at(0) {
                Err(Error::Unsupported(message)) if !skipped && stream.is_none() => {
                    return Err(Error::Unsupported(message));
                }
                header => header,
            };
            if let Ok(header) = header {
                let len = header.frame_bytes();
                let same = stream.is_none_or(|stream| stream.same_stream(header));
                if !skipped && same {
                    return Ok(self.fill(len)?.then_some((header, false)));
                }
                let followed = self.fill(len + HEADER_BYTES)?
                    && self
                        .header_at(len)
                        .is_ok_and(|next| next.same_stream(header));
                if !skipped && followed {
                    return Err(Error::InvalidInput(
                        "MPEG audio stream changes layer, rate or channels".into(),
                    ));
                }
                if same && followed {
                    return Ok(Some((header, skipped)));
                }
            }
            self.consume(1);
            skipped = true;
        }
    }

    /// Parses the header at `offset` in the window, which
    /// [`fill`](Self::fill) made available.
    fn header_at(&self, offset: usize) -> Result<Header, Error> {
        let at = self.start + offset;
        let bytes = self.buf[at..at + HEADER_BYTES].try_into();
        Header::parse(bytes.expect("four bytes"))
    }

    /// The first `len` bytes of the window, which [`fill`](Self::fill) made
    /// available.
    fn frame(&self, len: usize) -> &[u8] {
        &self.buf[self.start..self.start + len]
    }

    fn consume(&mut self, len: usize) {
        self.start += len;
    }

    /// Reads until the window holds `len` bytes. Returns false when the input
    /// ends first.
    fn fill(&mut self, len: usize) -> Result<bool, Error> {
        if self.end - self.start >= len {
            return Ok(true);
        }
        if self.start + len > self.buf.len() {
            self.buf.copy_within(self.start..self.end, 0);
            self.end -= self.start;
            self.start = 0;
        }
        let read = read_full(&mut self.reader, &mut self.buf[self.end..])?;
        self.end += read;
        Ok(self.end - self.start >= len)
    }
}
