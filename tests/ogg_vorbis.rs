//! Ogg Vorbis framing and trimming, on the packets of libvorbis fixtures laid
//! out on new pages.

#![cfg(feature = "vorbis")]

mod common;

use std::io::Cursor;

use common::{CONTINUED, FIRST, LAST, assert_same, ogg_packet_page, ogg_packets_page, ogg_page};
use waveform_peaks::{Error, OggVorbisSource, Overflow, PcmSource, VorbisOptions};

const CHORD: &[u8] = include_bytes!("data/chord-stereo-44100.ogg");
const SQUARE: &[u8] = include_bytes!("data/square-mono-48000.ogg");
const SERIAL: u32 = 7;

/// The packets of a fixture, and the frames each audio packet adds, counted
/// as libvorbis returns them: a quarter of the previous block and a quarter
/// of the current one.
struct Stream {
    headers: Vec<Vec<u8>>,
    audio: Vec<Vec<u8>>,
    frames: Vec<u64>,
}

impl Stream {
    fn new(file: &[u8]) -> Stream {
        let mut packets = packets(file);
        let audio = packets.split_off(3);
        let ident = lewton::header::read_header_ident(&packets[0]).unwrap();
        let setup = lewton::header::read_header_setup(
            &packets[2],
            ident.audio_channels,
            (ident.blocksize_0, ident.blocksize_1),
        )
        .unwrap();
        let short = 1u64 << ident.blocksize_0;
        let long = 1u64 << ident.blocksize_1;
        let blocks: Vec<u64> = audio
            .iter()
            .map(|packet| {
                let window = lewton::audio::get_decoded_sample_count(&ident, &setup, packet);
                if window.unwrap() as u64 == short / 2 {
                    short
                } else {
                    long
                }
            })
            .collect();
        let frames = (0..audio.len())
            .map(|i| match i {
                0 => 0,
                _ => blocks[i - 1] / 4 + blocks[i] / 4,
            })
            .collect();
        Stream {
            headers: packets,
            audio,
            frames,
        }
    }

    fn total(&self) -> u64 {
        self.frames.iter().sum()
    }

    /// Frames of the packets before `packet`.
    fn position(&self, packet: usize) -> u64 {
        self.frames[..packet].iter().sum()
    }

    fn header_pages(&self, serial: u32) -> Vec<Vec<u8>> {
        let rest: Vec<&[u8]> = self.headers[1..].iter().map(Vec::as_slice).collect();
        vec![
            ogg_packet_page(serial, 0, FIRST, 0, &self.headers[0]),
            ogg_packets_page(serial, 1, 0, 0, &rest),
        ]
    }

    /// Audio pages holding `layout[i]` packets each, with granule positions
    /// from `granule(page, frames up to the end of the page)`.
    fn pages(
        &self,
        serial: u32,
        layout: &[usize],
        end_of_stream: bool,
        granule: impl Fn(usize, u64) -> i64,
    ) -> Vec<Vec<u8>> {
        assert_eq!(layout.iter().sum::<usize>(), self.audio.len());
        let mut pages = self.header_pages(serial);
        let mut next = 0;
        for (page, &count) in layout.iter().enumerate() {
            let packets: Vec<&[u8]> = self.audio[next..next + count]
                .iter()
                .map(Vec::as_slice)
                .collect();
            next += count;
            let last = page == layout.len() - 1;
            let flags = if last && end_of_stream { LAST } else { 0 };
            let granule = granule(page, self.position(next)) as u64;
            pages.push(ogg_packets_page(
                serial,
                page as u32 + 2,
                flags,
                granule,
                &packets,
            ));
        }
        pages
    }

    /// Consistent granule positions, the last one ending at `end`.
    fn file(&self, layout: &[usize], end: u64) -> Vec<u8> {
        self.pages(SERIAL, layout, true, |page, position| {
            if page == layout.len() - 1 {
                end as i64
            } else {
                position as i64
            }
        })
        .concat()
    }

