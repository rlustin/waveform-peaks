//! MPEG audio streams, and MPEG audio in MP4 files built in memory around the
//! frames of small fixtures.

#![cfg(feature = "mp3")]

mod common;

use std::io::{self, Cursor, Read};

use common::*;
use waveform_peaks::{Error, Mp4Source, MpegAudioSource, PcmSource};

/// FFmpeg's MP3 with an Info tag whose LAME extension, written by `Lavc`,
/// records an encoder delay of 576.
const MP3: &[u8] = include_bytes!("data/sine-mono-44100.mp3");
/// FFmpeg's MP2, with no tag.
const MP2: &[u8] = include_bytes!("data/chord-stereo-48000.mp2");

/// The encoder delay of `MP3`, plus 529 frames of decoder delay.
const MP3_TRIM: usize = 576 + 529;

fn stream(bytes: &[u8]) -> Result<Vec<i16>, Error> {
    samples(MpegAudioSource::new(Cursor::new(bytes.to_vec()))?)
}

fn mp4(bytes: Vec<u8>) -> Result<Vec<i16>, Error> {
    samples(Mp4Source::new(Cursor::new(bytes))?)
}

/// The frames of `MP3` after its Info tag frame.
fn mp3_audio_frames() -> Vec<Vec<u8>> {
    mpa_frames(MP3)[1..].to_vec()
}

/// `MP3` without its Info tag, so decoded untrimmed.
fn bare_mp3() -> Vec<i16> {
    stream(&mp3_audio_frames().concat()).unwrap()
}

fn mpa_track(object_type: u8, channels: u16, rate: u32, frames: &[Vec<u8>]) -> Mp4Track {
    Mp4Track {
        sample_entry: audio_sample_entry(b"mp4a", channels, rate, &esds(object_type, &[])),
        ..Mp4Track::audio(&[], channels, rate, frames, 10)
    }
}

/// Rewrites the four bytes of `MP3` at `at` bytes past the Info tag id.
fn patch_info(at: usize, bytes: &[u8; 4]) -> Vec<u8> {
    let mut patched = MP3.to_vec();
    let info = patched.windows(4).position(|w| w == b"Info").unwrap();
    patched[info + at..info + at + 4].copy_from_slice(bytes);
    patched
}

/// Offset of the LAME extension from the Info tag id: id, flags, frame and
/// byte counts, table of contents and quality.
const LAME_AT: usize = 4 + 4 + 4 + 4 + 100 + 4;

#[test]
fn decodes_every_frame() {
    let frames = mpa_frames(MP2).len();
    let source = MpegAudioSource::new(Cursor::new(MP2)).unwrap();
    assert_eq!((source.sample_rate(), source.channels()), (48_000, 2));
    assert_eq!(samples(source).unwrap().len(), frames * 1152 * 2);
}

/// As audiowaveform does: the tag frame is not decoded, the delay is trimmed
/// and the end padding kept.
#[test]
fn info_tag_trims_the_delay() {
    let source = MpegAudioSource::new(Cursor::new(MP3)).unwrap();
    assert_eq!((source.sample_rate(), source.channels()), (44_100, 1));
    let bare = bare_mp3();
    assert_eq!(bare.len(), mp3_audio_frames().len() * 1152);
    assert_same(&samples(source).unwrap(), &bare[MP3_TRIM..]);
}

#[test]
fn delay_is_trimmed_only_for_lame_and_ffmpeg() {
    let mut lame = patch_info(LAME_AT, b"LAME");
    assert_same(&stream(&lame).unwrap(), &bare_mp3()[MP3_TRIM..]);
    lame = patch_info(LAME_AT, b"Lavf");
    assert_same(&stream(&lame).unwrap(), &bare_mp3());
}

/// A file whose tag is not recognised decodes its tag frame as audio.
#[test]
fn unrecognised_tag_frame_is_decoded() {
    let decoded = stream(&patch_info(0, b"Nope")).unwrap();
    assert_eq!(decoded.len(), mpa_frames(MP3).len() * 1152);
}

