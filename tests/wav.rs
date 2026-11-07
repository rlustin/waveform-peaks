//! WAV files built in memory.

mod common;

use std::io::{self, Cursor, Read};

use common::*;
use waveform_peaks::{
    Error, Limits, MemorySink, Options, Overflow, PcmSource, WavOptions, WavSource, generate,
};

const PCM: u16 = 1;
const FLOAT: u16 = 3;

fn decode(file: Vec<u8>) -> Result<Vec<i16>, Error> {
    samples(WavSource::new(Cursor::new(file))?)
}

fn decode_wrapping(file: Vec<u8>) -> Result<Vec<i16>, Error> {
    let mut options = WavOptions::default();
    options.overflow = Overflow::Wrap;
    samples(WavSource::with_options(Cursor::new(file), options)?)
}

fn f32_bytes(values: &[f32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

fn f64_bytes(values: &[f64]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

/// Fails if read past its first `len` bytes.
struct Fence<'a> {
    bytes: &'a [u8],
    len: usize,
}

impl Read for Fence<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut rest = &self.bytes[..self.len];
        if rest.is_empty() && !self.bytes.is_empty() {
            return Err(io::Error::other("read past the data chunk"));
        }
        let read = rest.read(buf)?;
        self.bytes = &self.bytes[read..];
        self.len -= read;
        Ok(read)
    }
}

#[test]
fn reads_the_header() {
    let source = WavSource::new(Cursor::new(wav(PCM, 3, 22_050, 16, &[]))).unwrap();
    assert_eq!(source.sample_rate(), 22_050);
    assert_eq!(source.channels(), 3);
}

#[test]
fn decodes_16_bit_samples() {
    let samples = pcm(5_000, 2, 1);
    assert_same(
        &decode(wav(PCM, 2, 44_100, 16, &le_bytes(&samples))).unwrap(),
        &samples,
    );
}

#[test]
fn decodes_8_bit_samples_as_unsigned() {
    let file = wav(PCM, 1, 8_000, 8, &[0, 1, 127, 128, 129, 255]);
    assert_eq!(decode(file).unwrap(), [-32768, -32512, -256, 0, 256, 32512]);
}

#[test]
fn keeps_the_16_most_significant_bits_of_wider_samples() {
    let i24: Vec<u8> = [
        0x7F_FFFF_i32,
        0x00_0001,
        -1,
        -0x80_0000,
        0x12_3456,
        -0x12_3456,
    ]
    .iter()
    .flat_map(|value| value.to_le_bytes()[..3].to_vec())
    .collect();
    assert_eq!(
        decode(wav(PCM, 1, 48_000, 24, &i24)).unwrap(),
        [32767, 0, -1, -32768, 0x1234, -0x1235]
    );
    let i32: Vec<u8> = [i32::MAX, 0xFFFF, -1, i32::MIN, 0x1234_5678, -0x1234_5678]
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect();
    assert_eq!(
        decode(wav(PCM, 1, 48_000, 32, &i32)).unwrap(),
        [32767, 0, -1, -32768, 0x1234, -0x1235]
    );
}

#[test]
fn takes_the_sample_width_from_the_bits_per_sample() {
    // 20-bit samples are stored in 3 bytes, 12-bit samples in 2.
    let i20: Vec<u8> = [0x7F_FFF0_i32, -0x80_0000]
        .iter()
        .flat_map(|value| value.to_le_bytes()[..3].to_vec())
        .collect();
    assert_eq!(
        decode(wav(PCM, 1, 48_000, 20, &i20)).unwrap(),
        [32767, -32768]
    );
    assert_eq!(
        decode(wav(PCM, 1, 48_000, 12, &le_bytes(&[0x7FF0, -0x8000]))).unwrap(),
        [0x7FF0, -0x8000]
    );
}

#[test]
fn scales_float_samples_by_32767_and_truncates() {
    let values = [0.0, 1.0, -1.0, 0.5, -0.5, 0.99999, 1.0 / 32767.0 - 1e-9];
    let expected = [0, 32767, -32767, 16383, -16383, 32766, 0];
    assert_eq!(
        decode(wav(FLOAT, 1, 48_000, 32, &f32_bytes(&values))).unwrap(),
        expected
    );
    let values = values.map(f64::from);
    assert_eq!(
        decode(wav(FLOAT, 1, 48_000, 64, &f64_bytes(&values))).unwrap(),
        expected
    );
}

