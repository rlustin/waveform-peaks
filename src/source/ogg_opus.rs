//! Ogg Opus (RFC 7845) decoding through libopus.

use std::fmt;
use std::io::{self, Read};

use ogg::Packet;
use ogg::reading::{BasePacketReader, OggReadError, PageParser};
use opus::{Channels, Decoder};

use super::read_full;
use crate::{Error, PcmSource};

/// Sample rates libopus can decode to.
const OPUS_RATES: [u32; 5] = [8_000, 12_000, 16_000, 24_000, 48_000];

/// Ogg page header flag marking the last page of a logical stream.
const END_OF_STREAM: u8 = 0x04;

/// Largest frame count one Opus packet can decode to (120 ms at 48 kHz).
const MAX_PACKET_FRAMES: usize = 5_760;

/// Sample rate the decoder runs at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DecodeRate {
    /// The smallest Opus rate (8, 12, 16, 24 or 48 kHz) at or above the input
    /// rate recorded in the `OpusHead` header, capped at 48 kHz, or 48 kHz when
    /// the header records none. This matches audiowaveform and decodes
    /// low-rate speech at a lower cost.
    #[default]
    MatchInput,
    /// Always 48 kHz.
    Full,
}

/// What happens to decoded samples outside `[-1.0, 1.0]`.
///
/// Opus decoding can overshoot full scale on loud or clipped audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Overflow {
    /// Clamp to the 16-bit range, so clipped audio peaks at full scale.
    #[default]
    Saturate,
    /// Wrap around in two's complement, as audiowaveform does (its decoder
    /// converts without clipping). An overshooting sample turns into a value
    /// of the opposite sign and drops out of the peaks, so clipped passages
    /// read slightly below full scale. Use this to match audiowaveform (within
    /// ±1 per value with the bundled libopus, exact with the same libopus build).
    Wrap,
}

/// Decoder settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct OpusOptions {
    /// Sample rate the decoder runs at, and so the output sample rate.
    pub decode_rate: DecodeRate,
    /// Handling of decoded samples beyond full scale.
    pub overflow: Overflow,
    /// Largest Ogg packet buffered before decoding, checked page by page, so a
    /// packet can exceed it by up to one page (64 KiB). Bounds memory against
    /// packets spanning many pages. The default, 16 MiB, leaves room for an
    /// `OpusTags` header carrying cover art. Audio packets are far smaller.
    pub max_packet_bytes: usize,
}

impl Default for OpusOptions {
    fn default() -> Self {
        OpusOptions {
            decode_rate: DecodeRate::MatchInput,
            overflow: Overflow::Saturate,
            max_packet_bytes: 16 * 1024 * 1024,
        }
    }
}

/// Streams PCM out of an Ogg Opus file.
///
/// Reads with plain `Read`, so an upload body can be decoded as it arrives.
/// Memory is one Ogg page, one packet, and one decoded packet, whatever the
/// input length. Applies the header's pre-skip, output gain, and end trimming.
///
/// Handles channel mapping family 0 (mono and stereo). Only the logical stream
/// of the first page is decoded: pages of other streams are discarded without
/// parsing, so they cannot use memory or fail the decode. Decoding stops at the
/// end of that stream, so chained files yield their first link.
///
/// A stream cut on a page boundary is not an error: the pages present are
/// decoded without end trimming, as audiowaveform does. A cut inside a page is
/// [`Error::InvalidInput`], where audiowaveform decodes what it can.
pub struct OggOpusSource<R: Read> {
    pages: PageReader<R>,
    decoder: Decoder,
    overflow: Overflow,
    sample_rate: u32,
    channels: u16,
    pre_skip: u64,
    decoded: u64,
    float_pcm: Vec<f32>,
    pcm: Vec<i16>,
    finished: bool,
}

impl<R: Read> OggOpusSource<R> {
    /// Reads the `OpusHead` and `OpusTags` headers with default options.
    ///
    /// # Errors
    ///
    /// As for [`with_options`](Self::with_options).
    pub fn new(reader: R) -> Result<Self, Error> {
        Self::with_options(reader, OpusOptions::default())
    }

