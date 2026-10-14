//! AAC decoding shared by the MP4 and ADTS readers, through Symphonia.

use std::fmt;
use std::ops::Range;

use symphonia_codec_aac::AacDecoder as SymphoniaDecoder;
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::audio::well_known::CODEC_ID_AAC;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::packet::PacketRef;
use symphonia_core::units::{Duration, Timestamp};

use crate::Error;

/// Largest AAC frame accepted. An AAC-LC frame holds at most 6144 bits per
/// channel, so a valid stereo frame is under 2 KiB.
pub(crate) const MAX_FRAME_BYTES: usize = 64 * 1024;

/// Decodes AAC frames to 16-bit PCM, dropping the encoder priming at the start
/// and the padding at the end.
pub(crate) struct AacDecoder {
    decoder: SymphoniaDecoder,
    sample_rate: u32,
    channels: u16,
    start: u64,
    end: Option<u64>,
    decoded: u64,
    float_pcm: Vec<f32>,
    pcm: Vec<i16>,
    kept: Range<usize>,
}

impl AacDecoder {
    /// `config` is the `AudioSpecificConfig` from the container.
    pub(crate) fn new(config: &[u8]) -> Result<Self, Error> {
        let config = core_config(config).unwrap_or_else(|| config.into());
        let mut params = AudioCodecParameters::new();
        params.for_codec(CODEC_ID_AAC).with_extra_data(config);
        let decoder = SymphoniaDecoder::try_new(&params, &AudioDecoderOptions::default())
            .map_err(symphonia_error)?;
        let params = decoder.codec_params();
        let sample_rate = params
            .sample_rate
            .ok_or_else(|| Error::InvalidInput("AAC config without a sample rate".into()))?;
        let channels = params
            .channels
            .as_ref()
            .map_or(0, |channels| channels.count());
        Ok(AacDecoder {
            decoder,
            sample_rate,
            channels: channels as u16,
            start: 0,
            end: None,
            decoded: 0,
            float_pcm: Vec::new(),
            pcm: Vec::new(),
            kept: 0..0,
        })
    }

    pub(crate) fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub(crate) fn channels(&self) -> u16 {
        self.channels
    }

    /// Keeps only frames `start..end` of the decoded stream, counted at the
    /// output rate from the first decoded frame.
    pub(crate) fn trim(&mut self, start: u64, end: Option<u64>) {
        self.start = start;
        self.end = end;
    }

    /// Whether every frame up to the end set by [`trim`](Self::trim) is
    /// decoded.
    pub(crate) fn past_end(&self) -> bool {
        self.end.is_some_and(|end| self.decoded >= end)
    }

    /// Decodes one AAC frame. Returns whether any samples are left after
    /// trimming; they are then in [`output`](Self::output).
    pub(crate) fn decode(&mut self, frame: &[u8]) -> Result<bool, Error> {
        let packet = PacketRef::new(0, Timestamp::ZERO, Duration::ZERO, frame);
        let decoded = self.decoder.decode_ref(&packet).map_err(symphonia_error)?;
        let GenericAudioBufferRef::F32(buffer) = decoded else {
            return Err(Error::InvalidInput(
                "AAC decoder returned non-float samples".into(),
            ));
        };
        buffer.copy_to_vec_interleaved(&mut self.float_pcm);
        // Scale by 32768 with round-half-to-even and saturation, as FFmpeg
        // does when converting its AAC decoder's output to 16 bits.
        self.pcm.clear();
        self.pcm.extend(
            self.float_pcm
                .iter()
                .map(|&value| (value * 32768.0).round_ties_even() as i16),
        );
        let frames = buffer.frames() as u64;
        let before = self.decoded;
        self.decoded += frames;

        let start = self.start.saturating_sub(before).min(frames);
        let end = self
            .end
            .map_or(frames, |end| end.saturating_sub(before))
            .clamp(start, frames);
        let channels = self.channels as usize;
        self.kept = start as usize * channels..end as usize * channels;
        Ok(start < end)
    }