    /// Every packet decoded, with no trimming.
    fn untrimmed(&self) -> Vec<i16> {
        decode(self.file(&self.layout(10), self.total())).unwrap()
    }

    /// Pages of `per_page` packets.
    fn layout(&self, per_page: usize) -> Vec<usize> {
        Stream::layout_of(self.audio.len(), per_page)
    }

    fn layout_of(count: usize, per_page: usize) -> Vec<usize> {
        let mut layout = vec![per_page; count / per_page];
        if !count.is_multiple_of(per_page) {
            layout.push(count % per_page);
        }
        layout
    }
}

/// The packets of an Ogg file with one logical stream.
fn packets(file: &[u8]) -> Vec<Vec<u8>> {
    let mut packets = Vec::new();
    let mut packet = Vec::new();
    let mut rest = file;
    while !rest.is_empty() {
        let segments = rest[26] as usize;
        let lacing = &rest[27..27 + segments];
        let mut body = &rest[27 + segments..];
        for &len in lacing {
            packet.extend_from_slice(&body[..len as usize]);
            body = &body[len as usize..];
            if len < 255 {
                packets.push(std::mem::take(&mut packet));
            }
        }
        rest = body;
    }
    packets
}

fn decode(file: Vec<u8>) -> Result<Vec<i16>, Error> {
    decode_with(file, VorbisOptions::default())
}

fn decode_with(file: Vec<u8>, options: VorbisOptions) -> Result<Vec<i16>, Error> {
    common::samples(OggVorbisSource::with_options(Cursor::new(file), options)?)
}

fn chord() -> Stream {
    Stream::new(CHORD)
}

#[test]
fn decodes_a_libvorbis_file_up_to_its_last_granule_position() {
    let source = OggVorbisSource::new(CHORD).unwrap();
    assert_eq!((source.sample_rate(), source.channels()), (44_100, 2));
    let samples = common::samples(source).unwrap();
    assert_eq!(samples.len(), 2 * 44_100);

    let stream = chord();
    assert!(stream.total() > 44_100);
    assert_same(&samples, &stream.untrimmed()[..samples.len()]);
}

#[test]
fn a_last_granule_position_past_the_audio_adds_nothing() {
    let stream = chord();
    let layout = stream.layout(10);
    let samples = decode(stream.file(&layout, stream.total() + 5_000)).unwrap();
    assert_same(&samples, &stream.untrimmed());
}

#[test]
fn the_last_page_trims_without_an_end_of_stream_flag() {
    let stream = chord();
    let layout = stream.layout(10);
    let end = stream.total() - 100;
    let file = stream
        .pages(SERIAL, &layout, false, |page, position| {
            if page == layout.len() - 1 {
                end as i64
            } else {
                position as i64
            }
        })
        .concat();
    let samples = decode(file).unwrap();
    assert_same(&samples, &stream.untrimmed()[..2 * end as usize]);
}

#[test]
fn a_stream_cut_after_a_page_ends_at_that_page() {
    let stream = chord();
    let layout = stream.layout(10);
    let mut pages = stream.pages(SERIAL, &layout, true, |_, position| position as i64);
    pages.pop();
    let end = stream.position(layout[..layout.len() - 1].iter().sum());
    let expected = &stream.untrimmed()[..2 * end as usize];
    assert_same(&decode(pages.concat()).unwrap(), expected);

    // A page cut short is dropped.
    let mut cut = pages.concat();
    cut.extend_from_slice(&stream.file(&layout, stream.total())[cut.len()..][..100]);
    assert_same(&decode(cut).unwrap(), expected);
}

#[test]
fn a_single_page_stream_loses_frames_at_its_end() {
    let stream = chord();
    let end = stream.total() - 300;
    let samples = decode(stream.file(&[stream.audio.len()], end)).unwrap();
    assert_same(&samples, &stream.untrimmed()[..2 * end as usize]);
}