/// The tag's flags locate the LAME extension, which must lie in the frame.
#[test]
fn lame_extension_is_found_by_the_flags() {
    let no_flags = patch_info(4, &[0; 4]);
    // 32 kbit/s shortens the tag frame to 104 bytes, before the extension.
    let first = &mpa_frames(MP3)[0];
    let mut short = first[..104].to_vec();
    short[2] = 0x10 | short[2] & 0x0D;
    let short = [&short[..], &mp3_audio_frames().concat()].concat();
    for file in [no_flags, short] {
        assert_same(&stream(&file).unwrap(), &bare_mp3());
    }
}

#[test]
fn largest_delay_does_not_overflow() {
    let mut patched = MP3.to_vec();
    let info = patched.windows(4).position(|w| w == b"Info").unwrap();
    patched[info + LAME_AT + 21..info + LAME_AT + 24].copy_from_slice(&[0xFF, 0xF0, 0]);
    let decoded = stream(&patched).unwrap();
    assert_same(&decoded, &bare_mp3()[4095 + 529..]);
}

#[test]
fn skips_a_leading_id3_tag() {
    let mut tagged = b"ID3\x04\0\0\0\0\x01\x00".to_vec();
    tagged.extend([0; 128]);
    tagged.extend(MP3);
    assert_same(&stream(&tagged).unwrap(), &stream(MP3).unwrap());
}

/// Uploads can arrive in reads of any size.
#[test]
fn reads_one_byte_at_a_time() {
    struct OneByte<R>(R);
    impl<R: Read> Read for OneByte<R> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let len = buf.len().min(1);
            self.0.read(&mut buf[..len])
        }
    }
    let source = MpegAudioSource::new(OneByte(Cursor::new(MP3))).unwrap();
    assert_same(&samples(source).unwrap(), &stream(MP3).unwrap());
}

#[test]
fn stops_at_a_trailing_tag() {
    let mut id3v1 = MP2.to_vec();
    id3v1.extend(b"TAG");
    id3v1.extend([0; 125]);
    let mut ape = MP2.to_vec();
    ape.extend(b"APETAGEX");
    ape.extend([0; 24]);
    for tagged in [id3v1, ape] {
        assert_same(&stream(&tagged).unwrap(), &stream(MP2).unwrap());
    }
}

/// Data that is not a frame. audiowaveform skips each of them.
fn junks() -> Vec<(&'static str, Vec<u8>)> {
    let header = &mpa_frames(MP2)[0][..4];
    let mut id3v2 = b"ID3\x04\0\0\0\0\0\x20".to_vec();
    id3v2.extend([0xFF; 32]);
    vec![
        ("zeros", vec![0; 10]),
        ("sync bytes", vec![0xFF; 10]),
        (
            "a header that no frame follows",
            [&[0; 3], header, &[0; 20]].concat(),
        ),
        ("an ID3v2 tag", id3v2),
        ("an ID3v1 tag", [&b"TAG"[..], &[0; 125]].concat()),
    ]
}

/// Frames dropped as libmad drops them after skipped data: while the main
/// data of a frame starts in earlier frames that were not read.
fn reservoir_drops(frames: &[Vec<u8>]) -> usize {
    let mut read = 0;
    let mut dropped = 0;
    for frame in frames {
        // MPEG-1 mono without CRC: 9 bits of main_data_begin after the
        // header, in 17 bytes of side information.
        let begin = (frame[4] as usize) << 1 | (frame[5] >> 7) as usize;
        if begin <= read {
            break;
        }
        read += frame.len() - 4 - 17;
        dropped += 1;
    }
    dropped
}

#[test]
fn skips_data_between_frames() {
    // MP2 has no bit reservoir, so the frames after the junk decode.
    let frames = mpa_frames(MP2);
    let expected = stream(MP2).unwrap();
    for (name, junk) in junks() {
        let file = [frames[..5].concat(), junk, frames[5..].concat()].concat();
        assert!(stream(&file).unwrap() == expected, "{name}");
    }
}

