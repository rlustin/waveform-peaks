use crate::{Bits, ChannelMode, Error, Options, PeakSink};

/// Header fields shared by every output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Header {
    /// Input frames per second.
    pub sample_rate: u32,
    /// Input frames summarized by each output point.
    pub samples_per_pixel: u32,
    /// Output sample width.
    pub bits: Bits,
    /// Output channel count: 1 for mixdown, the input channel count for split.
    pub channels: u16,
}

/// One `(min, max)` pair, already quantized to the output bit depth.
///
/// With [`Bits::Eight`] both values are in `-128..=127`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Peak {
    /// Lowest sample of the point.
    pub min: i16,
    /// Highest sample of the point.
    pub max: i16,
}

impl Peak {
    const EMPTY: Peak = Peak {
        min: i16::MAX,
        max: i16::MIN,
    };

    fn include(&mut self, value: i16) {
        self.min = self.min.min(value);
        self.max = self.max.max(value);
    }
}

/// Push-based peak generator.
///
/// Feed interleaved 16-bit frames with [`push_interleaved`](Self::push_interleaved)
/// in blocks of any size, then call [`finish`](Self::finish). State is one
/// accumulator per output channel, so memory does not grow with the input.
/// Each completed point goes straight to the sink.
#[derive(Debug)]
pub struct PeakBuilder<S: PeakSink> {
    sink: S,
    header: Header,
    input_channels: usize,
    frames_in_block: u32,
    length: u32,
    accumulators: Vec<Peak>,
    quantized: Vec<Peak>,
}

impl<S: PeakSink> PeakBuilder<S> {
    /// Starts a waveform and writes its header to `sink`.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidOptions`] when `samples_per_pixel` is below 2, or when
    /// `sample_rate` or `input_channels` is zero. Errors from
    /// [`PeakSink::begin`] pass through.
    pub fn new(
        sample_rate: u32,
        input_channels: u16,
        options: &Options,
        mut sink: S,
    ) -> Result<Self, Error> {
        if options.samples_per_pixel < 2 {
            return Err(Error::InvalidOptions(
                "samples_per_pixel must be at least 2",
            ));
        }
        if sample_rate == 0 {
            return Err(Error::InvalidOptions("sample_rate must be positive"));
        }
        if input_channels == 0 {
            return Err(Error::InvalidOptions("input_channels must be positive"));
        }
        let channels = match options.channel_mode {
            ChannelMode::Mixdown => 1,
            ChannelMode::Split => input_channels,
        };
        let header = Header {
            sample_rate,
            samples_per_pixel: options.samples_per_pixel,
            bits: options.bits,
            channels,
        };
        sink.begin(&header)?;
        Ok(PeakBuilder {
            sink,
            header,
            input_channels: input_channels as usize,
            frames_in_block: 0,
            length: 0,
            accumulators: vec![Peak::EMPTY; channels as usize],
            quantized: vec![Peak::EMPTY; channels as usize],
        })
    }

    /// The header written to the sink.
    pub fn header(&self) -> &Header {
        &self.header
    }

    /// Adds interleaved frames.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidInput`] when `samples` does not hold a whole number of
    /// frames. [`Error::LimitExceeded`] when the point count passes `u32::MAX`.
    /// Errors from [`PeakSink::point`] pass through.
    pub fn push_interleaved(&mut self, samples: &[i16]) -> Result<(), Error> {
        if !samples.len().is_multiple_of(self.input_channels) {
            return Err(Error::InvalidInput(format!(
                "{} samples is not a whole number of {}-channel frames",
                samples.len(),
                self.input_channels
            )));
        }
        let mixdown = self.accumulators.len() == 1;
        for frame in samples.chunks_exact(self.input_channels) {
            if mixdown {
                let sum: i32 = frame.iter().map(|&value| value as i32).sum();
                // Rust integer division truncates toward zero, matching audiowaveform.
                self.accumulators[0].include((sum / self.input_channels as i32) as i16);
            } else {
                for (accumulator, &value) in self.accumulators.iter_mut().zip(frame) {
                    accumulator.include(value);
                }
            }
            self.frames_in_block += 1;
            if self.frames_in_block == self.header.samples_per_pixel {
                self.emit()?;
            }
        }
        Ok(())
    }

    /// Flushes a trailing partial block and finishes the sink.
    ///
    /// # Errors
    ///
    /// [`Error::LimitExceeded`] when the point count passes `u32::MAX`. Errors
    /// from [`PeakSink::point`] and [`PeakSink::finish`] pass through.
    pub fn finish(mut self) -> Result<S::Output, Error> {
        if self.frames_in_block > 0 {
            self.emit()?;
        }
        self.sink.finish(self.length)
    }

    fn emit(&mut self) -> Result<(), Error> {
        let eight_bit = self.header.bits == Bits::Eight;
        for (out, accumulator) in self.quantized.iter_mut().zip(&mut self.accumulators) {
            *out = if eight_bit {
                Peak {
                    min: accumulator.min / 256,
                    max: accumulator.max / 256,
                }
            } else {
                *accumulator
            };
            *accumulator = Peak::EMPTY;
        }
        self.length = self.length.checked_add(1).ok_or(Error::LimitExceeded(
            "waveform length exceeds u32::MAX points",
        ))?;
        self.frames_in_block = 0;
        self.sink.point(&self.quantized)
    }
}
