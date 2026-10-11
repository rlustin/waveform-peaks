#![no_main]

use std::io::Cursor;

use libfuzzer_sys::fuzz_target;
use waveform_peaks::{
    Bits, ChannelMode, DecodeRate, Limits, MemorySink, OggOpusSource, Options, OpusOptions,
    Overflow, generate,
};

// The first byte picks the options, the rest is the file. The ogg crate skips
// page checksums under cfg(fuzzing), so mutations reach the decoder.
fuzz_target!(|data: &[u8]| {
    let Some((&flags, file)) = data.split_first() else {
        return;
    };
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
    if let Ok(mut source) = OggOpusSource::with_options(Cursor::new(file), opus_options) {
        let _ = generate(&mut source, &options, &Limits::default(), MemorySink::new());
    }
});
