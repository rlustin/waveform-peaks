#![allow(dead_code)]

use std::{ffi::OsStr, path::PathBuf, process::Command};

use waveform_peaks::{Bits, ChannelMode, Options};

/// Deterministic PCM: xorshift noise at varying levels, with full-scale and
/// zero runs mixed in so truncation and extremes get exercised.
pub fn pcm(frames: usize, channels: usize, seed: u64) -> Vec<i16> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    (0..frames * channels)
        .map(|i| {
            let r = next();
            match (i / 97) % 7 {
                0 => i16::MIN,
                1 => i16::MAX,
                2 => 0,
                3 => (r as i16) >> 8,
                _ => r as i16,
            }
        })
        .collect()
}

pub fn options(samples_per_pixel: u32, bits: Bits, channel_mode: ChannelMode) -> Options {
    let mut options = Options::default();
    options.samples_per_pixel = samples_per_pixel;
    options.bits = bits;
    options.channel_mode = channel_mode;
    options
}

pub fn le_bytes(samples: &[i16]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_le_bytes()).collect()
}

/// Header fields and values of a `.dat` buffer.
#[derive(Debug, PartialEq, Eq)]
pub struct Dat {
    pub header: Vec<u8>,
    pub values: Vec<i32>,
}

pub fn parse_dat(bytes: &[u8]) -> Dat {
    let version = i32::from_le_bytes(bytes[0..4].try_into().unwrap());
    let offset = if version == 2 { 24 } else { 20 };
    let eight_bit = bytes[4] & 1 == 1;
    let body = &bytes[offset..];
    let values = if eight_bit {
        body.iter().map(|&b| b as i8 as i32).collect()
    } else {
        body.as_chunks::<2>()
            .0
            .iter()
            .map(|&pair| i16::from_le_bytes(pair) as i32)
            .collect()
    };
    Dat {
        header: bytes[..offset].to_vec(),
        values,
    }
}

/// Asserts two `.dat` buffers have identical headers and values within
/// `tolerance`. With a zero tolerance the bytes must be identical.
pub fn assert_dat_matches(ours: &[u8], reference: &[u8], tolerance: i32, context: &str) {
    let (a, b) = (parse_dat(ours), parse_dat(reference));
    assert_eq!(a.header, b.header, "{context}: headers differ");
    assert_eq!(
        a.values.len(),
        b.values.len(),
        "{context}: value counts differ"
    );
    let diffs: Vec<(usize, i32, i32)> = a
        .values
        .iter()
        .zip(&b.values)
        .enumerate()
        .filter(|(_, (x, y))| x != y)
        .map(|(i, (&x, &y))| (i, x, y))
        .collect();
    let worst = diffs
        .iter()
        .map(|(_, x, y)| (x - y).abs())
        .max()
        .unwrap_or(0);
    assert!(
        worst <= tolerance,
        "{context}: {} of {} values differ, max |diff| {worst} (tolerance {tolerance}); first (index, ours, reference): {:?}",
        diffs.len(),
        a.values.len(),
        &diffs[..diffs.len().min(8)]
    );
    if tolerance == 0 {
        assert_eq!(ours, reference, "{context}: bytes differ");
    }
}

/// Path of an external tool, overridable with an environment variable.
pub fn tool(env: &str, default: &str) -> PathBuf {
    std::env::var_os(env).map_or_else(|| PathBuf::from(default), PathBuf::from)
}

/// Runs a command and panics with its stderr on failure.
pub fn run<I, S>(program: &PathBuf, args: I)
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = Command::new(program);
    command.args(args);
    let output = command
        .output()
        .unwrap_or_else(|err| panic!("cannot run {}: {err}", program.display()));
    assert!(
        output.status.success(),
        "{command:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Rewrites fields of the `OpusHead` packet on the first Ogg page and fixes the
/// page checksum.
pub fn patch_opus_head(file: &[u8], input_rate: Option<u32>, output_gain: Option<i16>) -> Vec<u8> {
    let mut bytes = file.to_vec();
    assert_eq!(&bytes[..4], b"OggS");
    let segments = bytes[26] as usize;
    let page_len = 27
        + segments
        + bytes[27..27 + segments]
            .iter()
            .map(|&b| b as usize)
            .sum::<usize>();
    let head = 27 + segments;
    assert_eq!(&bytes[head..head + 8], b"OpusHead");
    if let Some(rate) = input_rate {
        bytes[head + 12..head + 16].copy_from_slice(&rate.to_le_bytes());
    }
    if let Some(gain) = output_gain {
        bytes[head + 16..head + 18].copy_from_slice(&gain.to_le_bytes());
    }
    bytes[22..26].fill(0);
    let crc = ogg_crc(&bytes[..page_len]);
    bytes[22..26].copy_from_slice(&crc.to_le_bytes());
    bytes
}

/// Ogg page checksum: CRC-32, polynomial 0x04C11DB7, no reflection, zero init.
fn ogg_crc(data: &[u8]) -> u32 {
    data.iter().fold(0u32, |crc, &byte| {
        let mut crc = crc ^ ((byte as u32) << 24);
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04C1_1DB7
            } else {
                crc << 1
            };
        }
        crc
    })
}

