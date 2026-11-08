//! FLAC files built in memory from verbatim frames.

#![cfg(feature = "flac")]

mod common;

use std::io::{self, Cursor, Read};

use common::*;
use waveform_peaks::{Error, FlacSource, Limits, MemorySink, Options, PcmSource, generate};

fn decode(file: Vec<u8>) -> Result<Vec<i16>, Error> {
    samples(FlacSource::new(Cursor::new(file))?)
}

/// `frames` frames of `block_size` samples in each of `channels` channels,
/// with `bits` per sample: noise with full-scale extremes.
fn signal(frames: usize, block_size: usize, channels: usize, bits: u8) -> Vec<Vec<Vec<i32>>> {
    let noise = pcm(frames * block_size, channels, bits as u64);
    let (min, max) = (-1i32 << (bits - 1), (1i32 << (bits - 1)) - 1);
    (0..frames)
        .map(|frame| {
            (0..channels)
                .map(|channel| {
                    (0..block_size)
                        .map(|i| match (frame * block_size + i) % 97 {
                            0 => min,
                            1 => max,
                            _ => {
                                let sample = noise[(frame * block_size + i) * channels + channel];
                                let value = (sample as i32) << 8 | (i as i32 & 0xFF);
                                value >> (24 - bits as i32)
                            }
                        })
                        .collect()
                })
                .collect()
        })
        .collect()
}

/// The audiowaveform conversion: the 16 most significant bits.
fn expected(frames: &[Vec<Vec<i32>>], bits: &[u8]) -> Vec<i16> {
    frames
        .iter()
        .zip(bits.iter().cycle())
        .flat_map(|(channels, &bits)| {
            (0..channels[0].len()).flat_map(move |i| {
                channels.iter().map(move |channel| {
                    let sample = channel[i];
                    if bits > 16 {
                        (sample >> (bits - 16)) as i16
                    } else {
                        (sample << (16 - bits)) as i16
                    }
                })
            })
        })
        .collect()
}

fn file(rate: u32, bits: u8, total: u64, frames: &[Vec<Vec<i32>>]) -> Vec<u8> {
    let channels = frames[0].len() as u8;
    let encoded: Vec<_> = frames
        .iter()
        .enumerate()
        .map(|(number, frame)| flac_frame(number as u32, bits, frame))
        .collect();
    flac_file(&flac_streaminfo(rate, channels, bits, total), &encoded)
}

/// Frame boundaries in `file`, after the metadata of `flac_file`.
fn frame_ends(frames: &[Vec<Vec<i32>>], bits: u8) -> Vec<usize> {
    let mut end = 4 + 4 + 34;
    frames
        .iter()
        .enumerate()
        .map(|(number, frame)| {
            end += flac_frame(number as u32, bits, frame).len();
            end
        })
        .collect()
}

#[test]
fn reads_the_header() {
    let frames = signal(1, 16, 3, 16);
    let source = FlacSource::new(Cursor::new(file(96_000, 16, 0, &frames))).unwrap();
    assert_eq!(source.sample_rate(), 96_000);
    assert_eq!(source.channels(), 3);
}

#[test]
fn keeps_the_16_most_significant_bits() {
    for bits in [8, 16, 24] {
        for channels in [1, 2, 3, 8] {
            let frames = signal(3, 1000, channels, bits);
            let decoded = decode(file(44_100, bits, 0, &frames)).unwrap();
            assert_same(&decoded, &expected(&frames, &[bits]));
        }
    }
}

#[test]
fn converts_each_frame_by_its_own_width() {
    // libsndfile scales each frame by the width in its header.
    let frames = [
        signal(1, 300, 2, 16).remove(0),
        signal(1, 300, 2, 24).remove(0),
        signal(1, 300, 2, 12).remove(0),
        signal(1, 300, 2, 8).remove(0),
    ];
    let encoded: Vec<_> = frames
        .iter()
        .zip([16, 24, 12, 8])
        .enumerate()
        .map(|(number, (frame, bits))| flac_frame(number as u32, bits, frame))
        .collect();
    let file = flac_file(&flac_streaminfo(8_000, 2, 16, 0), &encoded);
    assert_same(&decode(file).unwrap(), &expected(&frames, &[16, 24, 12, 8]));
}

