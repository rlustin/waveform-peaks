//! MPEG-1, 2 and 2.5 audio, layers I to III (MP1, MP2, MP3), shared by the
//! MP4 and MPEG audio stream readers, through Symphonia.

use std::cmp::Ordering;
use std::fmt;

use symphonia_bundle_mp3::MpaDecoder as SymphoniaDecoder;
use symphonia_core::codecs::audio::well_known::{CODEC_ID_MP1, CODEC_ID_MP2, CODEC_ID_MP3};
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::packet::PacketRef;
use symphonia_core::units::{Duration, Timestamp};

use super::symphonia::{Output, symphonia_error};
use crate::Error;

/// Length of a frame header, without its CRC.
pub(crate) const HEADER_BYTES: usize = 4;

/// Bit rates in kbit/s by bit rate index, for MPEG-1 layers I, II and III,
/// then MPEG-2 and 2.5 layer I, and layers II and III.
const BIT_RATES: [[u16; 15]; 5] = [
    [
        0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448,
    ],
    [
        0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384,
    ],
    [
        0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
    ],
    [
        0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256,
    ],
    [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160],
];

/// An MPEG audio frame header.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Header(u32);

impl Header {
    /// Parses the first four bytes of a frame.
    pub(crate) fn parse(bytes: [u8; HEADER_BYTES]) -> Result<Self, Error> {
        let header = Header(u32::from_be_bytes(bytes));
        if header.0 >> 21 != 0x7FF {
            return Err(Error::InvalidInput("missing MPEG audio sync word".into()));
        }
        if header.0 >> 19 & 0b11 == 0b01 {
            return Err(Error::InvalidInput("reserved MPEG audio version".into()));
        }
        if header.layer() == 0 {
            return Err(Error::InvalidInput("reserved MPEG audio layer".into()));
        }
        match header.bit_rate_index() {
            0 => {
                return Err(Error::Unsupported("free-format MPEG audio bit rate".into()));
            }
            15 => return Err(Error::InvalidInput("invalid MPEG audio bit rate".into())),
            _ => {}
        }
        if header.0 >> 10 & 0b11 == 0b11 {
            return Err(Error::InvalidInput(
                "reserved MPEG audio sample rate".into(),
            ));
        }
        Ok(header)
    }

    fn is_mpeg1(self) -> bool {
        self.0 >> 19 & 0b11 == 0b11
    }

    /// 1, 2 or 3.
    fn layer(self) -> u32 {
        (4 - (self.0 >> 17 & 0b11)) % 4
    }

    fn bit_rate_index(self) -> usize {
        (self.0 >> 12 & 0xF) as usize
    }

    pub(crate) fn sample_rate(self) -> u32 {
        let base = [44_100, 48_000, 32_000][(self.0 >> 10 & 0b11) as usize];
        match self.0 >> 19 & 0b11 {
            0b11 => base,
            0b10 => base / 2,
            _ => base / 4,
        }
    }

    fn is_mono(self) -> bool {
        self.0 >> 6 & 0b11 == 0b11
    }

    pub(crate) fn channels(self) -> u16 {
        if self.is_mono() { 1 } else { 2 }
    }

    fn has_crc(self) -> bool {
        self.0 >> 16 & 1 == 0
    }

    /// The whole frame, header included.
    pub(crate) fn frame_bytes(self) -> usize {
        let table = match (self.is_mpeg1(), self.layer()) {
            (true, layer) => layer as usize - 1,
            (false, 1) => 3,
            (false, _) => 4,
        };
        let bit_rate = BIT_RATES[table][self.bit_rate_index()] as u32 * 1000;
        let padding = (self.0 >> 9 & 1) as usize;
        // Layer I counts in four-byte slots, the others in bytes.
        match self.layer() {
            1 => ((12 * bit_rate / self.sample_rate()) as usize + padding) * 4,
            3 if !self.is_mpeg1() => (72 * bit_rate / self.sample_rate()) as usize + padding,
            _ => (144 * bit_rate / self.sample_rate()) as usize + padding,
        }
    }

    /// Checks that a packet of `len` bytes holds exactly this frame.
    pub(crate) fn check_length(self, len: usize) -> Result<(), Error> {
        match len.cmp(&self.frame_bytes()) {
            Ordering::Less => Err(Error::InvalidInput("truncated MPEG audio frame".into())),
            Ordering::Greater => Err(Error::Unsupported(
                "several MPEG audio frames in one packet".into(),
            )),
            Ordering::Equal => Ok(()),
        }
    }

    /// Whether `other` belongs to the same stream: same version, layer, sample
    /// rate, and mono or not. Bit rate, padding and stereo mode may change.
    pub(crate) fn same_stream(self, other: Header) -> bool {
        const VERSION_LAYER_RATE: u32 = 0x001E_0C00;
        self.0 & VERSION_LAYER_RATE == other.0 & VERSION_LAYER_RATE
            && self.is_mono() == other.is_mono()
    }

