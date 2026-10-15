//! MPEG audio streams, the framing of `.mp3` and `.mp2` files, through
//! Symphonia.

use std::fmt;
use std::io::{BufReader, Read};

use super::mpa::{DECODER_DELAY, HEADER_BYTES, Header, MpaDecoder};
use super::{read_full, skip_id3v2};
use crate::{Error, PcmSource};

/// Streams PCM out of an MPEG audio file (`.mp3`, `.mp2`, `.mp1`), skipping a
/// leading ID3v2 tag.
///
/// Reads with plain `Read`, so an upload body can be decoded as it arrives.
/// Reads are buffered internally. Memory is one frame and one decoded frame,
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
/// A cut file is not an error: as audiowaveform does, the complete frames
/// are decoded and an incomplete last frame is dropped. Bytes between frames
/// that are not a frame are [`Error::InvalidInput`]. A trailing ID3v1 or APE
/// tag ends the stream.
pub struct MpegAudioSource<R: Read> {
    reader: BufReader<R>,
    decoder: MpaDecoder,
    frame: Vec<u8>,
    /// Whether `frame` holds a frame read but not decoded yet.
    pending: bool,
}

impl<R: Read> MpegAudioSource<R> {
    /// Reads up to the first audio frame.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidInput`] when the input is not an MPEG audio stream,
    ///   or holds no complete frame.
    /// - [`Error::Unsupported`] for a free-format bit rate.
    pub fn new(reader: R) -> Result<Self, Error> {
        let mut reader = BufReader::new(reader);
        let (start, read) = skip_id3v2(&mut reader)?;
        let mut frame = Vec::new();
        let header = read_frame(&mut reader, &mut frame, &start[..read])?
            .ok_or_else(|| Error::InvalidInput("no complete MPEG audio frame".into()))?;
        let mut decoder = MpaDecoder::new(header)?;
        let info_tag = header.info_tag(&frame);
        if let Some(Some(delay)) = info_tag {
            decoder.trim(delay + DECODER_DELAY, None);
        }
        Ok(MpegAudioSource {
            reader,
            decoder,
            frame,
            pending: info_tag.is_none(),
        })
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
            if !self.pending && read_frame(&mut self.reader, &mut self.frame, &[])?.is_none() {
                return Ok(None);
            }
            self.pending = false;
            if self.decoder.decode(&self.frame)? {
                return Ok(Some(self.decoder.output()));
            }
        }
    }
}

/// Reads the next frame into `frame`, header included, whose first bytes may
/// already have been read into `start`. Returns `None` at the end of the input,
/// including inside a frame, or at a trailing tag.
fn read_frame<R: Read>(
    reader: &mut R,
    frame: &mut Vec<u8>,
    start: &[u8],
) -> Result<Option<Header>, Error> {
    let mut bytes = [0u8; HEADER_BYTES];
    bytes[..start.len()].copy_from_slice(start);
    match start.len() + read_full(reader, &mut bytes[start.len()..])? {
        HEADER_BYTES => {}
        _ => return Ok(None),
    }
    if bytes.starts_with(b"TAG") || bytes == *b"APET" {
        return Ok(None);
    }
    let header = Header::parse(bytes)?;
    let len = header.frame_bytes();
    frame.clear();
    frame.extend(bytes);
    frame.resize(len, 0);
    if read_full(reader, &mut frame[HEADER_BYTES..])? < len - HEADER_BYTES {
        return Ok(None);
    }
    Ok(Some(header))
}