#[test]
fn stops_at_the_streaminfo_total() {
    let frames = signal(3, 1000, 2, 16);
    let all = expected(&frames, &[16]);
    for (total, frames_out) in [
        (0, 3000),
        (1, 1),
        (1999, 1999),
        (2000, 2000),
        (3000, 3000),
        (3001, 3000),
        (1 << 35, 3000),
    ] {
        let decoded = decode(file(8_000, 16, total, &frames)).unwrap();
        assert_same(&decoded, &all[..frames_out * 2]);
    }
}

#[test]
fn skips_id3v2_tags_and_other_metadata_blocks() {
    let frames = signal(2, 500, 1, 16);
    let encoded = [flac_frame(0, 16, &frames[0]), flac_frame(1, 16, &frames[1])];
    let id3 = [b"ID3\x03\x00\x00\x00\x00\x00\x05".as_slice(), &[0; 5]].concat();
    let file = [
        id3.as_slice(),
        &id3,
        b"fLaC",
        &flac_block(1, false, &[0; 100]),
        &flac_block(0, false, &flac_streaminfo(8_000, 1, 16, 0)),
        &flac_block(4, false, &[0xFF; 20]),
        &flac_block(127, false, &[]),
        &flac_block(6, true, &[0xAB; 70_000]),
        &encoded.concat(),
    ]
    .concat();
    assert_same(&decode(file).unwrap(), &expected(&frames, &[16]));
}

#[test]
fn a_later_streaminfo_block_replaces_an_earlier_one() {
    let frames = signal(2, 500, 2, 16);
    let encoded = [flac_frame(0, 16, &frames[0]), flac_frame(1, 16, &frames[1])];
    let mut long = flac_streaminfo(48_000, 2, 16, 600);
    long.extend([0; 4]);
    let file = [
        b"fLaC".as_slice(),
        &flac_block(0, false, &flac_streaminfo(8_000, 2, 16, 0)),
        &flac_block(0, true, &long),
        &encoded.concat(),
    ]
    .concat();
    let source = FlacSource::new(Cursor::new(&file)).unwrap();
    assert_eq!(source.sample_rate(), 48_000);
    assert_same(&samples(source).unwrap(), &expected(&frames, &[16])[..1200]);

    let file = [
        b"fLaC".as_slice(),
        &flac_block(0, false, &flac_streaminfo(8_000, 2, 16, 0)),
        &flac_block(0, true, &flac_streaminfo(8_000, 1, 16, 0)),
    ]
    .concat();
    assert!(matches!(decode(file), Err(Error::InvalidInput(_))));
}

#[test]
fn a_truncated_file_gives_its_complete_frames() {
    let frames = signal(3, 200, 2, 24);
    let full = file(8_000, 24, 0, &frames);
    let ends = frame_ends(&frames, 24);
    let all = expected(&frames, &[24]);
    for cut in ends[0] - 3..full.len() {
        let complete = ends.iter().filter(|&&end| end <= cut).count();
        let decoded = decode(full[..cut].to_vec()).unwrap();
        assert_same(&decoded, &all[..complete * 400]);
    }
}

