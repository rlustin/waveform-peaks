#![no_main]

mod options;

use std::io::Cursor;

use libfuzzer_sys::fuzz_target;
use waveform_peaks::{Limits, MemorySink, Mp4Options, Mp4Source, generate};

// The first byte picks the options, the rest is the file.
fuzz_target!(|data: &[u8]| {
    let Some((&flags, file)) = data.split_first() else {
        return;
    };
    let (opus_options, options) = options::from_flags(flags);
    let mut mp4_options = Mp4Options::default();
    mp4_options.max_table_bytes = 1 << 20;
    mp4_options.opus = opus_options;
    if let Ok(mut source) = Mp4Source::with_options(Cursor::new(file), mp4_options) {
        let _ = generate(&mut source, &options, &Limits::default(), MemorySink::new());
    }
});