/// A stream that starts later than its first page says keeps all its frames,
/// as captures of live streams do.
#[test]
fn a_positive_start_offset_keeps_all_frames() {
    let stream = chord();
    let layout = stream.layout(10);
    let end = stream.total() - 100;
    let file = stream
        .pages(SERIAL, &layout, true, |page, position| {
            let position = if page == layout.len() - 1 {
                end
            } else {
                position
            };
            position as i64 + 1_000_000
        })
        .concat();
    assert_same(
        &decode(file).unwrap(),
        &stream.untrimmed()[..2 * end as usize],
    );
}

/// FFmpeg writes a negative start offset when it cuts a file without
/// re-encoding. audiowaveform then drops frames at the start of the last
/// packet of the first page, at most that packet, and ends the output at the
/// last granule position.
#[test]
fn a_negative_start_offset_cuts_into_the_first_page_and_ends_at_the_last_granule() {
    let stream = chord();
    // A last page of two short packets, too short to absorb the offset.
    let mut layout = stream.layout(10);
    let last = layout.pop().unwrap() + layout.pop().unwrap() - 2;
    layout.extend([last, 2]);
    let last_page_start = stream.position(stream.audio.len() - 2) as usize;
    let last_packet = layout[0] - 1;
    let packet_frames = stream.frames[last_packet];
    let packet_start = stream.position(last_packet) as usize;
    let untrimmed = stream.untrimmed();
    for offset in [packet_frames / 2, packet_frames, packet_frames + 3_000] {
        let end = stream.total() - 100 - offset;
        let file = stream
            .pages(SERIAL, &layout, true, |page, position| {
                if page == layout.len() - 1 {
                    end as i64
                } else {
                    position as i64 - offset as i64
                }
            })
            .concat();
        let cut = offset.min(packet_frames) as usize;
        let mut expected = untrimmed[..2 * packet_start].to_vec();
        expected.extend(&untrimmed[2 * (packet_start + cut)..]);
        expected.truncate(2 * end as usize);
        assert_same(&decode(file.clone()).unwrap(), &expected);

        // Without holding frames back, the output ends at most at the last
        // page.
        let mut options = VorbisOptions::default();
        options.max_held_frames = 0;
        let mut expected = untrimmed[..2 * packet_start].to_vec();
        expected.extend(&untrimmed[2 * (packet_start + cut)..]);
        expected.truncate(2 * (end as usize).max(last_page_start - cut));
        assert_same(&decode_with(file, options).unwrap(), &expected);
    }
}

/// A negative first granule position trims at most the last packet of the
/// first page, and the output keeps the frames up to the end trimming.
#[test]
fn a_negative_first_granule_position_trims_one_packet() {
    let stream = chord();
    let mut layout = vec![2];
    layout.extend(Stream::layout_of(stream.audio.len() - 2, 10));
    let offset = 5_000;
    let end = stream.total() - 100 - offset;
    let file = stream
        .pages(SERIAL, &layout, true, |page, position| {
            if page == layout.len() - 1 {
                end as i64
            } else {
                position as i64 - offset as i64
            }
        })
        .concat();
    let untrimmed = stream.untrimmed();
    let mut expected = untrimmed[..2 * stream.position(1) as usize].to_vec();
    expected.extend(&untrimmed[2 * stream.position(2) as usize..]);
    expected.truncate(expected.len() - 2 * 100);
    assert_same(&decode(file).unwrap(), &expected);
}

#[test]
fn a_page_without_granule_position_counts_with_the_next() {
    let stream = chord();
    let layout = stream.layout(10);
    let end = stream.total() - 100;
    let file = stream
        .pages(SERIAL, &layout, true, |page, position| match page {
            0 => -1,
            _ if page == layout.len() - 1 => end as i64,
            _ => position as i64,
        })
        .concat();
    assert_same(
        &decode(file).unwrap(),
        &stream.untrimmed()[..2 * end as usize],
    );
}

