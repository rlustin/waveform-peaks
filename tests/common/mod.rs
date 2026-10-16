#![allow(dead_code)]

use std::{ffi::OsStr, path::PathBuf, process::Command};

use waveform_peaks::{Bits, ChannelMode, Error, Options, PcmSource};

/// Drains `source`.
pub fn samples<P: PcmSource>(mut source: P) -> Result<Vec<i16>, Error> {
    let mut samples = Vec::new();
    while let Some(block) = source.next_block()? {
        samples.extend(block);
    }
    Ok(samples)
}

/// Like `assert_eq!`, without printing thousands of samples.
#[track_caller]
pub fn assert_same(actual: &[i16], expected: &[i16]) {
    assert_eq!(actual.len(), expected.len(), "sample count");
    let first_difference = actual.iter().zip(expected).position(|(a, e)| a != e);
    assert_eq!(first_difference, None, "first differing sample");
}

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

/// Splits an ADTS file into its raw AAC frames, and returns the matching
/// two-byte `AudioSpecificConfig`.
pub fn adts_frames(adts: &[u8]) -> ([u8; 2], Vec<Vec<u8>>) {
    let mut frames = Vec::new();
    let mut config = [0; 2];
    let mut at = 0;
    while at < adts.len() {
        let header = &adts[at..];
        let header_len = if header[1] & 1 == 1 { 7 } else { 9 };
        let len = ((header[3] as usize & 3) << 11)
            | (header[4] as usize) << 3
            | (header[5] as usize) >> 5;
        let profile = (header[2] >> 6) as u16;
        let frequency = ((header[2] >> 2) & 0xF) as u16;
        let channels = ((header[2] & 1) << 2 | header[3] >> 6) as u16;
        config = ((profile + 1) << 11 | frequency << 7 | channels << 3).to_be_bytes();
        frames.push(header[header_len..len].to_vec());
        at += len;
    }
    (config, frames)
}

/// Splits an MPEG-1 layer II or III stream into its frames, headers included.
pub fn mpa_frames(stream: &[u8]) -> Vec<Vec<u8>> {
    const LAYER_2: [u32; 15] = [
        0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384,
    ];
    const LAYER_3: [u32; 15] = [
        0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
    ];
    let mut frames = Vec::new();
    let mut at = 0;
    while at < stream.len() {
        let header = &stream[at..];
        assert_eq!(header[1] & 0xF8, 0xF8, "MPEG-1 frame at {at}");
        let table = if header[1] & 0x06 == 0x02 {
            LAYER_3
        } else {
            LAYER_2
        };
        let bit_rate = table[(header[2] >> 4) as usize] * 1000;
        let rate = [44_100, 48_000, 32_000][(header[2] >> 2 & 3) as usize];
        let len = (144 * bit_rate / rate) as usize + (header[2] >> 1 & 1) as usize;
        frames.push(header[..len].to_vec());
        at += len;
    }
    frames
}

pub fn mp4_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = (8 + body.len() as u32).to_be_bytes().to_vec();
    out.extend(kind);
    out.extend(body);
    out
}

pub fn full_box(kind: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
    let mut full = (flags | (version as u32) << 24).to_be_bytes().to_vec();
    full.extend(body);
    mp4_box(kind, &full)
}

/// An MPEG-4 descriptor, with its size in the four-byte form FFmpeg writes.
fn descriptor(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![tag, 0x80, 0x80, 0x80, body.len() as u8];
    out.extend(body);
    out
}

