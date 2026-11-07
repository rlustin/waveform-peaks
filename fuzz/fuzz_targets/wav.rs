#![no_main]

mod options;

use std::io::Cursor;

use libfuzzer_sys::fuzz_target;
use waveform_peaks::{Limits, MemorySink, WavOptions, WavSource, generate};

// The first byte picks the options, the rest is the file.
fuzz_target!(|data: &[u8]| {
    let Some((&flags, file)) = data.split_first() else {
        return;
    };
    let (opus_options, options) = options::from_flags(flags);
    let mut wav_options = WavOptions::default();
    wav_options.overflow = opus_options.overflow;
    if let Ok(mut source) = WavSource::with_options(Cursor::new(file), wav_options) {
        let _ = generate(&mut source, &options, &Limits::default(), MemorySink::new());
    }
});
