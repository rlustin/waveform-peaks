//! Output handling shared by the decoders built on Symphonia.

use std::fmt;
use std::ops::Range;

use symphonia_core::audio::{Audio, GenericAudioBufferRef};

use crate::Error;

/// Converts a decoder's float output to 16-bit PCM, keeping only frames
/// `start..end` of the decoded stream.
pub(crate) struct Output {
    channels: u16,
    start: u64,
    end: Option<u64>,
    decoded: u64,
    float_pcm: Vec<f32>,
    pcm: Vec<i16>,
    kept: Range<usize>,
}

impl Output {
    pub(crate) fn new(channels: u16) -> Self {
        Output {
            channels,
            start: 0,
            end: None,
            decoded: 0,
            float_pcm: Vec::new(),
            pcm: Vec::new(),
            kept: 0..0,
        }
    }

    /// Keeps only frames `start..end`, counted at the output rate from the
    /// first decoded frame.
    pub(crate) fn trim(&mut self, start: u64, end: Option<u64>) {
        self.start = start;
        self.end = end;
    }

    /// Whether every frame up to the end set by [`trim`](Self::trim) is
    /// decoded.
    pub(crate) fn past_end(&self) -> bool {
        self.end.is_some_and(|end| self.decoded >= end)
    }

    /// Converts one decoded frame with `to_i16`. Returns whether any samples
    /// are left after trimming; they are then in [`samples`](Self::samples).
    pub(crate) fn push(
        &mut self,
        decoded: GenericAudioBufferRef<'_>,
        to_i16: fn(f32) -> i16,
        codec: &str,
    ) -> Result<bool, Error> {
        let GenericAudioBufferRef::F32(buffer) = decoded else {
            return Err(Error::InvalidInput(format!(
                "{codec} decoder returned non-float samples"
            )));
        };
        if buffer.num_planes() != self.channels as usize {
            return Err(Error::InvalidInput(format!(
                "{codec} stream changes its channel count"
            )));
        }
        buffer.copy_to_vec_interleaved(&mut self.float_pcm);
        self.pcm.clear();
        self.pcm
            .extend(self.float_pcm.iter().map(|&value| to_i16(value)));
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

    /// Samples kept from the last frame passed to [`push`](Self::push).
    pub(crate) fn samples(&self) -> &[i16] {
        &self.pcm[self.kept.clone()]
    }
}

impl fmt::Debug for Output {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Output")
            .field("start", &self.start)
            .field("end", &self.end)
            .field("decoded", &self.decoded)
            .finish_non_exhaustive()
    }
}

pub(crate) fn symphonia_error(err: symphonia_core::errors::Error, codec: &str) -> Error {
    use symphonia_core::errors::Error as E;
    match err {
        E::IoError(err) => Error::Io(err),
        E::Unsupported(what) => Error::Unsupported(format!("{codec}: {what}")),
        other => Error::InvalidInput(format!("{codec}: {other}")),
    }
}