pub fn esds(object_type: u8, config: &[u8]) -> Vec<u8> {
    let mut decoder_config = vec![object_type, 0x15, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    decoder_config.extend(descriptor(0x05, config));
    let mut es = vec![0, 1, 0];
    es.extend(descriptor(0x04, &decoder_config));
    es.extend(descriptor(0x06, &[2]));
    full_box(b"esds", 0, 0, &descriptor(0x03, &es))
}

/// An audio sample entry such as `mp4a`, with the given child boxes.
pub fn audio_sample_entry(kind: &[u8; 4], channels: u16, rate: u32, children: &[u8]) -> Vec<u8> {
    let mut body = vec![0; 6];
    body.extend(1u16.to_be_bytes());
    body.extend([0; 8]);
    body.extend(channels.to_be_bytes());
    body.extend(16u16.to_be_bytes());
    body.extend([0; 4]);
    body.extend((rate << 16).to_be_bytes());
    body.extend(children);
    mp4_box(kind, &body)
}

pub fn mp4a(config: &[u8], channels: u16, rate: u32) -> Vec<u8> {
    audio_sample_entry(b"mp4a", channels, rate, &esds(0x40, config))
}

/// A `dOps` box: the fields of `OpusHead` without its magic, big-endian.
pub fn dops(channels: u8, pre_skip: u16, input_rate: u32, gain: i16) -> Vec<u8> {
    let mut body = vec![0, channels];
    body.extend(pre_skip.to_be_bytes());
    body.extend(input_rate.to_be_bytes());
    body.extend(gain.to_be_bytes());
    body.push(0);
    mp4_box(b"dOps", &body)
}

/// A track of an MP4 file built by [`faststart_mp4`] or [`fragmented_mp4`].
pub struct Mp4Track {
    pub id: u32,
    pub handler: [u8; 4],
    pub sample_entry: Vec<u8>,
    /// Media timescale.
    pub timescale: u32,
    /// First edit: duration in the movie timescale, media time.
    pub edit: Option<(u64, i64)>,
    /// Samples in storage order, grouped in chunks.
    pub chunks: Vec<Vec<Vec<u8>>>,
    pub wide_offsets: bool,
    /// Adds the data handler QuickTime files have in `minf`.
    pub quicktime: bool,
}

impl Mp4Track {
    pub fn audio(
        config: &[u8],
        channels: u16,
        rate: u32,
        samples: &[Vec<u8>],
        per_chunk: usize,
    ) -> Self {
        Mp4Track {
            id: 1,
            handler: *b"soun",
            sample_entry: mp4a(config, channels, rate),
            timescale: rate,
            edit: None,
            chunks: samples.chunks(per_chunk).map(<[_]>::to_vec).collect(),
            wide_offsets: false,
            quicktime: false,
        }
    }

    /// An Opus track at the 48 kHz media timescale, with the given `dOps` box.
    pub fn opus(dops: &[u8], samples: &[Vec<u8>], per_chunk: usize) -> Self {
        Mp4Track {
            sample_entry: audio_sample_entry(
                b"Opus",
                dops.get(9).map_or(1, |&c| c as u16),
                48_000,
                dops,
            ),
            ..Mp4Track::audio(&[], 2, 48_000, samples, per_chunk)
        }
    }

    fn trak(&self, chunk_offsets: &[u64]) -> Vec<u8> {
        let mut tkhd = vec![0; 8];
        tkhd.extend(self.id.to_be_bytes());
        tkhd.resize(80, 0);
        let mut edts = Vec::new();
        if let Some((duration, media_time)) = self.edit {
            let mut elst = 1u32.to_be_bytes().to_vec();
            elst.extend((duration as u32).to_be_bytes());
            elst.extend((media_time as i32).to_be_bytes());
            elst.extend(0x0001_0000u32.to_be_bytes());
            edts = mp4_box(b"edts", &full_box(b"elst", 0, 0, &elst));
        }
        let mut mdhd = vec![0; 8];
        mdhd.extend(self.timescale.to_be_bytes());
        mdhd.extend([0, 0, 0, 0, 0x55, 0xC4, 0, 0]);
        let mut hdlr = vec![0; 4];
        hdlr.extend(self.handler);
        hdlr.extend([0; 13]);

        let mut stsc = Vec::new();
        let mut runs = 0u32;
        let mut previous = None;
        for (index, chunk) in self.chunks.iter().enumerate() {
            if previous != Some(chunk.len()) {
                stsc.extend((index as u32 + 1).to_be_bytes());
                stsc.extend((chunk.len() as u32).to_be_bytes());
                stsc.extend(1u32.to_be_bytes());
                runs += 1;
                previous = Some(chunk.len());
            }
        }
        let mut stsc_body = runs.to_be_bytes().to_vec();
        stsc_body.extend(stsc);
        let sizes: Vec<u32> = self
            .chunks
            .iter()
            .flatten()
            .map(|s| s.len() as u32)
            .collect();
        let mut stsz = 0u32.to_be_bytes().to_vec();
        stsz.extend((sizes.len() as u32).to_be_bytes());
        for size in &sizes {
            stsz.extend(size.to_be_bytes());
        }
        let mut offsets = (chunk_offsets.len() as u32).to_be_bytes().to_vec();
        for &offset in chunk_offsets {
            if self.wide_offsets {
                offsets.extend(offset.to_be_bytes());
            } else {
                offsets.extend((offset as u32).to_be_bytes());
            }
        }
        let offsets_kind = if self.wide_offsets { b"co64" } else { b"stco" };
        let mut stsd = 1u32.to_be_bytes().to_vec();
        stsd.extend(&self.sample_entry);
        let stbl = [
            full_box(b"stsd", 0, 0, &stsd),
            full_box(b"stts", 0, 0, &0u32.to_be_bytes()),
            full_box(b"stsc", 0, 0, &stsc_body),
            full_box(b"stsz", 0, 0, &stsz),
            full_box(offsets_kind, 0, 0, &offsets),
        ]
        .concat();
        let mut minf = Vec::new();
        if self.quicktime {
            let mut dhlr = b"dhlralis".to_vec();
            dhlr.extend([0; 13]);
            minf = full_box(b"hdlr", 0, 0, &dhlr);
        }
        minf.extend(mp4_box(b"stbl", &stbl));
        let mdia = [
            full_box(b"mdhd", 0, 0, &mdhd),
            full_box(b"hdlr", 0, 0, &hdlr),
            mp4_box(b"minf", &minf),
        ]
        .concat();
        mp4_box(
            b"trak",
            &[
                full_box(b"tkhd", 0, 3, &tkhd),
                edts,
                mp4_box(b"mdia", &mdia),
            ]
            .concat(),
        )
    }
}

fn ftyp() -> Vec<u8> {
    mp4_box(b"ftyp", b"M4A \0\0\0\0M4A mp42isom")
}

fn moov(movie_timescale: u32, traks: &[Vec<u8>], extra: &[u8]) -> Vec<u8> {
    let mut mvhd = vec![0; 8];
    mvhd.extend(movie_timescale.to_be_bytes());
    mvhd.resize(96, 0);
    mp4_box(
        b"moov",
        &[
            full_box(b"mvhd", 0, 0, &mvhd),
            traks.concat(),
            extra.to_vec(),
        ]
        .concat(),
    )
}

/// A non-fragmented MP4 file with its moov box first. Chunks of the tracks are
/// stored in turn, `extra` goes at the end of the moov box, and `between`
/// between the moov and mdat boxes.
pub fn faststart_mp4(
    tracks: &[Mp4Track],
    movie_timescale: u32,
    extra: &[u8],
    between: &[u8],
) -> Vec<u8> {
    let build = |offsets: &[Vec<u64>]| {
        let traks: Vec<Vec<u8>> = tracks.iter().zip(offsets).map(|(t, o)| t.trak(o)).collect();
        moov(movie_timescale, &traks, extra)
    };
    let placeholder: Vec<Vec<u64>> = tracks.iter().map(|t| vec![0; t.chunks.len()]).collect();
    let data_start = (ftyp().len() + build(&placeholder).len() + between.len() + 8) as u64;
    let mut offsets = vec![Vec::new(); tracks.len()];
    let mut mdat = Vec::new();
    let rounds = tracks.iter().map(|t| t.chunks.len()).max().unwrap_or(0);
    for round in 0..rounds {
        for (index, track) in tracks.iter().enumerate() {
            if let Some(chunk) = track.chunks.get(round) {
                offsets[index].push(data_start + mdat.len() as u64);
                mdat.extend(chunk.concat());
            }
        }
    }
    [
        ftyp(),
        build(&offsets),
        between.to_vec(),
        mp4_box(b"mdat", &mdat),
    ]
    .concat()
}

/// A fragmented MP4 file: an empty moov box, then a moof and mdat box per
/// fragment. Samples are addressed from the moof box (`default-base-is-moof`)
/// or, with `explicit_base`, through a base data offset.
pub fn fragmented_mp4(
    track: &Mp4Track,
    fragments: &[Vec<Vec<u8>>],
    explicit_base: bool,
) -> Vec<u8> {
    let empty = Mp4Track {
        id: track.id,
        handler: track.handler,
        sample_entry: track.sample_entry.clone(),
        timescale: track.timescale,
        edit: track.edit,
        chunks: Vec::new(),
        wide_offsets: false,
        quicktime: track.quicktime,
    };
    let mut trex = track.id.to_be_bytes().to_vec();
    trex.extend([0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    let mvex = mp4_box(b"mvex", &full_box(b"trex", 0, 0, &trex));
    let mut out = [ftyp(), moov(1000, &[empty.trak(&[])], &mvex)].concat();
    for (sequence, samples) in fragments.iter().enumerate() {
        let build = |data_offset: u32, base: u64| {
            let mut tfhd = track.id.to_be_bytes().to_vec();
            let flags = if explicit_base {
                tfhd.extend(base.to_be_bytes());
                0x1
            } else {
                0x2_0000
            };
            let mut trun = (samples.len() as u32).to_be_bytes().to_vec();
            trun.extend(data_offset.to_be_bytes());
            for sample in samples {
                trun.extend((sample.len() as u32).to_be_bytes());
            }
            let traf = [
                full_box(b"tfhd", 0, flags, &tfhd),
                full_box(b"trun", 0, 0x201, &trun),
            ]
            .concat();
            mp4_box(
                b"moof",
                &[
                    full_box(b"mfhd", 0, 0, &(sequence as u32 + 1).to_be_bytes()),
                    mp4_box(b"traf", &traf),
                ]
                .concat(),
            )
        };
        let moof_start = out.len() as u64;
        let moof_len = build(0, 0).len();
        let data_start = moof_start + moof_len as u64 + 8;
        let moof = if explicit_base {
            build(0, data_start)
        } else {
            build((moof_len + 8) as u32, 0)
        };
        out.extend(moof);
        out.extend(mp4_box(b"mdat", &samples.concat()));
    }
    out
}

/// Moves the sample description of every track after its sample tables, as
/// Chrome writes them. Box sizes are unchanged, so chunk offsets stay valid.
pub fn stsd_last(file: &[u8]) -> Vec<u8> {
    let be = |data: &[u8], at: usize| u32::from_be_bytes(data[at..at + 4].try_into().unwrap());
    let mut out = file.to_vec();
    let moov_end = file
        .windows(4)
        .position(|w| w == b"mdat")
        .unwrap_or(file.len());
    let mut from = 0;
    while let Some(found) = file[from..moov_end].windows(4).position(|w| w == b"stbl") {
        let start = from + found + 4;
        let end = start - 8 + be(file, start - 8) as usize;
        let (mut stsd, mut rest) = (Vec::<u8>::new(), Vec::<u8>::new());
        let mut at = start;
        while at < end {
            let size = be(file, at) as usize;
            let child = &file[at..at + size];
            if &child[4..8] == b"stsd" {
                stsd.extend_from_slice(child);
            } else {
                rest.extend_from_slice(child);
            }
            at += size;
        }
        rest.extend(stsd);
        out[start..end].copy_from_slice(&rest);
        from = end;
    }
    out
}

/// The samples of the first track of a non-fragmented MP4 file, and its
/// `AudioSpecificConfig`.
pub fn mp4_samples(file: &[u8]) -> (Vec<u8>, Vec<Vec<u8>>) {
    fn find<'a>(mut data: &'a [u8], path: &[&[u8; 4]]) -> &'a [u8] {
        for kind in path {
            loop {
                let size = u32::from_be_bytes(data[..4].try_into().unwrap()) as usize;
                if &data[4..8] == *kind {
                    data = &data[8..size];
                    break;
                }
                data = &data[size..];
            }
        }
        data
    }
    let be = |data: &[u8], at: usize| u32::from_be_bytes(data[at..at + 4].try_into().unwrap());
    let stbl = find(file, &[b"moov", b"trak", b"mdia", b"minf", b"stbl"]);
    let entry = &find(stbl, &[b"stsd"])[8..];
    let esds = &entry[entry.windows(4).position(|w| w == b"esds").unwrap() + 4..];
    let config_tag = esds
        .windows(2)
        .position(|w| w[0] == 0x05 && w[1] & 0x80 != 0)
        .unwrap();
    let config_len = esds[config_tag + 4] as usize;
    let config = esds[config_tag + 5..config_tag + 5 + config_len].to_vec();
    let stsz = find(stbl, &[b"stsz"]);
    let sizes: Vec<usize> = (0..be(stsz, 8) as usize)
        .map(|i| be(stsz, 12 + i * 4) as usize)
        .collect();
    let stco = find(stbl, &[b"stco"]);
    let offsets: Vec<usize> = (0..be(stco, 4) as usize)
        .map(|i| be(stco, 8 + i * 4) as usize)
        .collect();
    let stsc = find(stbl, &[b"stsc"]);
    let runs: Vec<(usize, usize)> = (0..be(stsc, 4) as usize)
        .map(|i| {
            (
                be(stsc, 8 + i * 12) as usize,
                be(stsc, 12 + i * 12) as usize,
            )
        })
        .collect();
    let mut samples = Vec::new();
    let mut sizes = sizes.into_iter();
    for (chunk, &offset) in offsets.iter().enumerate() {
        let per_chunk = runs
            .iter()
            .rev()
            .find(|(first, _)| chunk + 1 >= *first)
            .unwrap()
            .1;
        let mut at = offset;
        for size in sizes.by_ref().take(per_chunk) {
            samples.push(file[at..at + size].to_vec());
            at += size;
        }
    }
    (config, samples)
}

/// A `udta` box holding an `iTunSMPB` tag, after a large cover art item.
/// `iso_meta` gives `meta` the full-box header of ISO files rather than
/// QuickTime's bare form.
pub fn itunsmpb_udta(priming: u32, padding: u32, length: u64, iso_meta: bool) -> Vec<u8> {
    let text = format!(" 00000000 {priming:08X} {padding:08X} {length:016X} 00000000");
    let mut data = vec![0, 0, 0, 1, 0, 0, 0, 0];
    data.extend(text.as_bytes());
    let mut name = vec![0; 4];
    name.extend(b"iTunSMPB");
    let mut mean = vec![0; 4];
    mean.extend(b"com.apple.iTunes");
    let item = mp4_box(
        b"----",
        &[
            mp4_box(b"mean", &mean),
            mp4_box(b"name", &name),
            mp4_box(b"data", &data),
        ]
        .concat(),
    );
    let cover = mp4_box(b"covr", &mp4_box(b"data", &vec![0xAB; 100_000]));
    let ilst = mp4_box(b"ilst", &[cover, item].concat());
    let hdlr = full_box(b"hdlr", 0, 0, b"\0\0\0\0mdirappl\0\0\0\0\0\0\0\0\0");
    let meta_body = [hdlr, ilst].concat();
    let meta = if iso_meta {
        full_box(b"meta", 0, 0, &meta_body)
    } else {
        mp4_box(b"meta", &meta_body)
    };
    mp4_box(b"udta", &meta)
}