#[test]
fn skips_leading_junk() {
    let expected = stream(MP3).unwrap();
    for (name, junk) in junks() {
        let file = [&junk[..], MP3].concat();
        assert!(stream(&file).unwrap() == expected, "{name}");
    }
}

/// Past skipped data, a header is taken only when the header of another
/// frame follows it, so a last frame after junk is dropped.
#[test]
fn frame_after_junk_needs_a_next_frame() {
    let frames = mpa_frames(MP2);
    let file = [frames[..5].concat(), vec![0; 10], frames[5].clone()].concat();
    assert_eq!(stream(&file).unwrap().len(), 5 * 1152 * 2);
}

#[test]
fn junk_drops_frames_that_need_the_reservoir() {
    let frames = mp3_audio_frames();
    let at = 10;
    let dropped = reservoir_drops(&frames[at..]);
    assert!(dropped > 0);
    let file = [frames[..at].concat(), vec![0; 10], frames[at..].concat()].concat();
    assert_eq!(
        stream(&file).unwrap().len(),
        (frames.len() - dropped) * 1152
    );
}

/// A stream cut from a longer one also starts with an empty reservoir.
#[test]
fn cut_start_drops_frames_that_need_the_reservoir() {
    let frames = &mp3_audio_frames()[10..];
    let dropped = reservoir_drops(frames);
    assert!(dropped > 0);
    let file = frames.concat();
    assert_eq!(
        stream(&file).unwrap().len(),
        (frames.len() - dropped) * 1152
    );
}

/// As libmad does, a frame that does not decode is dropped.
#[test]
fn drops_a_frame_that_does_not_decode() {
    let frames = mp3_audio_frames();
    let mut damaged = frames[0].clone();
    damaged[4..].fill(0xFF);
    let file = [damaged, frames.concat()].concat();
    assert_same(&stream(&file).unwrap(), &bare_mp3());
}

/// libmad keeps the main data of a frame it rejects, so the frames after it
/// that need this data still decode.
#[test]
fn frame_that_does_not_decode_keeps_its_reservoir_data() {
    let mut frames = mp3_audio_frames();
    set_big_values(&mut frames[10], 511);
    assert_eq!(
        stream(&frames.concat()).unwrap().len(),
        (frames.len() - 1) * 1152
    );
}

/// Sets `big_values` of the first granule in an MPEG-1 mono frame without
/// CRC: after 9 bits of main_data_begin, 5 private bits, 4 bits of scfsi and
/// 12 bits of part2_3_length.
fn set_big_values(frame: &mut [u8], value: u16) {
    for bit in 0..9 {
        let (byte, shift) = (4 + (30 + bit) / 8, 7 - (30 + bit) % 8);
        let set = (value >> (8 - bit) & 1) as u8;
        frame[byte] = frame[byte] & !(1 << shift) | set << shift;
    }
}

/// Junk with many headers decodes to nothing. The window of fixed size
/// keeps the memory constant.
#[test]
fn skips_hostile_junk() {
    // MPEG-2.5 layer II at 160 kbit/s and 8 kHz with padding: the largest
    // frame, 2,881 bytes. The next header is never where the frame ends. The
    // first byte keeps the first header from being trusted as a first frame.
    let junk = [&[0][..], &[0xFF, 0xE5, 0xEA, 0x00].repeat(256 * 1024)].concat();
    let result = stream(&junk);
    assert!(matches!(result, Err(Error::InvalidInput(_))), "{result:?}");
    let file = [&mpa_frames(MP2)[..3].concat(), &junk[..], MP2].concat();
    assert_eq!(
        stream(&file).unwrap().len(),
        (3 + mpa_frames(MP2).len()) * 1152 * 2
    );
}

