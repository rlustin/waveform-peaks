//! WebM framing on files built in memory with libopus's encoder.

#![cfg(feature = "opus")]

mod common;

use std::io::Cursor;

use common::*;
use waveform_peaks::{Error, OggOpusSource, OpusOptions, PcmSource, WebmOpusSource};

const PACKETS: u64 = 30;
const END_TRIM: u64 = 100;

fn samples<P: PcmSource>(mut source: P) -> Result<Vec<i16>, Error> {
    let mut samples = Vec::new();
    while let Some(block) = source.next_block()? {
        samples.extend(block);
    }
    Ok(samples)
}

fn webm(bytes: Vec<u8>) -> Result<Vec<i16>, Error> {
    webm_with(bytes, OpusOptions::default())
}

fn webm_with(bytes: Vec<u8>, options: OpusOptions) -> Result<Vec<i16>, Error> {
    samples(WebmOpusSource::with_options(Cursor::new(bytes), options)?)
}

/// The same packets in Ogg, the reference for the WebM tests.
fn ogg_reference() -> Vec<i16> {
    let bytes = opus_stream(1, PACKETS, END_TRIM).concat();
    samples(OggOpusSource::new(Cursor::new(bytes)).unwrap()).unwrap()
}

/// Like `assert_eq!`, without printing thousands of samples.
#[track_caller]
fn assert_same(actual: &[i16], expected: &[i16]) {
    assert_eq!(actual.len(), expected.len(), "sample count");
    let first_difference = actual.iter().zip(expected).position(|(a, e)| a != e);
    assert_eq!(first_difference, None, "first differing sample");
}

fn simple_block(packet: &[u8]) -> Vec<u8> {
    element(SIMPLE_BLOCK, &block_body(1, 0, &[packet]))
}

fn padded_block(packets: &[&[u8]], lacing: u8, padding_frames: u64) -> Vec<u8> {
    element(
        BLOCK_GROUP,
        &[
            element(BLOCK, &block_body(1, lacing, packets)),
            uint_element(DISCARD_PADDING, ns_for_48k_frames(padding_frames)),
        ]
        .concat(),
    )
}

/// Blocks for `opus_packets(PACKETS)`, the last one trimmed by `END_TRIM`.
fn blocks() -> Vec<Vec<u8>> {
    let packets = opus_packets(PACKETS);
    let (last, rest) = packets.split_last().unwrap();
    let mut blocks: Vec<Vec<u8>> = rest.iter().map(|packet| simple_block(packet)).collect();
    blocks.push(padded_block(&[last], 0, END_TRIM));
    blocks
}

fn tracks() -> Vec<u8> {
    let delay = ns_for_48k_frames(OPUS_PRE_SKIP as u64);
    element(TRACKS, &opus_track(1, OPUS_PRE_SKIP, Some(delay)))
}

fn file(tracks: Vec<u8>, clusters: Vec<u8>) -> Vec<u8> {
    [
        ebml_header("webm"),
        element(SEGMENT, &[tracks, clusters].concat()),
    ]
    .concat()
}

fn cluster(blocks: &[Vec<u8>]) -> Vec<u8> {
    element(
        CLUSTER,
        &[uint_element(TIMESTAMP, 0), blocks.concat()].concat(),
    )
}

#[test]
fn matches_ogg_with_the_same_packets() {
    let reference = ogg_reference();
    assert_eq!(
        reference.len() as u64,
        PACKETS * OPUS_PACKET_FRAMES - OPUS_PRE_SKIP as u64 - END_TRIM
    );
    assert_same(
        &webm(file(tracks(), cluster(&blocks()))).unwrap(),
        &reference,
    );
}

/// Both containers must round the end position the same way below 48 kHz:
/// the 28,700-frame end at 48 kHz is not a whole number of 16 kHz frames.
#[test]
fn matches_ogg_below_48_khz() {
    let ogg = opus_stream(1, PACKETS, END_TRIM).concat();
    let ogg = OggOpusSource::new(Cursor::new(patch_opus_head(&ogg, Some(16_000), None))).unwrap();
    assert_eq!(ogg.sample_rate(), 16_000);
    let reference = samples(ogg).unwrap();

    let mut head = opus_head(1, OPUS_PRE_SKIP);
    head[12..16].copy_from_slice(&16_000u32.to_le_bytes());
    let track = element(
        TRACK_ENTRY,
        &[
            uint_element(TRACK_NUMBER, 1),
            element(CODEC_ID, b"A_OPUS"),
            element(CODEC_PRIVATE, &head),
            uint_element(CODEC_DELAY, ns_for_48k_frames(OPUS_PRE_SKIP as u64)),
        ]
        .concat(),
    );
    let bytes = file(element(TRACKS, &track), cluster(&blocks()));
    assert_same(&webm(bytes).unwrap(), &reference);
}

