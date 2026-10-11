#![no_main]

mod options;

use std::io::Cursor;

use libfuzzer_sys::fuzz_target;
use waveform_peaks::{Limits, MemorySink, OggOpusSource, generate};

// The first byte picks the options, the rest is the file. The ogg crate skips
// page checksums under cfg(fuzzing), so mutations reach the decoder.
fuzz_target!(|data: &[u8]| {
    let Some((&flags, file)) = data.split_first() else {
        return;
    };
    let (opus_options, options) = options::from_flags(flags);
    if let Ok(mut source) = OggOpusSource::with_options(Cursor::new(file), opus_options) {
        let _ = generate(&mut source, &options, &Limits::default(), MemorySink::new());
    }
});
