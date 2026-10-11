use std::time::Duration;

/// Output sample width.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Bits {
    /// Signed 8-bit values. Each 16-bit peak is divided by 256, truncating
    /// toward zero, as audiowaveform does.
    Eight,
    /// Signed 16-bit values.
    #[default]
    Sixteen,
}

/// How input channels map to output channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChannelMode {
    /// Average all input channels into one output channel. The average uses
    /// integer division truncating toward zero, as audiowaveform does.
    #[default]
    Mixdown,
    /// One output channel per input channel (audiowaveform `--split-channels`).
    Split,
}

/// What to generate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Options {
    /// Input frames summarized by each output point (audiowaveform `--zoom`).
    /// Must be at least 2.
    pub samples_per_pixel: u32,
    /// Output sample width.
    pub bits: Bits,
    /// Channel mapping.
    pub channel_mode: ChannelMode,
}

impl Default for Options {
    /// The audiowaveform defaults: zoom 256, 16 bits, mixdown.
    fn default() -> Self {
        Options {
            samples_per_pixel: 256,
            bits: Bits::Sixteen,
            channel_mode: ChannelMode::Mixdown,
        }
    }
}

/// Bounds for untrusted input, enforced by [`generate`](crate::generate).
///
/// Memory use does not depend on the input length, so these bound CPU time and
/// output size rather than memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Limits {
    /// Highest accepted input channel count.
    pub max_channels: u16,
    /// Longest accepted input. Decoding stops with
    /// [`Error::LimitExceeded`](crate::Error::LimitExceeded) once the decoded
    /// audio passes it.
    pub max_duration: Duration,
}

impl Default for Limits {
    /// 8 channels and 4 hours.
    fn default() -> Self {
        Limits {
            max_channels: 8,
            max_duration: Duration::from_secs(4 * 60 * 60),
        }
    }
}