/// audiowaveform fails on such a stream.
#[test]
fn a_stream_without_granule_position_is_decoded_to_its_end() {
    let stream = chord();
    let file = stream
        .pages(SERIAL, &stream.layout(10), true, |_, _| -1)
        .concat();
    assert_same(&decode(file).unwrap(), &stream.untrimmed());
}

#[test]
fn decodes_only_the_stream_of_the_first_page() {
    let stream = chord();
    let layout = stream.layout(10);
    let end = stream.total() - 100;
    let granule = |page: usize, position: u64| {
        if page == layout.len() - 1 {
            end as i64
        } else {
            position as i64
        }
    };
    let ours = stream.pages(SERIAL, &layout, true, granule);
    let other = stream.pages(SERIAL + 1, &layout, true, granule);
    let mut file = Vec::new();
    for (i, page) in ours.iter().enumerate() {
        file.extend(page);
        file.extend(&other[i]);
    }
    // A chained link, with the same serial number as the first.
    file.extend(stream.file(&layout, stream.total()));
    assert_same(
        &decode(file).unwrap(),
        &stream.untrimmed()[..2 * end as usize],
    );
}

#[test]
fn skips_bytes_that_are_not_part_of_a_valid_page() {
    let stream = chord();
    let layout = stream.layout(10);
    let pages = stream.pages(SERIAL, &layout, true, |_, position| position as i64);
    let untrimmed = stream.untrimmed();

    let mut file = Vec::new();
    for page in &pages {
        file.extend(b"junk between pages");
        file.extend(false_header(1, 100));
        file.extend(page);
    }
    assert_same(&decode(file).unwrap(), &untrimmed);

    // A page with a wrong checksum is lost, and so are the frames of the
    // next packet, which only primes the decoder again.
    let lost = 3;
    let mut file = Vec::new();
    for (i, page) in pages.iter().enumerate() {
        let mut page = page.clone();
        if i == lost {
            page[22] ^= 1;
        }
        file.extend(page);
    }
    let first_lost = layout[..lost - 2].iter().sum::<usize>();
    let next = first_lost + layout[lost - 2];
    let mut expected = untrimmed[..2 * stream.position(first_lost) as usize].to_vec();
    expected.extend(&untrimmed[2 * stream.position(next + 1) as usize..]);
    assert_same(&decode(file).unwrap(), &expected);
}

/// The start of a page that claims `segments` lacing values of `lacing`
/// bytes each, with no checksum.
fn false_header(segments: u8, lacing: u8) -> Vec<u8> {
    let mut header = b"OggS".to_vec();
    header.resize(26, 0);
    header.push(segments);
    header.extend(vec![lacing; segments as usize]);
    header
}

/// libogg reads the length of a false page header before it checks the page,
/// so a length past the end of the input ends the stream.
#[test]
fn a_false_page_header_that_runs_past_the_input_ends_the_stream() {
    let stream = chord();
    let layout = stream.layout(10);
    let pages = stream.pages(SERIAL, &layout, true, |_, position| position as i64);
    let last = pages.len() - 1;
    let file = [
        pages[..last].concat(),
        false_header(255, 255),
        pages[last].clone(),
    ]
    .concat();
    let end = stream.position(layout[..layout.len() - 1].iter().sum());
    assert_same(
        &decode(file).unwrap(),
        &stream.untrimmed()[..2 * end as usize],
    );
}