    /// Reads the `OpusHead` and `OpusTags` headers.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidInput`] when the input is not an Ogg Opus stream.
    /// - [`Error::Unsupported`] for a channel mapping family other than 0 or a
    ///   major version other than 0.
    pub fn with_options(reader: R, options: OpusOptions) -> Result<Self, Error> {
        let mut pages = PageReader {
            reader,
            packets: BasePacketReader::new(),
            serial: None,
            end_granule: None,
            max_packet_bytes: options.max_packet_bytes,
            pending_bytes: 0,
        };

        let head = pages.next_packet()?.ok_or_else(|| invalid("empty input"))?;
        if !head.first_in_stream() {
            return Err(invalid("first packet is not the start of a stream"));
        }
        let head = OpusHead::parse(&head.data)?;

        let tags = pages
            .next_packet()?
            .ok_or_else(|| invalid("missing OpusTags header"))?;
        if !tags.data.starts_with(b"OpusTags") {
            return Err(invalid("second packet is not OpusTags"));
        }

        let sample_rate = match options.decode_rate {
            DecodeRate::Full => 48_000,
            DecodeRate::MatchInput => OPUS_RATES
                .into_iter()
                .find(|&rate| head.input_sample_rate != 0 && rate >= head.input_sample_rate)
                .unwrap_or(48_000),
        };
        let decoder_channels = if head.channels == 1 {
            Channels::Mono
        } else {
            Channels::Stereo
        };
        let mut decoder = Decoder::new(sample_rate, decoder_channels).map_err(opus_error)?;
        if head.output_gain != 0 {
            decoder
                .set_gain(head.output_gain as i32)
                .map_err(opus_error)?;
        }

        Ok(OggOpusSource {
            pages,
            decoder,
            overflow: options.overflow,
            sample_rate,
            channels: head.channels as u16,
            pre_skip: to_decode_rate(head.pre_skip as u64, sample_rate),
            decoded: 0,
            float_pcm: vec![0.0; MAX_PACKET_FRAMES * head.channels as usize],
            pcm: vec![0; MAX_PACKET_FRAMES * head.channels as usize],
            finished: false,
        })
    }
}

impl<R: Read> fmt::Debug for OggOpusSource<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OggOpusSource")
            .field("sample_rate", &self.sample_rate)
            .field("channels", &self.channels)
            .field("overflow", &self.overflow)
            .field("pre_skip", &self.pre_skip)
            .field("decoded", &self.decoded)
            .field("finished", &self.finished)
            .finish_non_exhaustive()
    }
}

impl<R: Read> PcmSource for OggOpusSource<R> {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn next_block(&mut self) -> Result<Option<&[i16]>, Error> {
        loop {
            if self.finished {
                return Ok(None);
            }
            let Some(packet) = self.pages.next_packet()? else {
                // Truncated stream without an end-of-stream page: no end trimming.
                self.finished = true;
                return Ok(None);
            };
            self.finished = packet.last_in_stream();
            if packet.data.is_empty() {
                continue;
            }

            let frames = self
                .decoder
                .decode_float(&packet.data, &mut self.float_pcm, false)
                .map_err(opus_error)? as u64;
            // Scale by 32767 with round-half-to-even (C lrintf), as audiowaveform
            // does. libopus's own int16 output scales by 32768 instead and
            // differs by 1 on some samples.
            let samples = frames as usize * self.channels as usize;
            for (out, &value) in self.pcm.iter_mut().zip(&self.float_pcm[..samples]) {
                let scaled = (value * 32767.0).round_ties_even();
                *out = match self.overflow {
                    // Float-to-int casts saturate.
                    Overflow::Saturate => scaled as i16,
                    Overflow::Wrap => scaled as i32 as i16,
                };
            }
            let before = self.decoded;
            self.decoded += frames;

            // Pre-skip drops decoder warm-up at the start of the stream.
            let start = self.pre_skip.saturating_sub(before).min(frames);
            // The end-of-stream granule position can end the audio inside any
            // packet of that page, not only the last.
            let mut end = frames;
            if let Some(granule) = self.pages.end_granule {
                let total = to_decode_rate(granule, self.sample_rate);
                end = total.saturating_sub(before).clamp(start, frames);
            }

            if start < end {
                let channels = self.channels as usize;
                return Ok(Some(
                    &self.pcm[start as usize * channels..end as usize * channels],
                ));
            }
        }
    }
}

/// Fields of the `OpusHead` identification header used for decoding.
struct OpusHead {
    channels: u8,
    pre_skip: u16,
    input_sample_rate: u32,
    output_gain: i16,
}

