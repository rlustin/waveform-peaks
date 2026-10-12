//! AAC decoding through Symphonia, independent of the container.

use std::fmt;
use std::ops::Range;

use symphonia_codec_aac::AacDecoder as SymphoniaDecoder;
use symphonia_core::audio::{Audio, GenericAudioBufferRef};
use symphonia_core::codecs::audio::well_known::CODEC_ID_AAC;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::packet::PacketRef;
use symphonia_core::units::{Duration, Timestamp};

use crate::Error;

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
        let mut params = AudioCodecParameters::new();
        params
            .for_codec(CODEC_ID_AAC)
            .with_extra_data(config.into());
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

fn symphonia_error(err: symphonia_core::errors::Error) -> Error {
    use symphonia_core::errors::Error as E;
    match err {
        E::IoError(err) => Error::Io(err),
        E::Unsupported(what) => Error::Unsupported(format!("AAC: {what}")),
        other => Error::InvalidInput(format!("AAC: {other}")),
    }
}
