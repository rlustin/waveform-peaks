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
    let streamed =
        Mp4Source::with_options(Cursor::new(file), mp4_options).and_then(|mut source| {
            generate(&mut source, &options, &Limits::default(), MemorySink::new())
        });
    let seeked =
        Mp4Source::seekable_with_options(Cursor::new(file), mp4_options).and_then(|mut source| {
            generate(&mut source, &options, &Limits::default(), MemorySink::new())
        });
    // Seeking past data must not change what reading through it gives.
    if let Ok(streamed) = streamed {
        assert_eq!(seeked.ok(), Some(streamed));
    }
});