#[test]
fn replaces_frames_lost_to_damage_with_silence() {
    let frames = signal(5, 200, 2, 16);
    let full = file(8_000, 16, 0, &frames);
    let ends = frame_ends(&frames, 16);
    let metadata = 4 + 4 + 34;
    let all = expected(&frames, &[16]);
    let silence = |frames: usize| vec![0i16; frames * 2];
    let flip = |offsets: &[usize]| {
        let mut file = full.clone();
        for &offset in offsets {
            file[offset] ^= 0x55;
        }
        file
    };
    let insert = |offset: usize, junk: &[u8]| [&full[..offset], junk, &full[offset..]].concat();

    for (name, file, output) in [
        (
            "first frame",
            flip(&[metadata + 50]),
            [silence(200), all[400..].to_vec()].concat(),
        ),
        (
            "second frame",
            flip(&[ends[0] + 50]),
            [&all[..400], &silence(200), &all[800..]].concat(),
        ),
        (
            "two frames",
            flip(&[ends[0] + 50, ends[1] + 50]),
            [&all[..400], &silence(400), &all[1200..]].concat(),
        ),
        (
            "frame header",
            flip(&[ends[1] + 5]),
            [&all[..800], &silence(200), &all[1200..]].concat(),
        ),
        (
            "junk before the first frame",
            insert(metadata, &[0; 100]),
            all.clone(),
        ),
        (
            "junk between frames",
            insert(ends[2], &[0xFF; 100]),
            all.clone(),
        ),
        (
            "ID3v1 tag",
            [&full, b"TAG".as_slice(), &[0x20; 125]].concat(),
            all.clone(),
        ),
        (
            "cut out bytes",
            [&full[..ends[0] + 50], &full[ends[1] + 50..]].concat(),
            [&all[..400], &silence(400), &all[1200..]].concat(),
        ),
        (
            // Claxon would read the next frame as the end of this header.
            "false frame header after damage",
            [
                &flip(&[ends[0] + 50])[..ends[1]],
                &[0xFF, 0xF8, 0x70, 0x10, 0x00],
                &full[ends[1]..],
            ]
            .concat(),
            [&all[..400], &silence(200), &all[800..]].concat(),
        ),
        (
            "other channel count after damage",
            [
                &flip(&[ends[0] + 50])[..ends[1]],
                &flac_frame(2, 16, &frames[2][..1]),
                &full[ends[2]..],
            ]
            .concat(),
            [&all[..400], &silence(400), &all[1200..]].concat(),
        ),
    ] {
        let decoded = decode(file).unwrap_or_else(|err| panic!("{name}: {err}"));
        assert_same(&decoded, &output);
    }

    let mut other_channels = full[..ends[1]].to_vec();
    other_channels.extend(flac_frame(2, 16, &frames[2][..1]));
    other_channels.extend(flac_frame(3, 16, &frames[2]));
    assert_same(&decode(other_channels).unwrap(), &all[..800]);
}

#[test]
fn places_frames_after_damage_by_their_number() {
    // A short last frame: claxon multiplies its number by its own size.
    let mut frames = signal(4, 200, 1, 16);
    frames[3][0].truncate(120);
    let all = expected(&frames, &[16]);
    let mut damaged = file(8_000, 16, 0, &frames);
    damaged[frame_ends(&frames, 16)[1] + 50] ^= 0x55;
    assert_same(
        &decode(damaged).unwrap(),
        &[&all[..400], &[0; 200], &all[600..]].concat(),
    );

    let sizes = [200, 300, 100, 250];
    let frames: Vec<_> = sizes
        .iter()
        .map(|&size| signal(1, size, 1, 16).remove(0))
        .collect();
    let all = expected(&frames, &[16]);
    let mut sample = 0;
    let mut encoded: Vec<_> = frames
        .iter()
        .map(|frame| {
            sample += frame[0].len() as u32;
            flac_variable_frame(sample - frame[0].len() as u32, 16, frame)
        })
        .collect();
    encoded[1][50] ^= 0x55;
    let variable = flac_file(&flac_streaminfo(8_000, 1, 16, 0), &encoded);
    assert_same(
        &decode(variable).unwrap(),
        &[&all[..200], &[0; 300], &all[500..]].concat(),
    );
}

#[test]
fn the_streaminfo_total_cuts_the_silence() {
    let frames = signal(3, 200, 2, 16);
    let all = expected(&frames, &[16]);
    for total in [300, 400] {
        let mut file = file(8_000, 16, total, &frames);
        file[frame_ends(&frames, 16)[0] + 50] ^= 0x55;
        let silence = vec![0; (total as usize - 200) * 2];
        assert_same(&decode(file).unwrap(), &[&all[..400], &silence].concat());
    }
}

