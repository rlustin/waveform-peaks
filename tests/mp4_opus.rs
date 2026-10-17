//! Opus in MP4 on files built in memory with libopus's encoder.

#![cfg(feature = "opus")]

mod common;

use std::io::Cursor;

use common::*;
use waveform_peaks::{DecodeRate, Error, Mp4Options, Mp4Source, OggOpusSource, PcmSource};

/// Recorded by Chrome 155's `MediaRecorder`: fragmented and stereo, with the
/// sample tables before the sample description and no edit list.
const CHROME: &[u8] = include_bytes!("data/chrome-opus-stereo.mp4");
/// Recorded by Safari 27's `MediaRecorder`: fragmented and stereo, with
/// 2.5 ms packets and an edit list of unknown duration.
const SAFARI: &[u8] = include_bytes!("data/safari-opus-stereo.mp4");

const PACKETS: u64 = 30;
const END_TRIM: u64 = 100;
/// Frames left after trimming the pre-skip and `END_TRIM`.
const VALID: u64 = PACKETS * OPUS_PACKET_FRAMES - OPUS_PRE_SKIP as u64 - END_TRIM;

fn mp4(bytes: Vec<u8>) -> Result<Vec<i16>, Error> {
    mp4_with(bytes, Mp4Options::default())
}

fn mp4_with(bytes: Vec<u8>, options: Mp4Options) -> Result<Vec<i16>, Error> {
    samples(Mp4Source::with_options(Cursor::new(bytes), options)?)
}

/// The same packets in Ogg, trimmed by `end_trim`: the reference for MP4.
fn ogg_reference(end_trim: u64) -> Vec<i16> {
    let bytes = opus_stream(1, PACKETS, end_trim).concat();
    samples(OggOpusSource::new(Cursor::new(bytes)).unwrap()).unwrap()
}

fn track(dops_box: &[u8], per_chunk: usize) -> Mp4Track {
    Mp4Track::opus(dops_box, &opus_packets(PACKETS), per_chunk)
}

/// A mono track trimmed as the Opus in ISOBMFF specification asks: an edit
/// list skipping the pre-skip and ending after `VALID` frames.
fn trimmed_track(per_chunk: usize) -> Mp4Track {
    let mut track = track(&dops(1, OPUS_PRE_SKIP, 48_000, 0), per_chunk);
    track.edit = Some((VALID, OPUS_PRE_SKIP as i64));
    track
}

#[test]
fn matches_ogg_with_the_same_packets() {
    let reference = ogg_reference(END_TRIM);
    assert_eq!(reference.len() as u64, VALID);
    for per_chunk in [1, 7, 1000] {
        let file = faststart_mp4(&[trimmed_track(per_chunk)], 48_000, &[], &[]);
        let source = Mp4Source::new(Cursor::new(file.clone())).unwrap();
        assert_eq!((source.sample_rate(), source.channels()), (48_000, 1));
        assert_same(&mp4(file).unwrap(), &reference);
    }
}

#[test]
fn seekable_reads_media_data_before_moov() {
    let file = media_first_mp4(&[trimmed_track(7)], 48_000, &[]);
    let decoded = samples(Mp4Source::new_seekable(Cursor::new(file)).unwrap()).unwrap();
    assert_same(&decoded, &ogg_reference(END_TRIM));
}

#[test]
fn reads_chrome_recordings() {
    let source = Mp4Source::new(Cursor::new(CHROME)).unwrap();
    assert_eq!((source.sample_rate(), source.channels()), (48_000, 2));
    // 45 packets of 960 frames, less Chrome's 3,840-frame (80 ms) dOps
    // pre-skip, as FFmpeg also decodes it.
    assert_eq!(samples(source).unwrap().len(), 39_360 * 2);
}

#[test]
fn reads_safari_recordings() {
    let source = Mp4Source::new(Cursor::new(SAFARI)).unwrap();
    assert_eq!((source.sample_rate(), source.channels()), (48_000, 2));
    // 401 packets of 120 frames, less the edit list's 312-frame pre-skip, as
    // FFmpeg also decodes it.
    assert_eq!(samples(source).unwrap().len(), 47_808 * 2);
}

/// The edit list is in the media timescale, which need not be 48 kHz.
#[test]
fn converts_the_edit_list_from_the_media_timescale() {
    let mut track = trimmed_track(10);
    track.timescale = 24_000;
    track.edit = Some((VALID, OPUS_PRE_SKIP as i64 / 2));
    let file = faststart_mp4(&[track], 48_000, &[], &[]);
    assert_same(&mp4(file).unwrap(), &ogg_reference(END_TRIM));
}

/// Without an edit list, only the `dOps` pre-skip is trimmed.
#[test]
fn trims_the_dops_pre_skip_without_an_edit_list() {
    let file = faststart_mp4(
        &[track(&dops(1, OPUS_PRE_SKIP, 48_000, 0), 10)],
        1000,
        &[],
        &[],
    );
    assert_same(&mp4(file).unwrap(), &ogg_reference(0));
}