impl OpusHead {
    fn parse(data: &[u8]) -> Result<Self, Error> {
        if data.len() < 19 || !data.starts_with(b"OpusHead") {
            return Err(invalid("first packet is not OpusHead"));
        }
        let version = data[8];
        if version >> 4 != 0 {
            return Err(Error::Unsupported(format!("Ogg Opus version {version}")));
        }
        let channels = data[9];
        let family = data[18];
        if family != 0 {
            return Err(Error::Unsupported(format!(
                "channel mapping family {family}"
            )));
        }
        if channels == 0 || channels > 2 {
            return Err(invalid(&format!(
                "channel mapping family 0 with {channels} channels"
            )));
        }
        Ok(OpusHead {
            channels,
            pre_skip: u16::from_le_bytes([data[10], data[11]]),
            input_sample_rate: u32::from_le_bytes([data[12], data[13], data[14], data[15]]),
            output_gain: i16::from_le_bytes([data[16], data[17]]),
        })
    }
}

/// Reads Ogg pages from a plain `Read` and splits them into packets of one
/// logical stream.
struct PageReader<R: Read> {
    reader: R,
    packets: BasePacketReader,
    serial: Option<u32>,
    /// Granule position of the end-of-stream page, once it is read. Packets
    /// are drained before each page is read, so every packet returned after
    /// it ends on that page.
    end_granule: Option<u64>,
    max_packet_bytes: usize,
    pending_bytes: usize,
}

impl<R: Read> PageReader<R> {
    fn next_packet(&mut self) -> Result<Option<Packet>, Error> {
        loop {
            if let Some(packet) = self.packets.read_packet() {
                self.pending_bytes = 0;
                return Ok(Some(packet));
            }
            if !self.read_page()? {
                return Ok(None);
            }
        }
    }

    /// Reads one page into the packet reader. Returns false at the end of input.
    ///
    /// Unlike `ogg::PacketReader`, this needs no `Seek`: the page must start
    /// exactly where the previous one ended, which holds for any well-formed
    /// file. Garbage between pages is rejected instead of skipped.
    fn read_page(&mut self) -> Result<bool, Error> {
        let mut header = [0u8; 27];
        match read_full(&mut self.reader, &mut header)? {
            0 => return Ok(false),
            27 => {}
            _ => return Err(invalid("truncated Ogg page header")),
        }
        if &header[..4] != b"OggS" {
            return Err(invalid("missing Ogg capture pattern"));
        }
        let serial = u32::from_le_bytes([header[14], header[15], header[16], header[17]]);
        if serial != *self.serial.get_or_insert(serial) {
            // BasePacketReader keeps partial packets of every stream it sees,
            // outside the max_packet_bytes count, so other streams never reach it.
            return self.skip_page(header[26] as usize).map(|()| true);
        }
        let (mut parser, segment_count) = PageParser::new(header).map_err(ogg_error)?;
        let mut segments = vec![0; segment_count];
        self.read_exact(&mut segments)?;
        let mut body = vec![0; parser.parse_segments(segments)];
        self.read_exact(&mut body)?;

        self.pending_bytes += body.len();
        if self.pending_bytes > self.max_packet_bytes {
            return Err(Error::LimitExceeded(
                "Ogg packet larger than max_packet_bytes",
            ));
        }
        let page = parser.parse_packet_data(body).map_err(ogg_error)?;
        self.packets.push_page(page).map_err(ogg_error)?;
        if header[5] & END_OF_STREAM != 0 {
            self.end_granule = Some(u64::from_le_bytes(header[6..14].try_into().unwrap()));
        }
        Ok(true)
    }

    fn skip_page(&mut self, segment_count: usize) -> Result<(), Error> {
        let mut segments = [0u8; 255];
        self.read_exact(&mut segments[..segment_count])?;
        let body_len: u64 = segments[..segment_count]
            .iter()
            .map(|&len| len as u64)
            .sum();
        let skipped = io::copy(&mut (&mut self.reader).take(body_len), &mut io::sink())?;
        if skipped < body_len {
            return Err(invalid("truncated Ogg page"));
        }
        Ok(())
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), Error> {
        if read_full(&mut self.reader, buf)? < buf.len() {
            return Err(invalid("truncated Ogg page"));
        }
        Ok(())
    }
}

/// Converts a 48 kHz position (pre-skip, granule position) to the decode rate.
fn to_decode_rate(position: u64, sample_rate: u32) -> u64 {
    (position as u128 * sample_rate as u128 / 48_000) as u64
}

fn invalid(message: &str) -> Error {
    Error::InvalidInput(message.to_owned())
}

fn ogg_error(err: OggReadError) -> Error {
    match err {
        OggReadError::ReadError(err) => Error::Io(err),
        other => Error::InvalidInput(format!("Ogg: {other}")),
    }
}

fn opus_error(err: opus::Error) -> Error {
    Error::InvalidInput(format!("Opus: {err}"))
}
