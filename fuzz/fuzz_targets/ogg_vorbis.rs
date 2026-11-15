#![no_main]

mod options;

use std::{io::Cursor, panic};

use libfuzzer_sys::fuzz_target;
use waveform_peaks::{Limits, MemorySink, OggVorbisSource, VorbisOptions, generate};

// The first byte picks the options, the rest is the file. Page checksums are
// filled in so that mutations reach the decoder, except zero ones, which stay
// wrong.
//
// lewton overflows on some crafted floors, and the source catches the panic.
// libFuzzer's hook would abort first, so panics inside lewton skip it.
fuzz_target!(
    init: {
        let abort = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            if !info.location().is_some_and(|at| at.file().contains("lewton")) {
                abort(info);
            }
        }));
    },
    |data: &[u8]| {
    let Some((&flags, file)) = data.split_first() else {
        return;
    };
    let file = &with_checksums(file);
    let (opus_options, options) = options::from_flags(flags);
    let mut vorbis_options = VorbisOptions::default();
    vorbis_options.overflow = opus_options.overflow;
    vorbis_options.max_packet_bytes = 1 << 20;
    if flags & 1 != 0 {
        vorbis_options.max_held_frames = 0;
    }
    if let Ok(mut source) = OggVorbisSource::with_options(Cursor::new(file), vorbis_options) {
        let _ = generate(&mut source, &options, &Limits::default(), MemorySink::new());
    }
}
);

fn with_checksums(file: &[u8]) -> Vec<u8> {
    let mut file = file.to_vec();
    let mut start = 0;
    while let Some(header) = file.get(start..start + 27) {
        if &header[..4] != b"OggS" {
            start += 1;
            continue;
        }
        let segments = header[26] as usize;
        let Some(lacing) = file.get(start + 27..start + 27 + segments) else {
            break;
        };
        let len = 27 + segments + lacing.iter().map(|&len| len as usize).sum::<usize>();
        let Some(page) = file.get_mut(start..start + len) else {
            break;
        };
        if page[22..26] == [0; 4] {
            start += 1;
            continue;
        }
        page[22..26].fill(0);
        let crc = page.iter().fold(0u32, |mut crc, &byte| {
            crc ^= (byte as u32) << 24;
            for _ in 0..8 {
                crc = if crc & 0x8000_0000 != 0 {
                    (crc << 1) ^ 0x04C1_1DB7
                } else {
                    crc << 1
                };
            }
            crc
        });
        page[22..26].copy_from_slice(&crc.to_le_bytes());
        start += len;
    }
    file
}
