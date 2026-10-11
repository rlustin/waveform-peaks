//! Opus decoding, independent of the container.

use std::fmt;
use std::ops::Range;

use opus::{Channels, Decoder};

use super::invalid;
use crate::Error;

/// Sample rates libopus can decode to.
const OPUS_RATES: [u32; 5] = [8_000, 12_000, 16_000, 24_000, 48_000];

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

/// Fields of the `OpusHead` identification header used for decoding.
pub(crate) struct OpusHead {
    pub(crate) channels: u8,
    pub(crate) pre_skip: u16,
    input_sample_rate: u32,
    output_gain: i16,
}

impl OpusHead {
    pub(crate) fn parse(data: &[u8]) -> Result<Self, Error> {
        if data.len() < 19 || !data.starts_with(b"OpusHead") {
            return Err(invalid("missing OpusHead header"));
        }
        let version = data[8];
        if version >> 4 != 0 {
            return Err(Error::Unsupported(format!("OpusHead version {version}")));
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

/// Where the audio of a packet ends, in frames at the decode rate.
pub(crate) enum End {
    /// Nowhere in this packet.
    Open,
    /// At this position counted from the start of the stream, pre-skip
    /// included (an Ogg granule position).
    At(u64),
}

/// Decodes Opus packets to 16-bit PCM, dropping the pre-skip at the start and
/// the padding at the end.
pub(crate) struct OpusDecoder {
    decoder: Decoder,
    overflow: Overflow,
    sample_rate: u32,
    channels: u16,
    pre_skip: u64,
    decoded: u64,
    float_pcm: Vec<f32>,
    pcm: Vec<i16>,
    kept: Range<usize>,
}

impl OpusDecoder {
    /// `pre_skip` is in 48 kHz frames, as in `OpusHead`.
    pub(crate) fn new(
        head: &OpusHead,
        pre_skip: u64,
        options: &OpusOptions,
    ) -> Result<Self, Error> {
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
        Ok(OpusDecoder {
            decoder,
            overflow: options.overflow,
            sample_rate,
            channels: head.channels as u16,
            pre_skip: to_decode_rate(pre_skip, sample_rate),
            decoded: 0,
            float_pcm: vec![0.0; MAX_PACKET_FRAMES * head.channels as usize],
            pcm: vec![0; MAX_PACKET_FRAMES * head.channels as usize],
            kept: 0..0,
        })
    }

    pub(crate) fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub(crate) fn channels(&self) -> u16 {
        self.channels
    }

    /// Decodes `packet`. Returns whether any samples are left after trimming;
    /// they are then in [`output`](Self::output).
    pub(crate) fn decode(&mut self, packet: &[u8], end: End) -> Result<bool, Error> {
        // libopus treats an empty packet as a lost one and conceals it.
        if packet.is_empty() {
            return Ok(false);
        }
        let frames = self
            .decoder
            .decode_float(packet, &mut self.float_pcm, false)
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

        let start = self.pre_skip.saturating_sub(before).min(frames);
        let end = match end {
            End::Open => frames,
            End::At(position) => position.saturating_sub(before),
        }
        .clamp(start, frames);
        let channels = self.channels as usize;
        self.kept = start as usize * channels..end as usize * channels;
        Ok(start < end)
    }

    /// Samples kept from the last packet passed to [`decode`](Self::decode).
    pub(crate) fn output(&self) -> &[i16] {
        &self.pcm[self.kept.clone()]
    }
}

impl fmt::Debug for OpusDecoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpusDecoder")
            .field("sample_rate", &self.sample_rate)
            .field("channels", &self.channels)
            .field("overflow", &self.overflow)
            .field("pre_skip", &self.pre_skip)
            .field("decoded", &self.decoded)
            .finish_non_exhaustive()
    }
}

/// Converts a 48 kHz position (pre-skip, granule position) to the decode rate.
pub(crate) fn to_decode_rate(position: u64, sample_rate: u32) -> u64 {
    (position as u128 * sample_rate as u128 / 48_000) as u64
}

fn opus_error(err: opus::Error) -> Error {
    Error::InvalidInput(format!("Opus: {err}"))
}