#[test]
fn converts_doubles_to_float_before_scaling() {
    // Rounds to 0.5 as a float, so scales to 16383, not 16382.
    let value = 0.5 - 1e-12;
    assert_eq!(
        decode(wav(FLOAT, 1, 48_000, 64, &f64_bytes(&[value]))).unwrap(),
        [16383]
    );
}

#[test]
fn float_samples_beyond_full_scale_saturate_or_wrap() {
    let values = [1.5, -1.5, 2.0, f32::NAN];
    let file = || wav(FLOAT, 1, 48_000, 32, &f32_bytes(&values));
    assert_eq!(decode(file()).unwrap(), [32767, -32768, 32767, 0]);
    // 49150.5 truncates to 49150, which wraps to -16386.
    assert_eq!(decode_wrapping(file()).unwrap(), [-16386, 16386, -2, 0]);
}

#[test]
fn reads_extensible_formats() {
    let samples = pcm(100, 6, 2);
    let file = wav_file(&[
        riff_chunk(b"fmt ", &wav_fmt_extensible(PCM, 6, 48_000, 16)),
        riff_chunk(b"data", &le_bytes(&samples)),
    ]);
    assert_same(&decode(file).unwrap(), &samples);

    let file = wav_file(&[
        riff_chunk(b"fmt ", &wav_fmt_extensible(FLOAT, 1, 48_000, 32)),
        riff_chunk(b"data", &f32_bytes(&[-1.0, 1.0])),
    ]);
    assert_eq!(decode(file).unwrap(), [-32767, 32767]);
}

#[test]
fn rejects_an_unknown_extensible_guid() {
    let mut fmt = wav_fmt_extensible(PCM, 1, 48_000, 16);
    *fmt.last_mut().unwrap() ^= 1;
    let file = wav_file(&[riff_chunk(b"fmt ", &fmt), riff_chunk(b"data", &[])]);
    assert!(matches!(decode(file), Err(Error::Unsupported(_))));
}

#[test]
fn skips_other_chunks_and_their_pad_bytes() {
    let samples = pcm(1_000, 2, 3);
    let file = wav_file(&[
        riff_chunk(b"JUNK", &[0xAA; 27]),
        riff_chunk(b"fmt ", &wav_fmt(PCM, 2, 44_100, 16)),
        riff_chunk(b"LIST", b"INFOISFT\x05\0\0\0Lavf\0\0"),
        riff_chunk(b"fact", &[0xFF; 4]),
        riff_chunk(b"data", &le_bytes(&samples)),
        riff_chunk(b"LIST", &[0x55; 101]),
    ]);
    assert_same(&decode(file).unwrap(), &samples);
}

#[test]
fn skips_the_end_of_a_long_fmt_chunk() {
    let mut fmt = wav_fmt(PCM, 1, 8_000, 16);
    fmt.extend([0x01; 61]);
    let file = wav_file(&[riff_chunk(b"fmt ", &fmt), riff_chunk(b"data", &[1, 0])]);
    assert_eq!(decode(file).unwrap(), [1]);
}

#[test]
fn does_not_read_past_the_data_chunk() {
    let samples = pcm(70_000, 1, 4);
    let mut file = wav(PCM, 1, 48_000, 16, &le_bytes(&samples));
    let len = file.len();
    file.extend(riff_chunk(b"LIST", &[0x55; 100]));
    let source = WavSource::new(Fence { bytes: &file, len }).unwrap();
    assert_same(&common::samples(source).unwrap(), &samples);
}

#[test]
fn splits_frames_wider_than_one_sample_across_blocks() {
    // 9-byte frames do not divide the block size.
    let samples = pcm(30_000, 3, 5);
    let i24: Vec<u8> = samples
        .iter()
        .flat_map(|&sample| [0x80, sample as u8, (sample >> 8) as u8])
        .collect();
    assert_same(&decode(wav(PCM, 3, 48_000, 24, &i24)).unwrap(), &samples);
}