/// The specification makes the `dOps` pre-skip informative only.
#[test]
fn edit_list_takes_precedence_over_dops() {
    let mut track = track(&dops(1, 0, 48_000, 0), 10);
    track.edit = Some((VALID, OPUS_PRE_SKIP as i64));
    let file = faststart_mp4(&[track], 48_000, &[], &[]);
    assert_same(&mp4(file).unwrap(), &ogg_reference(END_TRIM));
}

/// Recorders write fragments, with an edit list of unknown duration.
#[test]
fn reads_fragmented_files() {
    let packets = opus_packets(PACKETS);
    let fragments: Vec<Vec<Vec<u8>>> = packets.chunks(7).map(<[_]>::to_vec).collect();
    for edit in [None, Some((0, OPUS_PRE_SKIP as i64))] {
        let mut track = track(&dops(1, OPUS_PRE_SKIP, 48_000, 0), 1);
        track.edit = edit;
        for explicit_base in [false, true] {
            let file = fragmented_mp4(&track, &fragments, explicit_base);
            assert_same(&mp4(file).unwrap(), &ogg_reference(0));
        }
    }
}

/// Both containers must round the end position the same way below 48 kHz.
#[test]
fn matches_ogg_below_48_khz() {
    let ogg = opus_stream(1, PACKETS, END_TRIM).concat();
    let ogg = OggOpusSource::new(Cursor::new(patch_opus_head(&ogg, Some(16_000), None))).unwrap();
    let reference = samples(ogg).unwrap();

    let mut track = track(&dops(1, OPUS_PRE_SKIP, 16_000, 0), 10);
    track.edit = Some((VALID, OPUS_PRE_SKIP as i64));
    let file = faststart_mp4(&[track], 48_000, &[], &[]);
    let source = Mp4Source::new(Cursor::new(file.clone())).unwrap();
    assert_eq!(source.sample_rate(), 16_000);
    assert_same(&mp4(file.clone()).unwrap(), &reference);

    let mut options = Mp4Options::default();
    options.opus.decode_rate = DecodeRate::Full;
    let source = Mp4Source::with_options(Cursor::new(file.clone()), options).unwrap();
    assert_eq!(source.sample_rate(), 48_000);
    assert_same(&mp4_with(file, options).unwrap(), &ogg_reference(END_TRIM));
}

#[test]
fn applies_the_output_gain() {
    let ogg = opus_stream(1, PACKETS, END_TRIM).concat();
    let ogg = OggOpusSource::new(Cursor::new(patch_opus_head(&ogg, None, Some(-1536)))).unwrap();
    let mut track = track(&dops(1, OPUS_PRE_SKIP, 48_000, -1536), 10);
    track.edit = Some((VALID, OPUS_PRE_SKIP as i64));
    let file = faststart_mp4(&[track], 48_000, &[], &[]);
    assert_same(&mp4(file).unwrap(), &samples(ogg).unwrap());
}

/// Other tracks' chunks sit between the Opus track's in the media data.
#[test]
fn skips_other_tracks() {
    let video = Mp4Track {
        id: 2,
        handler: *b"vide",
        sample_entry: mp4_box(b"avc1", &[0; 78]),
        timescale: 90_000,
        edit: None,
        chunks: vec![vec![vec![0xEE; 5_000]]; 8],
        wide_offsets: false,
        quicktime: false,
    };
    let file = faststart_mp4(&[video, trimmed_track(4)], 48_000, &[], &[]);
    assert_same(&mp4(file).unwrap(), &ogg_reference(END_TRIM));
}

#[test]
fn rejects_unsupported_and_malformed_dops() {
    let dops_with = |patch: &dyn Fn(&mut Vec<u8>)| {
        let mut dops_box = dops(1, OPUS_PRE_SKIP, 48_000, 0);
        patch(&mut dops_box);
        mp4(faststart_mp4(&[track(&dops_box, 10)], 1000, &[], &[]))
    };
    // Version 1, then channel mapping family 1 (surround).
    for at in [8, 18] {
        let result = dops_with(&|dops_box| dops_box[at] = 1);
        assert!(matches!(result, Err(Error::Unsupported(_))), "{result:?}");
    }
    let three_channels = dops_with(&|dops_box| dops_box[9] = 3);
    assert!(
        matches!(three_channels, Err(Error::InvalidInput(_))),
        "{three_channels:?}"
    );
    let truncated = mp4(faststart_mp4(
        &[track(&mp4_box(b"dOps", &[0, 1, 0]), 10)],
        1000,
        &[],
        &[],
    ));
    assert!(
        matches!(truncated, Err(Error::InvalidInput(_))),
        "{truncated:?}"
    );
    let missing = mp4(faststart_mp4(&[track(&[], 10)], 1000, &[], &[]));
    assert!(
        matches!(missing, Err(Error::InvalidInput(_))),
        "{missing:?}"
    );
}

