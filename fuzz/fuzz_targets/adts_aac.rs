#![no_main]

mod options;

use std::io::Cursor;

use libfuzzer_sys::fuzz_target;
use waveform_peaks::{AdtsAacSource, Limits, MemorySink, generate};

// The first byte picks the options, the rest is the file.
fuzz_target!(|data: &[u8]| {
    let Some((&flags, file)) = data.split_first() else {
        return;
    };
    let (_, options) = options::from_flags(flags);
    if let Ok(mut source) = AdtsAacSource::new(Cursor::new(file)) {
        let _ = generate(&mut source, &options, &Limits::default(), MemorySink::new());
    }
});