#[test]
fn bounds_the_silence_by_the_bytes_lost() {
    let frames = signal(2, 200, 1, 16);
    let all = expected(&frames, &[16]);
    let good = flac_frame(0, 16, &frames[0]);
    let mut lost = flac_frame(1, 16, &frames[1]);
    lost[50] ^= 0x55;
    let far = flac_frame(1 << 30, 16, &frames[1]);
    let mut streaminfo = flac_streaminfo(8_000, 1, 16, 0);
    // The largest block, and the smallest frame.
    streaminfo[2..4].copy_from_slice(&200u16.to_be_bytes());
    streaminfo[4..7].copy_from_slice(&(lost.len() as u32).to_be_bytes()[1..]);
    let file = flac_file(&streaminfo, &[good, lost, far]);
    let mut source = FlacSource::new(Cursor::new(file)).unwrap();
    let mut decoded = Vec::new();
    while let Some(block) = source.next_block().unwrap() {
        decoded.extend_from_slice(block);
        assert!(decoded.len() <= 600, "unbounded silence");
    }
    assert_same(&decoded, &[&all[..200], &[0; 200], &all[200..]].concat());
}

#[test]
fn bounds_the_work_of_frames_that_do_not_decode() {
    let frames = signal(2, 16, 2, 16);
    let all = expected(&frames, &[16]);
    // Stereo, 65535 frames, then a subframe with its padding bit set.
    let mut header = vec![0xFF, 0xF8, 0x70, 0x18, 0x00, 0xFF, 0xFE];
    header.push(flac_crc8(&header));
    let failing = [header, vec![0x80]].concat();
    for (count, decoded) in [(200, all.clone()), (300, all[..32].to_vec())] {
        let file = flac_file(
            &flac_streaminfo(8_000, 2, 16, 0),
            &[
                flac_frame(0, 16, &frames[0]),
                failing.repeat(count),
                flac_frame(1, 16, &frames[1]),
            ],
        );
        assert_same(&decode(file).unwrap(), &decoded);
    }
}

#[test]
fn searches_long_junk_for_the_next_frame() {
    let frames = signal(2, 200, 2, 16);
    let full = file(8_000, 16, 0, &frames);
    let ends = frame_ends(&frames, 16);
    let junk = io::repeat(0xFF).take(4 << 20);
    let reader = Cursor::new(&full[..ends[0]])
        .chain(junk)
        .chain(Cursor::new(&full[ends[0]..]));
    let decoded = FlacSource::new(reader).and_then(samples).unwrap();
    assert_same(&decoded, &expected(&frames, &[16]));
}

#[test]
fn gives_an_error_when_no_frame_decodes() {
    let frames = signal(2, 200, 2, 16);
    let full = file(8_000, 16, 0, &frames);
    let metadata = 4 + 4 + 34;
    let ends = frame_ends(&frames, 16);

    let mut corrupt = full.clone();
    corrupt[metadata + 50] ^= 0x55;
    corrupt[ends[0] + 50] ^= 0x55;
    let junk = [&full[..metadata], &[0x5A; 1000]].concat();
    let other_channels = [
        flac_file(&flac_streaminfo(8_000, 1, 16, 0), &[]),
        full[metadata..].to_vec(),
    ]
    .concat();
    let mut reserved_channels = other_channels.clone();
    reserved_channels[metadata + 3] |= 0xF0;
    for file in [corrupt, junk, other_channels] {
        let result = decode(file);
        assert!(matches!(result, Err(Error::InvalidInput(_))), "{result:?}");
    }
    let result = decode(reserved_channels);
    assert!(
        matches!(&result, Err(Error::InvalidInput(message)) if !message.contains("channels")),
        "{result:?}"
    );

    // As with audiowaveform, a cut is not damage.
    assert_eq!(decode(full[..ends[0] - 1].to_vec()).unwrap(), []);
    assert_eq!(decode(full[..metadata].to_vec()).unwrap(), []);
}

