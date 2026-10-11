//! Ogg Opus framing on streams built in memory with libopus's encoder.

#![cfg(feature = "opus")]

mod common;

use std::io::Cursor;

use common::{
    CONTINUED, FIRST, LAST, OPUS_PACKET_FRAMES, OPUS_PRE_SKIP, ogg_packet_page, ogg_packets_page,
    ogg_page, opus_head, opus_header_pages, opus_packets, opus_stream, opus_tags,
};
use waveform_peaks::{Error, OggOpusSource, OpusOptions, PcmSource};

fn frames(bytes: Vec<u8>) -> Result<u64, Error> {
    frames_with(bytes, OpusOptions::default())
}

fn frames_with(bytes: Vec<u8>, options: OpusOptions) -> Result<u64, Error> {
    let mut source = OggOpusSource::with_options(Cursor::new(bytes), options)?;
    let mut frames = 0;
    while let Some(block) = source.next_block()? {
        frames += block.len() as u64 / source.channels() as u64;
    }
    Ok(frames)
}

#[test]
fn applies_pre_skip_and_end_trimming() {
    let bytes = opus_stream(1, 50, 100).concat();
    assert_eq!(
        frames(bytes).unwrap(),
        50 * OPUS_PACKET_FRAMES - OPUS_PRE_SKIP as u64 - 100
    );
}

/// The end granule position can fall in a packet before the last one on the
/// end-of-stream page. audiowaveform also gives 7,788 frames on this file.
#[test]
fn end_trimming_reaches_earlier_packets_of_the_last_page() {
    let packets = opus_packets(10);
    let mut pages = opus_header_pages(1);
    for (sequence, packet) in (2..).zip(&packets[..8]) {
        let granule = (sequence as u64 - 1) * OPUS_PACKET_FRAMES;
        pages.push(ogg_packet_page(1, sequence, 0, granule, packet));
    }
    let end = 10 * OPUS_PACKET_FRAMES - 1_500;
    let last: Vec<&[u8]> = packets[8..].iter().map(Vec::as_slice).collect();
    pages.push(ogg_packets_page(1, 10, LAST, end, &last));

    assert_eq!(frames(pages.concat()).unwrap(), end - OPUS_PRE_SKIP as u64);
}

#[test]
fn cut_at_page_boundary_decodes_what_is_there() {
    let pages = opus_stream(1, 50, 100);
    let kept_packets = 20;
    let bytes = pages[..2 + kept_packets].concat();
    assert_eq!(
        frames(bytes).unwrap(),
        kept_packets as u64 * OPUS_PACKET_FRAMES - OPUS_PRE_SKIP as u64
    );
}

#[test]
fn cut_inside_page_is_rejected() {
    let pages = opus_stream(1, 50, 100);
    let mut bytes = pages[..22].concat();
    bytes.extend(&pages[22][..10]);
    assert!(matches!(frames(bytes), Err(Error::InvalidInput(_))));
}

#[test]
fn rejects_non_ogg_input() {
    for input in [&b""[..], b"RIFF\0\0\0\0WAVEfmt ", &[0u8; 64]] {
        let result = OggOpusSource::new(Cursor::new(input.to_vec()));
        assert!(matches!(result, Err(Error::InvalidInput(_))), "{input:?}");
    }
}

#[test]
fn rejects_truncated_first_page() {
    let page = ogg_packet_page(1, 0, FIRST, 0, &opus_head(1, OPUS_PRE_SKIP));
    let result = OggOpusSource::new(Cursor::new(page[..page.len() - 5].to_vec()));
    assert!(matches!(result, Err(Error::InvalidInput(_))));
}

#[test]
fn foreign_stream_pages_are_skipped_even_when_corrupt() {
    let pages = opus_stream(1, 50, 100);
    let mut corrupt = ogg_packet_page(2, 0, FIRST, 0, b"not opus");
    corrupt[22] ^= 0xFF;
    let interleaved: Vec<u8> = pages
        .iter()
        .flat_map(|page| [page.clone(), corrupt.clone()])
        .flatten()
        .collect();
    assert_eq!(
        frames(interleaved).unwrap(),
        frames(pages.concat()).unwrap()
    );
}

/// A packet spanning many pages must hit `max_packet_bytes` even when another
/// stream completes a packet between each of its pages.
#[test]
fn foreign_stream_cannot_bypass_max_packet_bytes() {
    let continuation = |sequence: u32| {
        let flags = if sequence == 2 { 0 } else { CONTINUED };
        ogg_page(1, sequence, flags, u64::MAX, &[255; 255], &[0; 255 * 255])
    };
    let mut bytes = [
        ogg_packet_page(1, 0, FIRST, 0, &opus_head(1, OPUS_PRE_SKIP)),
        ogg_packet_page(1, 1, 0, 0, &opus_tags()),
    ]
    .concat();
    for sequence in 2..12 {
        bytes.extend(continuation(sequence));
        bytes.extend(ogg_packet_page(sequence + 100, 0, FIRST, 0, b"x"));
    }

    let mut options = OpusOptions::default();
    options.max_packet_bytes = 200_000;
    let result = frames_with(bytes, options);
    assert!(matches!(result, Err(Error::LimitExceeded(_))), "{result:?}");
}