    /// The encoder delay recorded in a LAME tag, when `frame` holds a Xing or
    /// Info tag rather than audio. `Some(None)` for a tag without a delay
    /// audiowaveform would trim.
    pub(crate) fn info_tag(self, frame: &[u8]) -> Option<Option<u64>> {
        if self.layer() != 3 {
            return None;
        }
        let side_info = match (self.is_mpeg1(), self.is_mono()) {
            (true, true) => 17,
            (true, false) => 32,
            (false, true) => 9,
            (false, false) => 17,
        };
        let at = HEADER_BYTES + if self.has_crc() { 2 } else { 0 } + side_info;
        let tag = frame.get(at..at + 8)?;
        if !matches!(&tag[..4], b"Xing" | b"Info") {
            return None;
        }
        let flags = u32::from_be_bytes(tag[4..8].try_into().expect("four bytes"));
        // Frame count, byte count, table of contents and quality, when
        // flagged, come before the LAME extension.
        let skipped: usize = [(1, 4), (2, 4), (4, 100), (8, 4)]
            .iter()
            .filter(|&&(flag, _)| flags & flag != 0)
            .map(|&(_, len)| len)
            .sum();
        let lame = at + 8 + skipped;
        // Only these encoders' delays are trimmed by audiowaveform.
        let delay = frame
            .get(lame..lame + 24)
            .filter(|lame| matches!(&lame[..4], b"LAME" | b"Lavc"))
            .map(|lame| u32::from_be_bytes([0, lame[21], lame[22], lame[23]]) as u64 >> 12);
        Some(delay)
    }
}

/// Samples of decoder delay, which the LAME tag's encoder delay leaves out.
pub(crate) const DECODER_DELAY: u64 = 529;

/// Decodes MPEG audio frames to 16-bit PCM, one frame at a time.
pub(crate) struct MpaDecoder {
    decoder: SymphoniaDecoder,
    header: Header,
    output: Output,
}

impl MpaDecoder {
    /// A decoder for the stream whose first frame has `header`.
    pub(crate) fn new(header: Header) -> Result<Self, Error> {
        let codec = match header.layer() {
            1 => CODEC_ID_MP1,
            2 => CODEC_ID_MP2,
            _ => CODEC_ID_MP3,
        };
        let mut params = AudioCodecParameters::new();
        params.for_codec(codec);
        let decoder = SymphoniaDecoder::try_new(&params, &AudioDecoderOptions::default())
            .map_err(mpa_error)?;
        Ok(MpaDecoder {
            decoder,
            header,
            output: Output::new(header.channels()),
        })
    }

    pub(crate) fn sample_rate(&self) -> u32 {
        self.header.sample_rate()
    }

    pub(crate) fn channels(&self) -> u16 {
        self.header.channels()
    }

    /// Keeps only frames `start..end` of the decoded stream, counted from the
    /// first decoded frame.
    pub(crate) fn trim(&mut self, start: u64, end: Option<u64>) {
        self.output.trim(start, end);
    }

    /// Whether every frame up to the end set by [`trim`](Self::trim) is
    /// decoded.
    pub(crate) fn past_end(&self) -> bool {
        self.output.past_end()
    }

    /// Decodes one frame, header included. Returns whether any samples are
    /// left after trimming; they are then in [`output`](Self::output).
    pub(crate) fn decode(&mut self, frame: &[u8]) -> Result<bool, Error> {
        let bytes = frame
            .first_chunk()
            .ok_or_else(|| Error::InvalidInput("truncated MPEG audio frame".into()))?;
        let header = Header::parse(*bytes)?;
        if !header.same_stream(self.header) {
            return Err(Error::InvalidInput(
                "MPEG audio stream changes layer, rate or channels".into(),
            ));
        }
        header.check_length(frame.len())?;
        let packet = PacketRef::new(0, Timestamp::ZERO, Duration::ZERO, frame);
        let decoded = self.decoder.decode_ref(&packet).map_err(mpa_error)?;
        // Scaled by 32768 rounding down, as audiowaveform converts libmad's
        // fixed-point output to 16 bits; it clips at -32767, not -32768.
        self.output.push(
            decoded,
            |value| {
                if value <= -1.0 {
                    -32767
                } else {
                    (value * 32768.0).floor() as i16
                }
            },
            "MPEG audio",
        )
    }

    /// Samples kept from the last frame passed to [`decode`](Self::decode).
    pub(crate) fn output(&self) -> &[i16] {
        self.output.samples()
    }
}

impl fmt::Debug for MpaDecoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MpaDecoder")
            .field("layer", &self.header.layer())
            .field("sample_rate", &self.sample_rate())
            .field("channels", &self.channels())
            .field("output", &self.output)
            .finish_non_exhaustive()
    }
}

fn mpa_error(err: symphonia_core::errors::Error) -> Error {
    symphonia_error(err, "MPEG audio")
}