/// EBML lets strings end in zero bytes and Void elements sit at any level.
#[test]
fn accepts_null_padded_strings_and_void_before_the_segment() {
    let track = element(
        TRACK_ENTRY,
        &[
            uint_element(TRACK_NUMBER, 1),
            element(CODEC_ID, b"A_OPUS\0\0"),
            element(CODEC_PRIVATE, &opus_head(1, OPUS_PRE_SKIP)),
            uint_element(CODEC_DELAY, ns_for_48k_frames(OPUS_PRE_SKIP as u64)),
        ]
        .concat(),
    );
    let bytes = [
        ebml_header("webm\0"),
        element(VOID, &[0; 4]),
        element(
            SEGMENT,
            &[element(TRACKS, &track), cluster(&blocks())].concat(),
        ),
    ]
    .concat();
    assert_same(&webm(bytes).unwrap(), &ogg_reference());
}

#[test]
fn passes_over_an_encrypted_opus_track() {
    let encrypted = element(
        TRACK_ENTRY,
        &[
            uint_element(TRACK_NUMBER, 2),
            element(CODEC_ID, b"A_OPUS"),
            element(CODEC_PRIVATE, &opus_head(1, OPUS_PRE_SKIP)),
            element(CONTENT_ENCODINGS, &[]),
        ]
        .concat(),
    );
    let plain = opus_track(
        1,
        OPUS_PRE_SKIP,
        Some(ns_for_48k_frames(OPUS_PRE_SKIP as u64)),
    );
    let tracks = element(TRACKS, &[encrypted, plain].concat());
    assert_same(
        &webm(file(tracks, cluster(&blocks()))).unwrap(),
        &ogg_reference(),
    );
}

/// Live recorders such as `MediaRecorder` cannot go back to write sizes.
#[test]
fn reads_segments_and_clusters_of_unknown_size() {
    let blocks = blocks();
    let (first, second) = blocks.split_at(10);
    let bytes = [
        ebml_header("webm"),
        unknown_size(SEGMENT),
        tracks(),
        unknown_size(CLUSTER),
        first.concat(),
        unknown_size(CLUSTER),
        uint_element(TIMESTAMP, 200),
        second.concat(),
        element(CUES, &[0; 16]),
    ]
    .concat();
    assert_same(&webm(bytes).unwrap(), &ogg_reference());
}

#[test]
fn laced_blocks_decode_like_separate_blocks() {
    let packets = opus_packets(PACKETS);
    let packets: Vec<&[u8]> = packets.iter().map(Vec::as_slice).collect();
    let unlaced: Vec<Vec<u8>> = packets.iter().map(|packet| simple_block(packet)).collect();
    let untrimmed = webm(file(tracks(), cluster(&unlaced))).unwrap();

    // Padding longer than the last packet trims into the one before it.
    let padding = OPUS_PACKET_FRAMES + END_TRIM;
    let expected = &untrimmed[..untrimmed.len() - padding as usize];
    let chunks: Vec<&[&[u8]]> = packets.chunks(4).collect();
    for lacing in [1, 3] {
        let laced: Vec<Vec<u8>> = chunks
            .iter()
            .enumerate()
            .map(|(index, chunk)| {
                let last = index + 1 == chunks.len();
                padded_block(chunk, lacing, if last { padding } else { 0 })
            })
            .collect();
        assert_same(&webm(file(tracks(), cluster(&laced))).unwrap(), expected);
    }

    // A laced block can hold a single frame, which leaves no sizes to read.
    let unlaced_untrimmed = untrimmed;
    for lacing in [1, 2, 3] {
        let single: Vec<Vec<u8>> = packets
            .iter()
            .map(|packet| padded_block(&[packet], lacing, 0))
            .collect();
        assert_same(
            &webm(file(tracks(), cluster(&single))).unwrap(),
            &unlaced_untrimmed,
        );
    }

    // Fixed lacing needs equal sizes, so repeat one packet.
    let repeated = [packets[0]; 3];
    let separate = vec![simple_block(packets[0]); 3];
    assert_same(
        &webm(file(tracks(), cluster(&[padded_block(&repeated, 2, 0)]))).unwrap(),
        &webm(file(tracks(), cluster(&separate))).unwrap(),
    );
}

/// Other tracks and large elements are read past, so they count against
/// neither memory nor `max_packet_bytes`.
#[test]
fn skips_other_tracks_and_elements_without_buffering() {
    let video_track = element(
        TRACK_ENTRY,
        &[
            uint_element(TRACK_NUMBER, 2),
            uint_element(TRACK_TYPE, 1),
            element(CODEC_ID, b"V_VP8"),
        ]
        .concat(),
    );
    let tracks = element(
        TRACKS,
        &[
            video_track,
            opus_track(
                1,
                OPUS_PRE_SKIP,
                Some(ns_for_48k_frames(OPUS_PRE_SKIP as u64)),
            ),
        ]
        .concat(),
    );
    let video_block = element(SIMPLE_BLOCK, &block_body(2, 0, &[&[0xAB; 100_000]]));
    let interleaved: Vec<Vec<u8>> = blocks()
        .into_iter()
        .flat_map(|block| [block, video_block.clone(), element(VOID, &[0; 8])])
        .collect();
    let bytes = file(
        tracks,
        [
            cluster(&interleaved),
            element(ATTACHMENTS, &vec![0; 1 << 20]),
        ]
        .concat(),
    );
    let mut options = OpusOptions::default();
    options.max_packet_bytes = 10_000;
    assert_same(&webm_with(bytes, options).unwrap(), &ogg_reference());
}