#[test]
fn rejects_malformed_headers() {
    let streaminfo = flac_streaminfo(8_000, 1, 16, 0);
    let frame = flac_frame(0, 16, &signal(1, 16, 1, 16)[0]);
    let mut zero_rate = flac_streaminfo(8_000, 1, 16, 0);
    zero_rate[10..13].fill(0);
    for (name, file) in [
        ("empty", vec![]),
        ("WAV", wav(1, 1, 8_000, 16, &[0; 4])),
        ("marker only", b"fLaC".to_vec()),
        (
            "no STREAMINFO",
            [b"fLaC".as_slice(), &flac_block(1, true, &[0; 34]), &frame].concat(),
        ),
        (
            "short STREAMINFO",
            [
                b"fLaC".as_slice(),
                &flac_block(0, true, &streaminfo[..30]),
                &frame,
            ]
            .concat(),
        ),
        (
            "zero sample rate",
            flac_file(&zero_rate, std::slice::from_ref(&frame)),
        ),
        (
            "no last block",
            [
                b"fLaC".as_slice(),
                &flac_block(0, false, &streaminfo),
                &frame,
            ]
            .concat(),
        ),
        ("cut metadata", flac_file(&streaminfo, &[])[..20].to_vec()),
        (
            "block past the end",
            [
                b"fLaC".as_slice(),
                &flac_block(0, false, &streaminfo),
                &[0x81, 0xFF, 0xFF, 0xFF],
            ]
            .concat(),
        ),
    ] {
        assert!(
            matches!(decode(file), Err(Error::InvalidInput(_))),
            "{name}"
        );
    }
}

#[test]
fn rejects_widths_audiowaveform_rejects() {
    for bits in [4, 12, 20, 32] {
        let file = flac_file(&flac_streaminfo(8_000, 1, bits, 0), &[]);
        assert!(
            matches!(decode(file), Err(Error::Unsupported(_))),
            "{bits} bits"
        );
    }
}

#[test]
fn a_frame_without_a_width_code_is_unsupported() {
    // 17 bits has no frame header code: claxon does not decode it.
    let frame = flac_frame(0, 17, &[vec![0; 16]]);
    let file = [flac_file(&flac_streaminfo(8_000, 1, 16, 0), &[]), frame].concat();
    assert!(matches!(decode(file), Err(Error::Unsupported(_))));
}

#[test]
fn replaces_frames_claxon_does_not_decode_with_silence() {
    let frames = signal(3, 64, 1, 16);
    let all = expected(&frames, &[16]);
    let streaminfo = flac_streaminfo(8_000, 1, 16, 0);
    let escaped = |number| flac_escaped_frame(number, 16, &frames[number as usize][0]);
    let frame = |number| flac_frame(number, 16, &frames[number as usize]);

    let only = flac_file(&streaminfo, &[escaped(0), escaped(1)]);
    assert!(matches!(decode(only), Err(Error::Unsupported(_))));

    let first = flac_file(&streaminfo, &[escaped(0), frame(1), frame(2)]);
    assert_same(&decode(first).unwrap(), &[&[0; 64], &all[64..]].concat());
    let later = flac_file(&streaminfo, &[frame(0), escaped(1), frame(2)]);
    assert_same(
        &decode(later).unwrap(),
        &[&all[..64], &[0; 64], &all[128..]].concat(),
    );

    // At the end, the total in STREAMINFO gives the frames lost.
    let last = flac_file(&streaminfo, &[frame(0), escaped(1)]);
    assert_same(&decode(last).unwrap(), &all[..64]);
    let last = flac_file(&flac_streaminfo(8_000, 1, 16, 128), &[frame(0), escaped(1)]);
    assert_same(&decode(last).unwrap(), &[&all[..64], &[0; 64]].concat());
}

/// Returns an error once, at `at`.
struct FailsOnce<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Read for FailsOnce<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.at == 0 {
            self.at = usize::MAX;
            return Err(io::Error::other("network"));
        }
        let len = buf.len().min(self.bytes.len()).min(self.at);
        buf[..len].copy_from_slice(&self.bytes[..len]);
        self.bytes = &self.bytes[len..];
        self.at -= len;
        Ok(len)
    }
}