pub const CONTINUED: u8 = 0x01;
pub const FIRST: u8 = 0x02;
pub const LAST: u8 = 0x04;

/// One Ogg page with the given lacing values and body.
pub fn ogg_page(
    serial: u32,
    sequence: u32,
    flags: u8,
    granule: u64,
    lacing: &[u8],
    body: &[u8],
) -> Vec<u8> {
    let mut page = b"OggS\0".to_vec();
    page.push(flags);
    page.extend(granule.to_le_bytes());
    page.extend(serial.to_le_bytes());
    page.extend(sequence.to_le_bytes());
    page.extend([0; 4]);
    page.push(lacing.len() as u8);
    page.extend(lacing);
    page.extend(body);
    let crc = ogg_crc(&page);
    page[22..26].copy_from_slice(&crc.to_le_bytes());
    page
}

/// A page holding one complete packet of at most 255 × 254 bytes.
pub fn ogg_packet_page(
    serial: u32,
    sequence: u32,
    flags: u8,
    granule: u64,
    packet: &[u8],
) -> Vec<u8> {
    ogg_packets_page(serial, sequence, flags, granule, &[packet])
}

/// A page holding complete packets with at most 255 lacing values in total.
pub fn ogg_packets_page(
    serial: u32,
    sequence: u32,
    flags: u8,
    granule: u64,
    packets: &[&[u8]],
) -> Vec<u8> {
    let mut lacing = Vec::new();
    for packet in packets {
        lacing.extend(vec![255; packet.len() / 255]);
        lacing.push((packet.len() % 255) as u8);
    }
    ogg_page(serial, sequence, flags, granule, &lacing, &packets.concat())
}

pub fn opus_head(channels: u8, pre_skip: u16) -> Vec<u8> {
    let mut head = b"OpusHead\x01".to_vec();
    head.push(channels);
    head.extend(pre_skip.to_le_bytes());
    head.extend(48_000u32.to_le_bytes());
    head.extend([0, 0, 0]);
    head
}

pub fn opus_tags() -> Vec<u8> {
    let mut tags = b"OpusTags".to_vec();
    tags.extend([0; 8]);
    tags
}

pub const OPUS_PRE_SKIP: u16 = 312;
pub const OPUS_PACKET_FRAMES: u64 = 960;

/// `count` mono 20 ms Opus packets of a 440 Hz tone.
#[cfg(feature = "opus")]
pub fn opus_packets(count: u64) -> Vec<Vec<u8>> {
    use opus::{Application, Channels, Encoder};

    let mut encoder = Encoder::new(48_000, Channels::Mono, Application::Audio).unwrap();
    let mut packet = [0u8; 4000];
    (0..count)
        .map(|index| {
            let pcm: Vec<i16> = (0..OPUS_PACKET_FRAMES)
                .map(|i| {
                    let t = (index * OPUS_PACKET_FRAMES + i) as f64 / 48_000.0;
                    ((t * 440.0 * std::f64::consts::TAU).sin() * 16_000.0) as i16
                })
                .collect();
            let len = encoder.encode(&pcm, &mut packet).unwrap();
            packet[..len].to_vec()
        })
        .collect()
}

/// The `OpusHead` and `OpusTags` pages of a mono stream.
pub fn opus_header_pages(serial: u32) -> Vec<Vec<u8>> {
    vec![
        ogg_packet_page(serial, 0, FIRST, 0, &opus_head(1, OPUS_PRE_SKIP)),
        ogg_packet_page(serial, 1, 0, 0, &opus_tags()),
    ]
}

/// A mono Ogg Opus stream, one item per page: `OpusHead`, `OpusTags`, then
/// one page per [`opus_packets`] packet. The last page's granule position
/// trims `end_trim` frames.
#[cfg(feature = "opus")]
pub fn opus_stream(serial: u32, packets: u64, end_trim: u64) -> Vec<Vec<u8>> {
    let mut pages = opus_header_pages(serial);
    for (index, packet) in (0..).zip(opus_packets(packets)) {
        let last = index + 1 == packets;
        let granule = (index + 1) * OPUS_PACKET_FRAMES - if last { end_trim } else { 0 };
        let flags = if last { LAST } else { 0 };
        pages.push(ogg_packet_page(
            serial,
            index as u32 + 2,
            flags,
            granule,
            &packet,
        ));
    }
    pages
}