#[test]
fn codec_delay_takes_precedence_over_the_opus_head_pre_skip() {
    let reference = ogg_reference();
    let delay = ns_for_48k_frames(OPUS_PRE_SKIP as u64);
    let with_delay = element(TRACKS, &opus_track(1, 0, Some(delay)));
    assert_eq!(
        webm(file(with_delay, cluster(&blocks()))).unwrap(),
        reference
    );
    let head_only = element(TRACKS, &opus_track(1, OPUS_PRE_SKIP, None));
    assert_eq!(
        webm(file(head_only, cluster(&blocks()))).unwrap(),
        reference
    );
}

#[test]
fn cut_between_elements_decodes_what_is_there() {
    let blocks = blocks();
    let kept = 12;
    let bytes = [
        ebml_header("webm"),
        unknown_size(SEGMENT),
        tracks(),
        unknown_size(CLUSTER),
        blocks[..kept].concat(),
    ]
    .concat();
    let decoded = webm(bytes).unwrap();
    assert_eq!(
        decoded.len() as u64,
        kept as u64 * OPUS_PACKET_FRAMES - OPUS_PRE_SKIP as u64
    );
    assert_same(&decoded, &ogg_reference()[..decoded.len()]);
}

#[test]
fn cut_inside_an_element_is_rejected() {
    let bytes = file(tracks(), cluster(&blocks()));
    for cut in [bytes.len() - 5, 40] {
        let result = webm(bytes[..cut].to_vec());
        assert!(
            matches!(result, Err(Error::InvalidInput(_))),
            "{cut}: {result:?}"
        );
    }
}

/// Only segments and clusters may have an unknown size; found by fuzzing.
#[test]
fn unknown_size_inside_a_block_group_is_rejected() {
    let mut group_body = unknown_size(BLOCK);
    group_body.extend(block_body(1, 0, &[&[0xFC, 0xFF, 0xFE]]));
    let bytes = file(tracks(), cluster(&[element(BLOCK_GROUP, &group_body)]));
    let result = webm(bytes);
    assert!(matches!(result, Err(Error::InvalidInput(_))), "{result:?}");
}

#[test]
fn oversized_block_is_rejected() {
    let mut options = OpusOptions::default();
    options.max_packet_bytes = 50;
    let result = webm_with(file(tracks(), cluster(&blocks())), options);
    assert!(matches!(result, Err(Error::LimitExceeded(_))), "{result:?}");
}

#[test]
fn rejects_other_containers_codecs_and_layouts() {
    let ogg = opus_stream(1, 2, 0).concat();
    assert!(matches!(webm(ogg), Err(Error::InvalidInput(_))));

    let other_doc = [ebml_header("x-custom"), element(SEGMENT, &tracks())].concat();
    assert!(matches!(webm(other_doc), Err(Error::Unsupported(_))));

    let vorbis = element(
        TRACKS,
        &element(
            TRACK_ENTRY,
            &[
                uint_element(TRACK_NUMBER, 1),
                element(CODEC_ID, b"A_VORBIS"),
            ]
            .concat(),
        ),
    );
    let result = webm(file(vorbis, cluster(&blocks())));
    assert!(
        matches!(&result, Err(Error::Unsupported(m)) if m.contains("A_VORBIS")),
        "{result:?}"
    );

    let encrypted = element(
        TRACKS,
        &element(
            TRACK_ENTRY,
            &[
                uint_element(TRACK_NUMBER, 1),
                element(CODEC_ID, b"A_OPUS"),
                element(CODEC_PRIVATE, &opus_head(1, OPUS_PRE_SKIP)),
                element(CONTENT_ENCODINGS, &[]),
            ]
            .concat(),
        ),
    );
    let result = webm(file(encrypted, cluster(&blocks())));
    assert!(matches!(result, Err(Error::Unsupported(_))), "{result:?}");

    let oversized_head = element(
        TRACKS,
        &element(
            TRACK_ENTRY,
            &[
                uint_element(TRACK_NUMBER, 1),
                element(CODEC_ID, b"A_OPUS"),
                element(CODEC_PRIVATE, &vec![0; 8192]),
            ]
            .concat(),
        ),
    );
    let result = webm(file(oversized_head, cluster(&blocks())));
    assert!(
        matches!(&result, Err(Error::InvalidInput(m)) if m.contains("oversized")),
        "{result:?}"
    );

    let cluster_first = [
        ebml_header("webm"),
        element(SEGMENT, &[cluster(&blocks()), tracks()].concat()),
    ]
    .concat();
    assert!(matches!(webm(cluster_first), Err(Error::InvalidInput(_))));
}