/// libogg refuses a page of another version, and libsndfile stops there.
#[test]
fn a_page_of_another_version_ends_the_stream() {
    let stream = chord();
    let layout = stream.layout(10);
    let pages = stream.pages(SERIAL, &layout, true, |_, position| position as i64);
    let with_version = |serial: u32| {
        let mut file = pages[..4].concat();
        let mut page = pages[3].clone();
        page[4] = 1;
        page[14..18].copy_from_slice(&serial.to_le_bytes());
        common::fix_ogg_checksum(&mut page);
        file.extend(page);
        file.extend(pages[4..].concat());
        file
    };
    let end = stream.position(layout[..2].iter().sum());
    assert_same(
        &decode(with_version(SERIAL)).unwrap(),
        &stream.untrimmed()[..2 * end as usize],
    );
    // Pages of other streams are skipped before their version is read.
    assert_same(
        &decode(with_version(SERIAL + 1)).unwrap(),
        &stream.untrimmed(),
    );
}

#[test]
fn a_packet_continued_on_a_lost_page_is_dropped() {
    let stream = chord();
    let split = (10..).find(|&i| stream.audio[i].len() > 255).unwrap();
    let mut pages = stream.header_pages(SERIAL);
    let before: Vec<&[u8]> = stream.audio[..split].iter().map(Vec::as_slice).collect();
    pages.push(ogg_packets_page(
        SERIAL,
        2,
        0,
        stream.position(split),
        &before,
    ));
    let (head, tail) = stream.audio[split].split_at(255);
    pages.push(ogg_page(SERIAL, 3, 0, u64::MAX, &[255], head));
    let mut lacing = vec![255; tail.len() / 255];
    lacing.push((tail.len() % 255) as u8);
    let mut body = tail.to_vec();
    for packet in &stream.audio[split + 1..] {
        lacing.extend(vec![255; packet.len() / 255]);
        lacing.push((packet.len() % 255) as u8);
        body.extend(packet);
    }
    pages.push(ogg_page(
        SERIAL,
        4,
        CONTINUED | LAST,
        stream.total(),
        &lacing,
        &body,
    ));
    let untrimmed = stream.untrimmed();
    assert_same(&decode(pages.concat()).unwrap(), &untrimmed);

    pages.remove(3);
    let mut expected = untrimmed[..2 * stream.position(split) as usize].to_vec();
    expected.extend(&untrimmed[2 * stream.position(split + 2) as usize..]);
    assert_same(&decode(pages.concat()).unwrap(), &expected);
}

/// `stream` with `packet` inserted before audio packet `index`, as a packet
/// that adds no frames.
fn with_packet(mut stream: Stream, index: usize, packet: &[u8]) -> Stream {
    stream.audio.insert(index, packet.to_vec());
    stream.frames.insert(index, 0);
    stream
}

/// lewton refuses a short block after a long block whose window flags
/// announce a long one; libvorbis decodes it.
#[test]
fn a_packet_that_lewton_refuses_becomes_silence_of_its_length() {
    let stream = chord();
    let index = (2..stream.audio.len())
        .find(|&i| {
            stream.frames[i - 1..=i + 1]
                .iter()
                .all(|&frames| frames == 1024)
        })
        .unwrap();
    let (short, long) = (256, 2048);
    let mut damaged = with_packet(chord(), index, &[0x00, 0xFF, 0xFF, 0xFF]);
    damaged.frames[index] = long / 4 + short / 4;
    damaged.frames[index + 1] = short / 4 + long / 4;
    let file = damaged.file(&damaged.layout(10), damaged.total());

    let untrimmed = stream.untrimmed();
    let mut expected = untrimmed[..2 * stream.position(index) as usize].to_vec();
    expected.extend(vec![0; 2 * (long / 2 + short / 2) as usize]);
    expected.extend(&untrimmed[2 * stream.position(index + 1) as usize..]);
    assert_same(&decode(file).unwrap(), &expected);
}