/// A sample size is read before the sample, so it must be bounded before
/// anything is allocated for it.
#[test]
fn oversized_samples_are_rejected() {
    for size in [64 * 1024 + 1, u32::MAX] {
        let mut file = faststart_mp4(&[trimmed_track(10)], 48_000, &[], &[]);
        let stsz = file.windows(4).position(|w| w == b"stsz").unwrap();
        file[stsz + 16..stsz + 20].copy_from_slice(&size.to_be_bytes());
        let result = mp4(file);
        assert!(
            matches!(result, Err(Error::InvalidInput(_))),
            "{size}: {result:?}"
        );
    }
}

/// libopus's decoder is not `Sync`, which the source must not inherit.
#[test]
fn source_is_send_and_sync() {
    fn send_and_sync<T: Send + Sync>() {}
    send_and_sync::<Mp4Source<std::fs::File>>();
}

/// An AAC track Symphonia cannot decode is passed over like surround Opus.
#[cfg(feature = "aac")]
#[test]
fn passes_over_aac_it_cannot_decode() {
    // AAC-LC at 44.1 kHz, mono, with 960-sample frames.
    let mut aac = Mp4Track::audio(&[0x12, 0x0C], 1, 44_100, &[vec![0; 10]], 1);
    aac.id = 2;
    let file = faststart_mp4(&[aac, trimmed_track(10)], 48_000, &[], &[]);
    assert_same(&mp4(file).unwrap(), &ogg_reference(END_TRIM));
}

#[cfg(not(feature = "aac"))]
#[test]
fn aac_needs_its_feature() {
    let track = Mp4Track::audio(&[0x12, 0x08], 1, 44_100, &[vec![0; 10]], 1);
    let result = mp4(faststart_mp4(&[track], 1000, &[], &[]));
    assert!(
        matches!(&result, Err(Error::Unsupported(m)) if m.contains("without the aac")),
        "{result:?}"
    );
}

#[cfg(not(feature = "mp3"))]
#[test]
fn mpeg_audio_needs_its_feature() {
    for entry in [
        audio_sample_entry(b"mp4a", 1, 44_100, &esds(0x6B, &[])),
        audio_sample_entry(b".mp3", 1, 44_100, &[]),
    ] {
        let mut track = Mp4Track::audio(&[], 1, 44_100, &[vec![0; 10]], 1);
        track.sample_entry = entry;
        let result = mp4(faststart_mp4(&[track], 1000, &[], &[]));
        assert!(
            matches!(&result, Err(Error::Unsupported(m)) if m.contains("mp3 feature")),
            "{result:?}"
        );
    }
}

/// A track that cannot be decoded is passed over for the next one.
#[test]
fn passes_over_opus_it_cannot_decode() {
    let mut surround = dops(1, OPUS_PRE_SKIP, 48_000, 0);
    surround[18] = 1;
    let mut surround_track = track(&surround, 10);
    surround_track.id = 2;
    let file = faststart_mp4(&[surround_track, trimmed_track(10)], 48_000, &[], &[]);
    assert_same(&mp4(file).unwrap(), &ogg_reference(END_TRIM));
}

/// Without the features, an `mp4a` track is not parsed, so its malformed
/// `esds` box does not reject the file.
#[cfg(not(any(feature = "aac", feature = "mp3")))]
#[test]
fn passes_over_mp4a_without_its_feature() {
    let mut aac = Mp4Track::audio(&[0x12, 0x08], 1, 44_100, &[vec![0; 10]], 1);
    aac.id = 2;
    aac.sample_entry = audio_sample_entry(b"mp4a", 1, 44_100, &mp4_box(b"esds", &[0xFF; 3]));
    let file = faststart_mp4(&[aac, trimmed_track(10)], 48_000, &[], &[]);
    assert_same(&mp4(file).unwrap(), &ogg_reference(END_TRIM));
}

/// Nothing after the end of the edit is read, so a file cut there decodes.
#[test]
fn stops_at_the_end_of_the_edit() {
    let shown = 10;
    let mut track = track(&dops(1, OPUS_PRE_SKIP, 48_000, 0), 1);
    track.edit = Some((
        shown * OPUS_PACKET_FRAMES - OPUS_PRE_SKIP as u64,
        OPUS_PRE_SKIP as i64,
    ));
    let file = faststart_mp4(&[track], 48_000, &[], &[]);
    let cut = file[..file.len() - 100].to_vec();
    let reference =
        samples(OggOpusSource::new(Cursor::new(opus_stream(1, shown, 0).concat())).unwrap());
    assert_same(&mp4(cut).unwrap(), &reference.unwrap());
}