pub const EBML: u32 = 0x1A45_DFA3;
pub const DOC_TYPE: u32 = 0x4282;
pub const SEGMENT: u32 = 0x1853_8067;
pub const TRACKS: u32 = 0x1654_AE6B;
pub const TRACK_ENTRY: u32 = 0xAE;
pub const TRACK_NUMBER: u32 = 0xD7;
pub const TRACK_TYPE: u32 = 0x83;
pub const CODEC_ID: u32 = 0x86;
pub const CODEC_PRIVATE: u32 = 0x63A2;
pub const CODEC_DELAY: u32 = 0x56AA;
pub const CONTENT_ENCODINGS: u32 = 0x6D80;
pub const CLUSTER: u32 = 0x1F43_B675;
pub const TIMESTAMP: u32 = 0xE7;
pub const SIMPLE_BLOCK: u32 = 0xA3;
pub const BLOCK_GROUP: u32 = 0xA0;
pub const BLOCK: u32 = 0xA1;
pub const DISCARD_PADDING: u32 = 0x75A2;
pub const VOID: u32 = 0xEC;
pub const CUES: u32 = 0x1C53_BB6B;
pub const ATTACHMENTS: u32 = 0x1941_A469;

/// An EBML element with a known size, encoded in as few bytes as possible.
pub fn element(id: u32, body: &[u8]) -> Vec<u8> {
    let mut out = id_bytes(id);
    let size = body.len() as u64;
    let len = (1..=8).find(|&len| size < (1 << (7 * len)) - 1).unwrap();
    let marked = size | 1 << (7 * len);
    out.extend(&marked.to_be_bytes()[8 - len..]);
    out.extend(body);
    out
}

/// The header of an element of unknown size; its children follow.
pub fn unknown_size(id: u32) -> Vec<u8> {
    let mut out = id_bytes(id);
    out.extend([0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
    out
}

fn id_bytes(id: u32) -> Vec<u8> {
    let bytes = id.to_be_bytes();
    let skip = bytes.iter().take_while(|&&byte| byte == 0).count();
    bytes[skip..].to_vec()
}

pub fn uint_element(id: u32, value: u64) -> Vec<u8> {
    let bytes = value.to_be_bytes();
    let skip = bytes.iter().take_while(|&&byte| byte == 0).count().min(7);
    element(id, &bytes[skip..])
}

/// The EBML header of a file with the given document type.
pub fn ebml_header(doc_type: &str) -> Vec<u8> {
    element(EBML, &element(DOC_TYPE, doc_type.as_bytes()))
}

/// A mono Opus track entry. `codec_delay` is in nanoseconds.
pub fn opus_track(number: u64, pre_skip: u16, codec_delay: Option<u64>) -> Vec<u8> {
    let mut body = [
        uint_element(TRACK_NUMBER, number),
        uint_element(TRACK_TYPE, 2),
        element(CODEC_ID, b"A_OPUS"),
        element(CODEC_PRIVATE, &opus_head(1, pre_skip)),
    ]
    .concat();
    if let Some(delay) = codec_delay {
        body.extend(uint_element(CODEC_DELAY, delay));
    }
    element(TRACK_ENTRY, &body)
}

/// Block body: track number, timestamp, flags, then the lacing and frames.
/// `lacing` is 0 (none), 1 (Xiph), 2 (fixed) or 3 (EBML).
pub fn block_body(track: u8, lacing: u8, frames: &[&[u8]]) -> Vec<u8> {
    assert!(track < 127);
    let mut body = vec![0x80 | track, 0, 0, 0x80 | lacing << 1];
    if lacing != 0 {
        body.push(frames.len() as u8 - 1);
    }
    let laced_sizes = &frames[..frames.len() - 1];
    match lacing {
        0 => assert_eq!(frames.len(), 1),
        1 => {
            for frame in laced_sizes {
                body.extend(vec![255; frame.len() / 255]);
                body.push((frame.len() % 255) as u8);
            }
        }
        2 => assert!(frames.iter().all(|frame| frame.len() == frames[0].len())),
        _ => {
            let mut previous = None;
            for frame in laced_sizes {
                let size = frame.len() as i64;
                body.extend(match previous {
                    None => ebml_lace_size(size as u64),
                    Some(previous) => ebml_lace_size((size - previous + 8191) as u64),
                });
                previous = Some(size);
            }
        }
    }
    for frame in frames {
        body.extend(*frame);
    }
    body
}

/// A two-byte EBML lacing size, biased by the caller for differences.
fn ebml_lace_size(value: u64) -> Vec<u8> {
    assert!(value < 0x3FFF);
    vec![0x40 | (value >> 8) as u8, value as u8]
}

pub fn ns_for_48k_frames(frames: u64) -> u64 {
    frames * 1_000_000_000 / 48_000
}