#[test]
fn reads_a_streamed_file_to_the_end() {
    // FFmpeg writes these sizes to a pipe.
    let samples = pcm(5_000, 2, 6);
    let mut file = wav(PCM, 2, 48_000, 16, &le_bytes(&samples));
    file[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
    let data = file.len() - le_bytes(&samples).len() - 4;
    file[data..data + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_same(&decode(file).unwrap(), &samples);
}

#[test]
fn reads_a_file_left_with_its_first_header_to_the_end() {
    let samples = pcm(5_000, 1, 7);
    let mut file = wav(PCM, 1, 48_000, 16, &[]);
    file[4..8].copy_from_slice(&8u32.to_le_bytes());
    file.extend(le_bytes(&samples));
    assert_same(&decode(file).unwrap(), &samples);
}

#[test]
fn an_empty_data_chunk_has_no_samples() {
    let mut file = wav(PCM, 1, 48_000, 16, &[]);
    file.extend(riff_chunk(b"LIST", &[0x55; 100]));
    assert_eq!(decode(file).unwrap(), []);
    // Audio after an empty data chunk is not read unless the RIFF size is 8.
    for riff_size in [0u32, 36] {
        let mut file = wav(PCM, 1, 48_000, 16, &[]);
        file[4..8].copy_from_slice(&riff_size.to_le_bytes());
        file.extend(le_bytes(&pcm(100, 1, 12)));
        assert_eq!(decode(file).unwrap(), [], "RIFF size {riff_size}");
    }
}

#[test]
fn a_truncated_data_chunk_ends_at_the_last_whole_frame() {
    let samples = pcm(1_000, 2, 8);
    let mut file = wav(PCM, 2, 48_000, 16, &le_bytes(&samples));
    file.truncate(file.len() - 4 * 100 - 3);
    assert_same(&decode(file).unwrap(), &samples[..2 * 899]);
}

#[test]
fn a_data_chunk_that_is_not_whole_frames_drops_the_partial_frame() {
    let file = wav(PCM, 2, 48_000, 16, &[1, 0, 2, 0, 3, 0]);
    assert_eq!(decode(file).unwrap(), [1, 2]);
}

#[test]
fn reads_rf64() {
    let samples = pcm(1_000, 2, 9);
    let data = le_bytes(&samples);
    let ds64 = [
        0u64.to_le_bytes().as_slice(),
        &(data.len() as u64).to_le_bytes(),
        &1_000u64.to_le_bytes(),
        &0u32.to_le_bytes(),
    ]
    .concat();
    let chunks = [
        riff_chunk(b"ds64", &ds64),
        riff_chunk(b"fmt ", &wav_fmt(PCM, 2, 48_000, 16)),
        [b"data".as_slice(), &u32::MAX.to_le_bytes(), &data].concat(),
        riff_chunk(b"LIST", &[0x55; 100]),
    ]
    .concat();
    let file = [
        b"RF64".as_slice(),
        &u32::MAX.to_le_bytes(),
        b"WAVE",
        &chunks,
    ]
    .concat();
    assert_same(&decode(file.clone()).unwrap(), &samples);

    let mut bw64 = file;
    bw64[..4].copy_from_slice(b"BW64");
    assert_same(&decode(bw64).unwrap(), &samples);
}

#[test]
fn rejects_rf64_without_a_data_size() {
    let chunks = [
        riff_chunk(b"fmt ", &wav_fmt(PCM, 1, 48_000, 16)),
        [b"data".as_slice(), &u32::MAX.to_le_bytes(), &[0; 8]].concat(),
    ]
    .concat();
    let file = [
        b"RF64".as_slice(),
        &u32::MAX.to_le_bytes(),
        b"WAVE",
        &chunks,
    ]
    .concat();
    assert!(matches!(decode(file), Err(Error::InvalidInput(_))));
}

#[test]
fn rejects_files_that_are_not_wav() {
    for file in [
        Vec::new(),
        b"RIFF".to_vec(),
        b"RIFF\x04\0\0\0AVI ".to_vec(),
        b"OggS\0\x02\0\0\0\0\0\0\0\0".to_vec(),
    ] {
        assert!(
            matches!(decode(file.clone()), Err(Error::InvalidInput(_))),
            "{file:?}"
        );
    }
    let mut rifx = wav(PCM, 1, 8_000, 16, &[]);
    rifx[..4].copy_from_slice(b"RIFX");
    assert!(matches!(decode(rifx), Err(Error::Unsupported(_))));
}

#[test]
fn rejects_malformed_headers() {
    let fmt = || riff_chunk(b"fmt ", &wav_fmt(PCM, 1, 8_000, 16));
    for file in [
        wav_file(&[fmt()]),
        wav_file(&[riff_chunk(b"data", &[0; 4]), fmt()]),
        wav_file(&[fmt(), fmt(), riff_chunk(b"data", &[0; 4])]),
        wav_file(&[riff_chunk(b"fmt ", &wav_fmt(PCM, 1, 8_000, 16)[..14])]),
        wav(PCM, 0, 8_000, 16, &[]),
        wav(PCM, 1, 0, 16, &[]),
        wav(PCM, 1, 8_000, 0, &[]),
        wav_file(&[riff_chunk(b"fmt ", &wav_fmt(0xFFFE, 1, 8_000, 16))]),
        // Cut inside the fmt chunk, and inside a chunk before the data.
        fmt()[..20].to_vec(),
        wav_file(&[fmt(), riff_chunk(b"LIST", &[0; 100])])[..60].to_vec(),
    ] {
        let result = decode(file.clone());
        assert!(
            matches!(result, Err(Error::InvalidInput(_))),
            "{result:?} for {file:?}"
        );
    }
}

#[test]
fn rejects_unsupported_formats() {
    let mut in_32_bit_blocks = wav_fmt(PCM, 2, 48_000, 24);
    in_32_bit_blocks[12..14].copy_from_slice(&8u16.to_le_bytes());
    for file in [
        wav(6, 1, 8_000, 8, &[0]),
        wav(7, 1, 8_000, 8, &[0]),
        wav(2, 1, 8_000, 4, &[0]),
        wav(PCM, 1, 8_000, 40, &[0; 5]),
        wav(FLOAT, 1, 8_000, 16, &[0; 2]),
        wav(PCM, 1025, 8_000, 16, &[]),
        wav_file(&[
            riff_chunk(b"fmt ", &in_32_bit_blocks),
            riff_chunk(b"data", &[]),
        ]),
    ] {
        let result = decode(file);
        assert!(matches!(result, Err(Error::Unsupported(_))), "{result:?}");
    }
}

#[test]
fn a_huge_chunk_size_before_the_data_fails_at_the_end_of_the_input() {
    let file = wav_file(&[
        riff_chunk(b"fmt ", &wav_fmt(PCM, 1, 8_000, 16)),
        [b"JUNK".as_slice(), &0xFFFF_FFF0u32.to_le_bytes(), &[0; 64]].concat(),
    ]);
    assert!(matches!(decode(file), Err(Error::InvalidInput(_))));
}

#[test]
fn a_huge_fmt_chunk_is_skipped_without_a_buffer() {
    let mut fmt = riff_chunk(b"fmt ", &wav_fmt(PCM, 1, 8_000, 16));
    fmt[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(matches!(
        decode(wav_file(&[fmt])),
        Err(Error::InvalidInput(_))
    ));
}

#[test]
fn many_channels_of_wide_samples_use_small_blocks() {
    let samples = pcm(10, 1024, 10);
    let f64s: Vec<f64> = samples.iter().map(|&s| s as f64 / 32767.0).collect();
    let mut source =
        WavSource::new(Cursor::new(wav(FLOAT, 1024, 48_000, 64, &f64_bytes(&f64s)))).unwrap();
    let block = source.next_block().unwrap().unwrap();
    assert!(block.len() <= 64 * 1024 / 8, "{} samples", block.len());
    assert_eq!(block.len() % 1024, 0);
}

#[test]
fn generates_peaks() {
    let samples = pcm(48_000, 2, 11);
    let file = wav(PCM, 2, 48_000, 16, &le_bytes(&samples));
    let wav_peaks = generate(
        &mut WavSource::new(Cursor::new(file)).unwrap(),
        &Options::default(),
        &Limits::default(),
        MemorySink::new(),
    )
    .unwrap();
    let raw_peaks = generate(
        &mut waveform_peaks::RawPcmSource::new(Cursor::new(le_bytes(&samples)), 48_000, 2).unwrap(),
        &Options::default(),
        &Limits::default(),
        MemorySink::new(),
    )
    .unwrap();
    assert_eq!(wav_peaks, raw_peaks);
}
