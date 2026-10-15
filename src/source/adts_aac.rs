//! AAC in ADTS, the framing of raw `.aac` files, through Symphonia.

use std::fmt;
use std::io::{BufReader, Read};

use super::aac::AacDecoder;
use super::{read_full, skip_id3v2};
use crate::{Error, PcmSource};

/// Length of an ADTS header without its CRC.
const HEADER_BYTES: usize = 7;

/// Streams PCM out of an ADTS file (`.aac`), skipping a leading ID3v2 tag.
///
/// Reads with plain `Read`, so an upload body can be decoded as it arrives.
/// Reads are buffered internally. Memory is one frame and one decoded frame,
/// whatever the input length.
///
/// ADTS records no encoder priming or padding, so nothing is trimmed: the
/// output starts with the encoder's warm-up, as FFmpeg's does. Handles AAC-LC
/// in mono and stereo, one frame per ADTS header; other profiles and channel
/// layouts are [`Error::Unsupported`].
///
/// A file cut between frames is not an error: the frames present are
/// decoded. A cut inside a frame is [`Error::InvalidInput`]. A trailing ID3v1
/// or APE tag ends the stream.
pub struct AdtsAacSource<R: Read> {
    reader: BufReader<R>,
    decoder: AacDecoder,
    /// The fields of the first header that every later header must repeat.
    config: [u8; 2],
    frame: Vec<u8>,
    /// Whether `frame` holds a frame read but not decoded yet.
    pending: bool,
}

impl<R: Read> AdtsAacSource<R> {
    /// Reads up to the first frame.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidInput`] when the input is not ADTS.
    /// - [`Error::Unsupported`] for a profile other than AAC-LC, more than two
    ///   channels, or several frames per header.
    pub fn new(reader: R) -> Result<Self, Error> {
        let mut reader = BufReader::new(reader);
        let (start, read) = skip_id3v2(&mut reader)?;
        let mut frame = Vec::new();
        let header = read_frame(&mut reader, &mut frame, &start[..read])?
            .ok_or_else(|| Error::InvalidInput("empty input".into()))?;
        let decoder = AacDecoder::new(&header.audio_specific_config())?;
        Ok(AdtsAacSource {
            reader,
            decoder,
            config: header.config,
            frame,
            pending: true,
        })
    }
}

impl<R: Read> fmt::Debug for AdtsAacSource<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AdtsAacSource")
            .field("decoder", &self.decoder)
            .finish_non_exhaustive()
    }
}

impl<R: Read> PcmSource for AdtsAacSource<R> {
    fn sample_rate(&self) -> u32 {
        self.decoder.sample_rate()
    }

    fn channels(&self) -> u16 {
        self.decoder.channels()
    }

    fn next_block(&mut self) -> Result<Option<&[i16]>, Error> {
        loop {
            if !self.pending {
                let Some(header) = read_frame(&mut self.reader, &mut self.frame, &[])? else {
                    return Ok(None);
                };
                if header.config != self.config {
                    return Err(Error::InvalidInput(
                        "ADTS stream changes profile, rate or channels".into(),
                    ));
                }
            }
            self.pending = false;
            if self.decoder.decode(&self.frame)? {
                return Ok(Some(self.decoder.output()));
            }
        }
    }
}

/// The fields of an ADTS header needed to decode its frame.
struct Header {
    /// Profile, sampling frequency index and channel configuration, as they
    /// sit in header bytes 2 and 3.
    config: [u8; 2],
}

impl Header {
    fn profile(&self) -> u8 {
        self.config[0] >> 6
    }

    fn frequency_index(&self) -> u8 {
        (self.config[0] >> 2) & 0xF
    }

    fn channel_config(&self) -> u8 {
        (self.config[0] & 1) << 2 | self.config[1] >> 6
    }

    /// The two-byte `AudioSpecificConfig` an MP4 file would carry.
    fn audio_specific_config(&self) -> [u8; 2] {
        let object_type = self.profile() as u16 + 1;
        let config = object_type << 11
            | (self.frequency_index() as u16) << 7
            | (self.channel_config() as u16) << 3;
        config.to_be_bytes()
    }
}

/// Reads the next ADTS frame into `frame`, without its header, whose first
/// bytes may already have been read into `start`. Returns `None` at the end of
/// the input when it falls between frames, or at a trailing tag.
fn read_frame<R: Read>(
    reader: &mut R,
    frame: &mut Vec<u8>,
    start: &[u8],
) -> Result<Option<Header>, Error> {
    let mut bytes = [0u8; HEADER_BYTES];
    bytes[..start.len()].copy_from_slice(start);
    match start.len() + read_full(reader, &mut bytes[start.len()..])? {
        0 => return Ok(None),
        HEADER_BYTES => {}
        _ => return Err(truncated()),
    }
    if bytes.starts_with(b"TAG") || bytes.starts_with(b"APETAGE") {
        return Ok(None);
    }
    // 12-bit sync word, then layer 0.
    if bytes[0] != 0xFF || bytes[1] & 0xF6 != 0xF0 {
        return Err(Error::InvalidInput("missing ADTS sync word".into()));
    }
    let header = Header {
        config: [bytes[2] & 0xFD, bytes[3] & 0xC0],
    };
    if header.frequency_index() > 12 {
        return Err(Error::InvalidInput(
            "reserved ADTS sampling frequency".into(),
        ));
    }
    if header.channel_config() == 0 {
        return Err(Error::Unsupported(
            "ADTS channel layout in the stream".into(),
        ));
    }
    if bytes[6] & 0b11 != 0 {
        return Err(Error::Unsupported(
            "several AAC frames per ADTS header".into(),
        ));
    }
    let header_len = if bytes[1] & 1 == 0 { 9 } else { HEADER_BYTES };
    let frame_len =
        ((bytes[3] as usize & 0b11) << 11) | (bytes[4] as usize) << 3 | (bytes[5] as usize) >> 5;
    let payload = frame_len
        .checked_sub(header_len)
        .ok_or_else(|| Error::InvalidInput("ADTS frame shorter than its header".into()))?;
    // The CRC, when present, is skipped rather than checked.
    let mut crc = [0u8; 2];
    if header_len == 9 && read_full(reader, &mut crc)? < 2 {
        return Err(truncated());
    }
    frame.resize(payload, 0);
    if read_full(reader, frame)? < payload {
        return Err(truncated());
    }
    Ok(Some(header))
}

fn truncated() -> Error {
    Error::InvalidInput("truncated ADTS frame".into())
}
