//! ADTS decoding of small fixtures.

#![cfg(feature = "aac")]

mod common;

use std::io::{self, Cursor, Read};

use common::*;
use waveform_peaks::{AdtsAacSource, Error, PcmSource};

const MONO: &[u8] = include_bytes!("data/sine-mono-44100.aac");
const STEREO: &[u8] = include_bytes!("data/chord-stereo-48000.aac");

fn samples<P: PcmSource>(mut source: P) -> Result<Vec<i16>, Error> {
    let mut samples = Vec::new();
    while let Some(block) = source.next_block()? {
        samples.extend(block);
    }
    Ok(samples)
}

fn adts(bytes: &[u8]) -> Vec<i16> {
    samples(AdtsAacSource::new(Cursor::new(bytes.to_vec())).unwrap()).unwrap()
}

/// Like `assert_eq!`, without printing thousands of samples.
#[track_caller]
fn assert_same(actual: &[i16], expected: &[i16]) {
    assert_eq!(actual.len(), expected.len(), "sample count");
    let first_difference = actual.iter().zip(expected).position(|(a, e)| a != e);
    assert_eq!(first_difference, None, "first differing sample");
}

#[test]
fn adts_decodes_every_frame() {
    let (_, frames) = adts_frames(MONO);
    let source = AdtsAacSource::new(Cursor::new(MONO.to_vec())).unwrap();
    assert_eq!((source.sample_rate(), source.channels()), (44_100, 1));
    assert_eq!(samples(source).unwrap().len(), frames.len() * 1024);

    let (_, frames) = adts_frames(STEREO);
    let source = AdtsAacSource::new(Cursor::new(STEREO.to_vec())).unwrap();
    assert_eq!((source.sample_rate(), source.channels()), (48_000, 2));
    assert_eq!(samples(source).unwrap().len(), frames.len() * 1024 * 2);
}

#[test]
fn adts_skips_a_leading_id3_tag() {
    let mut tagged = b"ID3\x04\0\0\0\0\x01\x00".to_vec();
    tagged.extend([0; 128]);
    tagged.extend(MONO);
    assert_same(&adts(&tagged), &adts(MONO));
}

/// Uploads can arrive in reads of any size.
#[test]
fn adts_reads_one_byte_at_a_time() {
    struct OneByte<R>(R);
    impl<R: Read> Read for OneByte<R> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let len = buf.len().min(1);
            self.0.read(&mut buf[..len])
        }
    }
    let mut tagged = b"ID3\x04\0\0\0\0\x01\x00".to_vec();
    tagged.extend([0; 128]);
    tagged.extend(MONO);
    let source = AdtsAacSource::new(OneByte(Cursor::new(tagged))).unwrap();
    assert_same(&samples(source).unwrap(), &adts(MONO));
}

#[test]
fn adts_stops_at_a_trailing_tag() {
    let mut id3v1 = MONO.to_vec();
    id3v1.extend(b"TAG");
    id3v1.extend([0; 125]);
    let mut ape = MONO.to_vec();
    ape.extend(b"APETAGEX");
    ape.extend([0; 24]);
    for tagged in [id3v1, ape] {
        assert_same(&adts(&tagged), &adts(MONO));
    }
}

#[test]
fn rejects_other_inputs() {
    let result = AdtsAacSource::new(Cursor::new(opus_like_garbage()));
    assert!(matches!(result, Err(Error::InvalidInput(_))), "{result:?}");
}

fn opus_like_garbage() -> Vec<u8> {
    let mut bytes = b"OggS\0\x02".to_vec();
    bytes.extend([0; 60]);
    bytes
}