#[test]
fn ends_after_an_error() {
    let frames = signal(3, 200, 2, 16);
    let full = file(8_000, 16, 0, &frames);
    let ends = frame_ends(&frames, 16);
    // After the error, the input is inside the second frame.
    let reader = FailsOnce {
        bytes: &full,
        at: ends[0] + 300,
    };
    let mut source = FlacSource::new(reader).unwrap();
    assert!(source.next_block().unwrap().is_some());
    assert!(matches!(source.next_block(), Err(Error::Io(_))));
    assert!(matches!(source.next_block(), Ok(None)));
}

/// Returns `error` once its `bytes` are read, and `Interrupted` before every
/// read.
struct Faulty<'a> {
    bytes: &'a [u8],
    interrupt: bool,
}

impl Read for Faulty<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.interrupt = !self.interrupt;
        if self.interrupt {
            return Err(io::ErrorKind::Interrupted.into());
        }
        if self.bytes.is_empty() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "network"));
        }
        let len = buf.len().min(self.bytes.len()).min(100);
        buf[..len].copy_from_slice(&self.bytes[..len]);
        self.bytes = &self.bytes[len..];
        Ok(len)
    }
}

#[test]
fn passes_reader_errors_through() {
    let frames = signal(3, 200, 2, 16);
    let full = file(8_000, 16, 0, &frames);
    let ends = frame_ends(&frames, 16);
    for cut in [10, ends[0] - 1, ends[0] + 1, ends[0] + 300, full.len()] {
        let reader = Faulty {
            bytes: &full[..cut],
            interrupt: false,
        };
        let result = FlacSource::new(reader).and_then(samples);
        assert!(
            matches!(&result, Err(Error::Io(err)) if err.to_string() == "network"),
            "cut at {cut}: {result:?}"
        );
    }
}

/// Returns its `bytes`, its end once, then an error.
struct EndThenError<'a> {
    bytes: &'a [u8],
    ended: bool,
}

impl Read for EndThenError<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.bytes.is_empty() {
            if self.ended {
                return Err(io::Error::other("network"));
            }
            self.ended = true;
            return Ok(0);
        }
        let len = buf.len().min(self.bytes.len());
        buf[..len].copy_from_slice(&self.bytes[..len]);
        self.bytes = &self.bytes[len..];
        Ok(len)
    }
}

#[test]
fn passes_an_error_after_the_end_through() {
    let frames = signal(2, 200, 2, 16);
    let full = file(8_000, 16, 0, &frames);
    let reader = EndThenError {
        bytes: &full,
        ended: false,
    };
    let result = FlacSource::new(reader).and_then(samples);
    assert!(
        matches!(&result, Err(Error::Io(err)) if err.to_string() == "network"),
        "{result:?}"
    );
}

#[test]
fn reads_long_metadata_blocks_without_buffering_them() {
    let frames = signal(1, 100, 1, 16);
    let padding = (1 << 24) - 1;
    let header = [
        b"fLaC".as_slice(),
        &flac_block(0, false, &flac_streaminfo(8_000, 1, 16, 0)),
        &[0x01, 0xFF, 0xFF, 0xFF],
    ]
    .concat();
    let tail = [flac_block(1, true, &[]), flac_frame(0, 16, &frames[0])].concat();
    let mut reader = Cursor::new(header)
        .chain(io::repeat(0).take(padding))
        .chain(Cursor::new(tail));
    let mut source = FlacSource::new(&mut reader).unwrap();
    let mut decoded = Vec::new();
    while let Some(block) = source.next_block().unwrap() {
        decoded.extend(block);
    }
    assert_same(&decoded, &expected(&frames, &[16]));
}

#[test]
fn decodes_the_largest_frames() {
    let frames = signal(2, 65_535, 8, 24);
    let file = file(192_000, 24, 0, &frames);
    let peaks = generate(
        &mut FlacSource::new(Cursor::new(file)).unwrap(),
        &Options::default(),
        &Limits::default(),
        MemorySink::default(),
    )
    .unwrap();
    assert_eq!(peaks.length(), (2 * 65_535u32).div_ceil(256));
}
