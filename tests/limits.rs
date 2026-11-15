//! `Limits::max_duration` on crafted files whose audio is decoded and then
//! dropped: the limit stops them. The audio that an encoder adds and a real
//! file trims does not take from the limit of the output.

#![allow(dead_code, unused_imports)]

mod common;

use std::io::Cursor;
use std::time::Duration;

use common::*;
use waveform_peaks::{Error, Limits, MemorySink, Options, PcmSource, generate};

/// Runs `source` with a limit of 100 ms and checks that the limit stops it.
#[track_caller]
fn assert_stopped<P: PcmSource>(source: Result<P, Error>) {
    let mut limits = Limits::default();
    limits.max_duration = Duration::from_millis(100);
    let result = generate(
        &mut source.unwrap(),
        &Options::default(),
        &limits,
        MemorySink::new(),
    );
    assert!(
        matches!(
            result,
            Err(Error::LimitExceeded("dropped audio above max_duration"))
        ),
        "{result:?}"
    );
}

/// Runs the source that `open` gives with a limit of its exact duration.
#[track_caller]
fn assert_fits<P: PcmSource>(open: impl Fn() -> Result<P, Error>) {
    let mut source = open().unwrap();
    let (rate, channels) = (source.sample_rate() as u64, source.channels());
    let mut frames = 0;
    while let Some(block) = source.next_block().unwrap() {
        frames += (block.len() / channels as usize) as u64;
    }
    assert!(source.dropped_frames() > 0, "a file with trimmed audio");
    let mut limits = Limits::default();
    // One more nanosecond, so that rounding does not give a frame less.
    limits.max_duration = Duration::from_nanos((frames * 1_000_000_000).div_ceil(rate) + 1);
    generate(
        &mut open().unwrap(),
        &Options::default(),
        &limits,
        MemorySink::new(),
    )
    .unwrap();
}

/// An edit list that starts far beyond the audio.
const LATE_EDIT: Option<(u64, i64)> = Some((0, i32::MAX as i64));

#[cfg(feature = "aac")]
mod aac {
    use super::*;
    use waveform_peaks::Mp4Source;

    const MONO_AAC: &[u8] = include_bytes!("data/sine-mono-44100.aac");

    fn mp4(edit: Option<(u64, i64)>, extra: &[u8]) -> Vec<u8> {
        let (config, frames) = adts_frames(MONO_AAC);
        let mut track = Mp4Track::audio(&config, 1, 44_100, &frames, 50);
        track.edit = edit;
        faststart_mp4(&[track], 44_100, extra, &[])
    }

    #[test]
    fn audio_before_an_mp4_edit_counts() {
        assert_stopped(Mp4Source::new(Cursor::new(mp4(LATE_EDIT, &[]))));
    }

    #[test]
    fn audio_in_the_itunes_priming_counts() {
        let priming = itunsmpb_udta(u32::MAX, 0, 0, true);
        assert_stopped(Mp4Source::new(Cursor::new(mp4(None, &priming))));
    }

    #[test]
    fn a_file_as_long_as_the_limit_fits() {
        assert_fits(|| Mp4Source::new(Cursor::new(include_bytes!("data/safari-aac-stereo.mp4"))));
        assert_fits(|| Mp4Source::new(Cursor::new(include_bytes!("data/he-aac-apple.m4a"))));
    }
}

#[cfg(feature = "mp3")]
mod mp3 {
    use super::*;
    use waveform_peaks::{Mp4Source, MpegAudioSource};

    const MP3: &[u8] = include_bytes!("data/sine-mono-44100.mp3");

    /// The audio frames, after the frame of the Info tag.
    fn frames() -> Vec<Vec<u8>> {
        mpa_frames(MP3)[1..].to_vec()
    }

    #[test]
    fn audio_before_an_mp4_edit_counts() {
        let mut track = Mp4Track {
            sample_entry: audio_sample_entry(b"mp4a", 1, 44_100, &esds(0x6B, &[])),
            ..Mp4Track::audio(&[], 1, 44_100, &frames(), 10)
        };
        track.edit = LATE_EDIT;
        let file = faststart_mp4(&[track], 44_100, &[], &[]);
        assert_stopped(Mp4Source::new(Cursor::new(file)));
    }

    #[test]
    fn frames_decoded_for_the_reservoir_count() {
        // A skipped byte empties the reservoir, and each frame then claims
        // main data from 511 bytes before it, which two frames do not hold.
        let mut file = Vec::new();
        for pair in frames().windows(2).step_by(2).take(20) {
            file.push(0);
            for frame in pair {
                assert_eq!(frame[1] & 1, 1, "a frame without CRC");
                let mut frame = frame.clone();
                frame[4] = 0xFF;
                frame[5] |= 0x80;
                file.extend(frame);
            }
        }
        assert_stopped(MpegAudioSource::new(Cursor::new(file)));
    }

    #[test]
    fn frames_that_do_not_decode_count() {
        let mut file = Vec::new();
        for mut frame in frames().into_iter().take(20) {
            // No main data from earlier frames, so the reservoir is not why.
            frame[4] = 0;
            frame[5] &= 0x7F;
            set_big_values(&mut frame, 511);
            file.extend(frame);
        }
        assert_stopped(MpegAudioSource::new(Cursor::new(file)));
    }