/// As audiowaveform does, an incomplete last frame is dropped.
#[test]
fn cut_file_decodes_its_complete_frames() {
    let frames = mpa_frames(MP2);
    let kept = frames[..10].concat();
    for cut in [0, 2, 100] {
        let file = [&kept[..], &frames[10][..cut]].concat();
        assert_eq!(stream(&file).unwrap().len(), 10 * 1152 * 2, "{cut}");
    }
}

#[test]
fn rejects_malformed_streams() {
    let cut = &MP2[..100];
    let free_format = [0xFF, 0xFD, 0x04, 0xC4, 0, 0, 0, 0];
    let mp3_after_mp2 = [MP2, &mp3_audio_frames().concat()].concat();
    for (name, file) in [
        ("empty", &[][..]),
        ("cut inside the first frame", cut),
        ("ogg", b"OggS\0\x02\0\0"),
        ("layer change", &mp3_after_mp2),
    ] {
        let result = stream(file);
        assert!(
            matches!(result, Err(Error::InvalidInput(_))),
            "{name}: {result:?}"
        );
    }
    let result = stream(&free_format);
    assert!(matches!(result, Err(Error::Unsupported(_))), "{result:?}");
}

#[test]
fn seekable_mp4_reads_media_data_before_moov() {
    let track = mpa_track(0x6B, 2, 48_000, &mpa_frames(MP2));
    let file = media_first_mp4(&[track], 1000, &[]);
    let source = Mp4Source::new_seekable(Cursor::new(file)).unwrap();
    assert_eq!((source.sample_rate(), source.channels()), (48_000, 2));
    assert_same(&samples(source).unwrap(), &stream(MP2).unwrap());
}

#[test]
fn mp4_matches_the_stream_with_the_same_frames() {
    let frames = mp3_audio_frames();
    for object_type in [0x6B, 0x69] {
        let track = mpa_track(object_type, 1, 44_100, &frames);
        let file = faststart_mp4(&[track], 1000, &[], &[]);
        assert_same(&mp4(file).unwrap(), &bare_mp3());
    }
    let track = mpa_track(0x6B, 2, 48_000, &mpa_frames(MP2));
    let file = faststart_mp4(&[track], 1000, &[], &[]);
    let source = Mp4Source::new(Cursor::new(file.clone())).unwrap();
    assert_eq!((source.sample_rate(), source.channels()), (48_000, 2));
    assert_same(&mp4(file).unwrap(), &stream(MP2).unwrap());
}

/// QuickTime's `.mp3` and `.mp2` sample entries hold no configuration.
#[test]
fn reads_quicktime_sample_entries() {
    let mut track = mpa_track(0x6B, 1, 44_100, &mp3_audio_frames());
    track.sample_entry = audio_sample_entry(b".mp3", 1, 44_100, &[]);
    track.quicktime = true;
    assert_same(
        &mp4(faststart_mp4(&[track], 1000, &[], &[])).unwrap(),
        &bare_mp3(),
    );

    let mut track = mpa_track(0x6B, 2, 48_000, &mpa_frames(MP2));
    track.sample_entry = audio_sample_entry(b".mp2", 2, 48_000, &[]);
    assert_same(
        &mp4(faststart_mp4(&[track], 1000, &[], &[])).unwrap(),
        &stream(MP2).unwrap(),
    );
}

/// The rate and channels come from the frames, not the sample entry.
#[test]
fn sample_entry_fields_are_not_trusted() {
    let mut track = mpa_track(0x6B, 2, 48_000, &mp3_audio_frames());
    track.timescale = 44_100;
    let source = Mp4Source::new(Cursor::new(faststart_mp4(&[track], 1000, &[], &[]))).unwrap();
    assert_eq!((source.sample_rate(), source.channels()), (44_100, 1));
}

#[test]
fn edit_list_trims_the_delay() {
    let mut track = mpa_track(0x6B, 1, 44_100, &mp3_audio_frames());
    track.edit = Some((1013, MP3_TRIM as i64));
    let file = faststart_mp4(&[track], 1000, &[], &[]);
    let length = 44_673;
    assert_same(
        &mp4(file).unwrap(),
        &bare_mp3()[MP3_TRIM..MP3_TRIM + length],
    );
}