    /// Samples kept from the last frame passed to [`decode`](Self::decode).
    pub(crate) fn output(&self) -> &[i16] {
        &self.pcm[self.kept.clone()]
    }
}

impl fmt::Debug for AacDecoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AacDecoder")
            .field("sample_rate", &self.sample_rate)
            .field("channels", &self.channels)
            .field("start", &self.start)
            .field("end", &self.end)
            .field("decoded", &self.decoded)
            .finish_non_exhaustive()
    }
}

/// Rewrites an `AudioSpecificConfig` to describe only its AAC-LC core,
/// dropping the HE-AAC signalling Symphonia rejects, so that HE-AAC decodes
/// like files that signal it only inside the frames: without its SBR and
/// parametric stereo layers. `None` when the config cannot be parsed or the
/// core is not AAC-LC, leaving the decoder to report it.
fn core_config(config: &[u8]) -> Option<Box<[u8]>> {
    let mut bits = BitReader {
        data: config,
        at: 0,
    };
    let mut object_type = bits.object_type()?;
    let frequency = bits.frequency()?;
    let channels = bits.read(4)?;
    // HE-AAC (5) and HE-AAC v2 (29) give the output rate here, then the core's
    // object type; the core runs at the rate read above.
    if object_type == 5 || object_type == 29 {
        bits.frequency()?;
        object_type = bits.object_type()?;
    }
    if object_type != 2 {
        return None;
    }
    let frame_length_flag = bits.read(1)?;
    let mut out = BitWriter::default();
    out.write(2, 5);
    match frequency {
        Frequency::Index(index) => out.write(index, 4),
        Frequency::Explicit(rate) => {
            out.write(15, 4);
            out.write(rate, 24);
        }
    }
    out.write(channels, 4);
    // GASpecificConfig: the original frame length, so that Symphonia rejects
    // 960-sample frames; no core coder, no extension.
    out.write(frame_length_flag, 1);
    out.write(0, 2);
    Some(out.finish())
}

enum Frequency {
    Index(u32),
    Explicit(u32),
}

struct BitReader<'a> {
    data: &'a [u8],
    at: usize,
}

impl BitReader<'_> {
    fn read(&mut self, count: usize) -> Option<u32> {
        let mut value = 0;
        for _ in 0..count {
            let byte = *self.data.get(self.at / 8)?;
            let bit = byte >> (7 - self.at % 8) & 1;
            value = value << 1 | bit as u32;
            self.at += 1;
        }
        Some(value)
    }

    fn object_type(&mut self) -> Option<u32> {
        match self.read(5)? {
            31 => Some(32 + self.read(6)?),
            object_type => Some(object_type),
        }
    }

    fn frequency(&mut self) -> Option<Frequency> {
        match self.read(4)? {
            15 => Some(Frequency::Explicit(self.read(24)?)),
            index => Some(Frequency::Index(index)),
        }
    }
}

#[derive(Default)]
struct BitWriter {
    bytes: Vec<u8>,
    bits: usize,
}

impl BitWriter {
    fn write(&mut self, value: u32, count: usize) {
        for shift in (0..count).rev() {
            if self.bits.is_multiple_of(8) {
                self.bytes.push(0);
            }
            let bit = (value >> shift & 1) as u8;
            *self.bytes.last_mut().expect("pushed above") |= bit << (7 - self.bits % 8);
            self.bits += 1;
        }
    }

    fn finish(self) -> Box<[u8]> {
        self.bytes.into_boxed_slice()
    }
}

fn symphonia_error(err: symphonia_core::errors::Error) -> Error {
    use symphonia_core::errors::Error as E;
    match err {
        E::IoError(err) => Error::Io(err),
        E::Unsupported(what) => Error::Unsupported(format!("AAC: {what}")),
        other => Error::InvalidInput(format!("AAC: {other}")),
    }
}