    #[test]
    fn a_file_as_long_as_the_limit_fits() {
        assert_fits(|| MpegAudioSource::new(Cursor::new(MP3)));
    }
}

#[cfg(feature = "vorbis")]
mod vorbis {
    use super::*;
    use waveform_peaks::OggVorbisSource;

    const CHORD: &[u8] = include_bytes!("data/chord-stereo-44100.ogg");

    /// The header pages, then one audio packet per page. `step` is the page
    /// sequence increment, and `granule` gives the granule position of an
    /// audio page from whether it is the last.
    fn file(step: u32, granule: impl Fn(bool) -> i64) -> Vec<u8> {
        let packets = ogg_file_packets(CHORD);
        let (headers, audio) = packets.split_at(3);
        let mut file = ogg_packet_page(7, 0, FIRST, 0, &headers[0]);
        file.extend(ogg_packets_page(7, 1, 0, 0, &[&headers[1], &headers[2]]));
        let mut sequence = 2;
        for (index, packet) in audio.iter().enumerate() {
            let last = index + 1 == audio.len();
            let flags = if last { LAST } else { 0 };
            let position = granule(last) as u64;
            file.extend(ogg_packet_page(7, sequence, flags, position, packet));
            sequence += step;
        }
        file
    }

    #[test]
    fn audio_beyond_the_last_granule_position_counts() {
        let file = file(1, |last| if last { 0 } else { -1 });
        assert_stopped(OggVorbisSource::new(Cursor::new(file)));
    }

    #[test]
    fn packets_after_page_gaps_count() {
        // The first packet after a gap gives no frames.
        assert_stopped(OggVorbisSource::new(Cursor::new(file(2, |_| 0))));
    }

    #[test]
    fn a_file_as_long_as_the_limit_fits() {
        assert_fits(|| OggVorbisSource::new(Cursor::new(CHORD)));
        assert_fits(|| {
            OggVorbisSource::new(Cursor::new(include_bytes!("data/square-mono-48000.ogg")))
        });
    }
}

#[cfg(feature = "opus")]
mod opus {
    use super::*;
    use waveform_peaks::{Mp4Source, OggOpusSource, WebmOpusSource};

    /// 20 ms each.
    const PACKETS: u64 = 50;

    fn webm(delay: u64, blocks: &[Vec<u8>]) -> Vec<u8> {
        let tracks = element(TRACKS, &opus_track(1, OPUS_PRE_SKIP, Some(delay)));
        let cluster = element(
            CLUSTER,
            &[uint_element(TIMESTAMP, 0), blocks.concat()].concat(),
        );
        [
            ebml_header("webm"),
            element(SEGMENT, &[tracks, cluster].concat()),
        ]
        .concat()
    }

    #[test]
    fn audio_in_the_webm_codec_delay_counts() {
        let blocks: Vec<Vec<u8>> = opus_packets(PACKETS)
            .iter()
            .map(|packet| element(SIMPLE_BLOCK, &block_body(1, 0, &[packet])))
            .collect();
        assert_stopped(WebmOpusSource::new(Cursor::new(webm(u64::MAX, &blocks))));
    }

    #[test]
    fn audio_in_the_webm_discard_padding_counts() {
        let blocks: Vec<Vec<u8>> = opus_packets(PACKETS)
            .iter()
            .map(|packet| {
                let block = element(BLOCK, &block_body(1, 0, &[packet]));
                let padding = uint_element(DISCARD_PADDING, i64::MAX as u64);
                element(BLOCK_GROUP, &[block, padding].concat())
            })
            .collect();
        let delay = ns_for_48k_frames(OPUS_PRE_SKIP as u64);
        assert_stopped(WebmOpusSource::new(Cursor::new(webm(delay, &blocks))));
    }

    #[test]
    fn audio_after_an_empty_last_ogg_page_counts() {
        // The page ends the audio at position 0, and the packets follow it.
        let mut pages = opus_header_pages(9);
        pages.push(ogg_page(9, 2, LAST, 0, &[], &[]));
        for (index, packet) in (0u64..).zip(opus_packets(PACKETS)) {
            let granule = (index + 1) * OPUS_PACKET_FRAMES;
            pages.push(ogg_packet_page(9, 3 + index as u32, 0, granule, &packet));
        }
        assert_stopped(OggOpusSource::new(Cursor::new(pages.concat())));
    }

    #[test]
    fn audio_before_an_mp4_edit_counts() {
        let packets = opus_packets(PACKETS);
        let mut track = Mp4Track::opus(&dops(1, OPUS_PRE_SKIP, 48_000, 0), &packets, 50);
        track.edit = LATE_EDIT;
        let file = faststart_mp4(&[track], 48_000, &[], &[]);
        assert_stopped(Mp4Source::new(Cursor::new(file)));
    }

    #[test]
    fn a_file_as_long_as_the_limit_fits() {
        assert_fits(|| Mp4Source::new(Cursor::new(include_bytes!("data/chrome-opus-stereo.mp4"))));
        assert_fits(|| Mp4Source::new(Cursor::new(include_bytes!("data/safari-opus-stereo.mp4"))));
    }
}