/// libvorbis skips an empty packet, and then counts as if the stream
/// started after it: the first granule position no longer trims.
#[test]
fn a_packet_that_libvorbis_rejects_restarts_the_count() {
    let stream = chord();
    let layout = stream.layout(10);
    let low = |stream: &Stream, layout: &[usize]| {
        stream
            .pages(SERIAL, layout, true, |page, position| match page {
                0 => position as i64 - 700,
                _ if page == layout.len() - 1 => stream.total() as i64,
                _ => position as i64,
            })
            .concat()
    };
    let untrimmed = stream.untrimmed();
    assert!(decode(low(&stream, &layout)).unwrap().len() < untrimmed.len());

    let mut damaged_layout = layout.clone();
    damaged_layout[0] += 1;
    for packet in [&[][..], &[0x01]] {
        let damaged = with_packet(chord(), 5, packet);
        assert_same(&decode(low(&damaged, &damaged_layout)).unwrap(), &untrimmed);
    }
}

/// libsndfile starts at the page after the setup header.
#[test]
fn audio_packets_on_the_page_of_the_setup_header_are_dropped() {
    let stream = chord();
    let dropped = 5;
    let mut rest = chord();
    rest.audio.drain(..dropped);
    rest.frames.drain(..dropped);
    rest.frames[0] = 0;

    let mut setup_page: Vec<&[u8]> = stream.headers[1..].iter().map(Vec::as_slice).collect();
    setup_page.extend(stream.audio[..dropped].iter().map(Vec::as_slice));
    let mut file = ogg_packet_page(SERIAL, 0, FIRST, 0, &stream.headers[0]);
    file.extend(ogg_packets_page(SERIAL, 1, 0, 0, &setup_page));
    let layout = Stream::layout_of(rest.audio.len(), 10);
    let mut next = dropped;
    for (page, &count) in layout.iter().enumerate() {
        let packets: Vec<&[u8]> = stream.audio[next..next + count]
            .iter()
            .map(Vec::as_slice)
            .collect();
        next += count;
        let last = page == layout.len() - 1;
        file.extend(ogg_packets_page(
            SERIAL,
            page as u32 + 2,
            if last { LAST } else { 0 },
            stream.position(next),
            &packets,
        ));
    }
    // The start offset is the first granule position less the frames that
    // libsndfile counts on its page, which start at packet `dropped`.
    let end = stream.total() - stream.position(dropped + 1);
    assert_same(
        &decode(file).unwrap(),
        &rest.untrimmed()[..2 * end as usize],
    );
}

#[test]
fn overflow_wraps_or_saturates() {
    let saturated = common::samples(OggVorbisSource::new(SQUARE).unwrap()).unwrap();
    let mut options = VorbisOptions::default();
    options.overflow = Overflow::Wrap;
    let wrapped = common::samples(OggVorbisSource::with_options(SQUARE, options).unwrap()).unwrap();
    assert_eq!(saturated.len(), wrapped.len());
    let mut differences = 0;
    for (&saturated, &wrapped) in saturated.iter().zip(&wrapped) {
        if saturated != wrapped {
            assert!(saturated == i16::MAX || saturated == i16::MIN);
            assert_ne!(saturated.signum(), wrapped.signum());
            differences += 1;
        }
    }
    assert!(differences > 0);
}

/// lewton overflows in a floor of this fuzzed file. With overflow checks,
/// it panics, and the source skips the packet.
#[test]
fn survives_a_panic_in_lewton() {
    let file = include_bytes!("data/lewton-overflow.ogg");
    let source = OggVorbisSource::new(&file[..]).unwrap();
    common::samples(source).unwrap();
}

#[test]
fn rejects_what_is_not_ogg_vorbis() {
    let opus_head = ogg_packet_page(1, 0, FIRST, 0, &common::opus_head(2, 312));
    let skeleton = ogg_packet_page(1, 0, FIRST, 0, b"fishead\0");
    for file in [Vec::new(), b"not an Ogg file".to_vec(), opus_head, skeleton] {
        assert!(matches!(decode(file), Err(Error::InvalidInput(_))));
    }
}