#[test]
fn reads_fragmented_files() {
    let frames = mp3_audio_frames();
    let fragments: Vec<Vec<Vec<u8>>> = frames.chunks(7).map(<[_]>::to_vec).collect();
    let track = mpa_track(0x6B, 1, 44_100, &[]);
    for explicit_base in [false, true] {
        let file = fragmented_mp4(&track, &fragments, explicit_base);
        assert_same(&mp4(file).unwrap(), &bare_mp3());
    }
}

/// An empty first sample is passed over, as for the other codecs.
#[test]
fn passes_over_empty_samples() {
    let mut frames = mp3_audio_frames();
    frames.insert(0, Vec::new());
    frames.insert(5, Vec::new());
    let file = faststart_mp4(&[mpa_track(0x6B, 1, 44_100, &frames)], 1000, &[], &[]);
    assert_same(&mp4(file).unwrap(), &bare_mp3());
}

#[test]
fn rejects_malformed_mp4_samples() {
    let frames = mp3_audio_frames();
    // Unsupported from the constructor, so that a caller can fall back.
    let two_per_sample: Vec<Vec<u8>> = frames.chunks(2).map(<[_]>::concat).collect();
    let one_byte = vec![vec![0xFF]; 100];
    for samples in [two_per_sample, one_byte] {
        let file = faststart_mp4(&[mpa_track(0x6B, 1, 44_100, &samples)], 1000, &[], &[]);
        let result = Mp4Source::new(Cursor::new(file));
        assert!(matches!(result, Err(Error::Unsupported(_))), "{result:?}");
    }

    let mut short = frames.clone();
    short[3].truncate(100);
    let file = faststart_mp4(&[mpa_track(0x6B, 1, 44_100, &short)], 1000, &[], &[]);
    let result = mp4(file);
    assert!(matches!(result, Err(Error::InvalidInput(_))), "{result:?}");

    let mut mixed = frames.clone();
    mixed[3] = mpa_frames(MP2)[0].clone();
    let file = faststart_mp4(&[mpa_track(0x6B, 1, 44_100, &mixed)], 1000, &[], &[]);
    let result = mp4(file);
    assert!(matches!(result, Err(Error::InvalidInput(_))), "{result:?}");

    let file = faststart_mp4(
        &[mpa_track(0x6B, 1, 44_100, &[vec![0; 200]])],
        1000,
        &[],
        &[],
    );
    let result = mp4(file);
    assert!(matches!(result, Err(Error::InvalidInput(_))), "{result:?}");
}

/// As for AAC and Opus, a track without samples decodes to nothing, with the
/// rate and channels of its sample entry.
#[test]
fn track_without_samples_is_empty() {
    let track = mpa_track(0x6B, 2, 32_000, &[]);
    for file in [
        faststart_mp4(&[mpa_track(0x6B, 2, 32_000, &[Vec::new()])], 1000, &[], &[]),
        fragmented_mp4(&track, &[], false),
    ] {
        let source = Mp4Source::new(Cursor::new(file)).unwrap();
        assert_eq!((source.sample_rate(), source.channels()), (32_000, 2));
        assert_eq!(samples(source).unwrap(), []);
    }
    let no_rate = fragmented_mp4(&mpa_track(0x6B, 2, 0, &[]), &[], false);
    let result = mp4(no_rate);
    assert!(matches!(result, Err(Error::InvalidInput(_))), "{result:?}");
}

#[test]
fn other_object_types_are_passed_over() {
    let mut other = mpa_track(0x6C, 1, 44_100, &[vec![0; 10]]);
    other.id = 2;
    let file = faststart_mp4(
        &[other, mpa_track(0x6B, 1, 44_100, &mp3_audio_frames())],
        1000,
        &[],
        &[],
    );
    assert_same(&mp4(file).unwrap(), &bare_mp3());
}
