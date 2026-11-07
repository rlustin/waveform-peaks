use waveform_peaks::{Bits, ChannelMode, Crossover, DecodeRate, Options, OpusOptions, Overflow};

/// Options picked by the first byte of a fuzz input.
pub fn from_flags(flags: u8) -> (OpusOptions, Options) {
    let mut opus_options = OpusOptions::default();
    if flags & 1 != 0 {
        opus_options.decode_rate = DecodeRate::Full;
    }
    if flags & 2 != 0 {
        opus_options.overflow = Overflow::Wrap;
    }
    opus_options.max_packet_bytes = 1 << 20;
    let mut options = Options::default();
    options.samples_per_pixel = 2 + (flags >> 4) as u32 * 64;
    if flags & 4 != 0 {
        options.bits = Bits::Eight;
    }
    if flags & 8 != 0 {
        options.channel_mode = ChannelMode::Split;
    }
    // Shares its bit with the zoom: there are no free bits left.
    if flags & 16 != 0 {
        options.bands = Some(Crossover::default());
    }
    (opus_options, options)
}