#[test]
fn rejects_missing_or_damaged_headers() {
    let stream = chord();
    let pages = stream.header_pages(SERIAL);
    assert!(matches!(
        decode(pages[0].clone()),
        Err(Error::InvalidInput(_))
    ));

    let mut ident = stream.headers[0].clone();
    ident[7] = 1;
    let mut file = ogg_packet_page(SERIAL, 0, FIRST, 0, &ident);
    file.extend(&pages[1]);
    assert!(matches!(decode(file), Err(Error::Unsupported(_))));

    // A gap between the header pages.
    let mut file = pages[0].clone();
    file.extend(&ogg_packets_page(
        SERIAL,
        5,
        0,
        0,
        &[&stream.headers[1], &stream.headers[2]],
    ));
    assert!(matches!(decode(file), Err(Error::InvalidInput(_))));
}

#[test]
fn bounds_packets_by_max_packet_bytes() {
    let mut options = VorbisOptions::default();
    options.max_packet_bytes = 1_000;
    assert!(matches!(
        decode_with(CHORD.to_vec(), options),
        Err(Error::LimitExceeded(_))
    ));

    // Empty packets on pages without granule position add up.
    let stream = chord();
    let mut file = stream.header_pages(SERIAL).concat();
    let empty: Vec<&[u8]> = vec![&[]; 255];
    for sequence in 2..100 {
        file.extend(ogg_packets_page(SERIAL, sequence, 0, u64::MAX, &empty));
    }
    options.max_packet_bytes = 100_000;
    assert!(matches!(
        decode_with(file, options),
        Err(Error::LimitExceeded(_))
    ));
}

/// The bits of a setup header with one codebook, least significant first.
fn setup_header(fields: &[(u32, u32)]) -> Vec<u8> {
    let mut bytes = b"\x05vorbis".to_vec();
    let mut position = 0;
    for &(value, bits) in [(0, 8)].iter().chain(fields) {
        for i in 0..bits {
            if position % 8 == 0 {
                bytes.push(0);
            }
            *bytes.last_mut().unwrap() |= ((value >> i & 1) as u8) << (position % 8);
            position += 1;
        }
    }
    bytes.extend([0; 16]);
    bytes
}

fn with_setup(setup: Vec<u8>) -> Vec<u8> {
    let stream = chord();
    let mut file = stream.header_pages(SERIAL)[0].clone();
    file.extend(ogg_packets_page(
        SERIAL,
        1,
        0,
        0,
        &[&stream.headers[1], &setup],
    ));
    file
}

const SYNC: (u32, u32) = (0x56_4342, 24);

#[test]
fn bounds_codebooks_by_max_codebook_values() {
    let mut options = VorbisOptions::default();
    options.max_codebook_values = 1_000;
    assert!(matches!(
        decode_with(CHORD.to_vec(), options),
        Err(Error::LimitExceeded(_))
    ));

    // 2^24 - 1 entries of 65,535 dimensions, ordered, in a few bytes.
    let setup = setup_header(&[SYNC, (65_535, 16), ((1 << 24) - 1, 24), (1, 1)]);
    assert!(matches!(
        decode(with_setup(setup)),
        Err(Error::LimitExceeded(_))
    ));
}

#[test]
fn rejects_codebooks_that_libvorbis_rejects() {
    // Ordered codeword lengths that pass 32 bits.
    let mut fields = vec![SYNC, (1, 16), (100, 24), (1, 1), (31, 5)];
    fields.extend([(0, 7); 3]);
    assert!(matches!(
        decode(with_setup(setup_header(&fields))),
        Err(Error::InvalidInput(_))
    ));

    // A lookup of type 1 with no dimensions.
    let fields = [SYNC, (0, 16), (1, 24), (0, 1), (0, 1), (0, 5), (1, 4)];
    assert!(matches!(
        decode(with_setup(setup_header(&fields))),
        Err(Error::InvalidInput(_))
    ));
}
