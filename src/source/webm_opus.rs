//! Opus in WebM and Matroska, through libopus.

use std::fmt;
use std::io::Read;
use std::ops::Range;

use super::opus::{End, OpusDecoder, OpusHead, OpusOptions, to_decode_rate};
use super::{CountingReader, invalid};
use crate::{Error, PcmSource};

const EBML: u32 = 0x1A45_DFA3;
const DOC_TYPE: u32 = 0x4282;
const SEGMENT: u32 = 0x1853_8067;
const TRACKS: u32 = 0x1654_AE6B;
const TRACK_ENTRY: u32 = 0xAE;
const TRACK_NUMBER: u32 = 0xD7;
const CODEC_ID: u32 = 0x86;
const CODEC_PRIVATE: u32 = 0x63A2;
const CODEC_DELAY: u32 = 0x56AA;
const CONTENT_ENCODINGS: u32 = 0x6D80;
const CLUSTER: u32 = 0x1F43_B675;
const SIMPLE_BLOCK: u32 = 0xA3;
const BLOCK_GROUP: u32 = 0xA0;
const BLOCK: u32 = 0xA1;
const DISCARD_PADDING: u32 = 0x75A2;
const VOID: u32 = 0xEC;

/// Elements that can follow a cluster at the top of a segment. One of them, or
/// a new EBML header, is what ends a cluster of unknown size.
const SEGMENT_CHILDREN: [u32; 10] = [
    EBML,
    SEGMENT,
    0x114D_9B74, // SeekHead
    0x1549_A966, // Info
    TRACKS,
    CLUSTER,
    0x1C53_BB6B, // Cues
    0x1254_C367, // Tags
    0x1941_A469, // Attachments
    0x1043_A770, // Chapters
];

/// Largest header value read into memory: document type, codec ID, codec
/// private data. An `OpusHead` for mono or stereo is 19 bytes.
const MAX_HEADER_BYTES: u64 = 4096;

/// Streams PCM out of the first Opus track of a WebM or Matroska file, such as
/// a browser `MediaRecorder` recording.
///
/// Reads with plain `Read`, so an upload body can be decoded as it arrives.
/// Reads are buffered internally. Memory is one block and one decoded packet,
/// whatever the input length: other tracks, cues, tags, and attachments are
/// read past without being buffered. Applies the pre-skip (`CodecDelay`, or the `OpusHead` pre-skip
/// without it), the output gain, and the end trimming (`DiscardPadding`).
/// Segments and clusters of unknown size, as written by live recorders, are
/// supported, and so is block lacing.
///
/// Handles channel mapping family 0 (mono and stereo). Decoding stops at the
/// end of the first segment. Compressed or encrypted Opus tracks are passed
/// over, and [`Error::Unsupported`] when no other Opus track is left.
///
/// A file cut between elements is not an error: the blocks present are
/// decoded. A cut inside an element is [`Error::InvalidInput`].
pub struct WebmOpusSource<R: Read> {
    reader: EbmlReader<R>,
    decoder: OpusDecoder,
    track: u64,
    max_block_bytes: usize,
    segment_end: Option<u64>,
    /// Set while inside a cluster, to its end when the size is known.
    cluster: Option<Option<u64>>,
    block: Vec<u8>,
    /// Packets of the current block not decoded yet, as ranges of `block`.
    packets: Vec<Range<usize>>,
    next_packet: usize,
    /// Where the audio ends in the current block, in frames at the decode
    /// rate from the start of the stream, when it has `DiscardPadding`.
    block_end: Option<u64>,
    finished: bool,
}

impl<R: Read> WebmOpusSource<R> {
    /// Reads the headers up to the first Opus track, with default options.
    ///
    /// # Errors
    ///
    /// As for [`with_options`](Self::with_options).
    pub fn new(reader: R) -> Result<Self, Error> {
        Self::with_options(reader, OpusOptions::default())
    }

    /// Reads the headers up to the first Opus track.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidInput`] when the input is not a WebM or Matroska file,
    ///   or its headers are malformed.
    /// - [`Error::Unsupported`] when there is no Opus track, when the track is
    ///   compressed or encrypted, or for a channel mapping family other than 0.
    pub fn with_options(reader: R, options: OpusOptions) -> Result<Self, Error> {
        let mut reader = EbmlReader {
            input: CountingReader::new(reader, "truncated element"),
        };

        let header = reader.next()?.ok_or_else(|| invalid("empty input"))?;
        if header.id != EBML {
            return Err(invalid("not a WebM or Matroska file"));
        }
        let doc_type = read_doc_type(&mut reader, &header)?;
        if doc_type != b"webm" && doc_type != b"matroska" {
            return Err(Error::Unsupported(format!(
                "EBML document type {:?}",
                String::from_utf8_lossy(&doc_type)
            )));
        }

        let segment = loop {
            let element = reader.next()?.ok_or_else(|| invalid("missing Segment"))?;
            match element.id {
                SEGMENT => break element,
                VOID => reader.skip(&element)?,
                _ => return Err(invalid("missing Segment")),
            }
        };
        let track = loop {
            let element = reader
                .next()?
                .ok_or_else(|| invalid("no Tracks before the end of the input"))?;
            check_bounds(&element, segment.end)?;
            match element.id {
                TRACKS => break read_opus_track(&mut reader, &element)?,
                CLUSTER => return Err(invalid("Cluster before Tracks")),
                _ => reader.skip(&element)?,
            }
        };

        let head = OpusHead::parse(&track.codec_private)?;
        let pre_skip = track
            .codec_delay
            .map_or(head.pre_skip as u64, nanoseconds_to_48k);
        Ok(WebmOpusSource {
            reader,
            decoder: OpusDecoder::new(&head, pre_skip, &options)?,
            track: track.number,
            max_block_bytes: options.max_packet_bytes,
            segment_end: segment.end,
            cluster: None,
            block: Vec::new(),
            packets: Vec::new(),
            next_packet: 0,
            block_end: None,
            finished: false,
        })
    }

    /// Reads up to the next block of the Opus track. Returns false at the end
    /// of the segment.
    fn read_block(&mut self) -> Result<bool, Error> {
        loop {
            let Some(element) = self.next_element()? else {
                return Ok(false);
            };
            let in_cluster = self.cluster.is_some();
            match element.id {
                EBML | SEGMENT if !in_cluster => return Ok(false),
                CLUSTER if !in_cluster => self.cluster = Some(element.end),
                SIMPLE_BLOCK if in_cluster => {
                    if self.read_block_body(&element)? {
                        return self.start_block(0);
                    }
                }
                BLOCK_GROUP if in_cluster => {
                    if let Some(padding) = self.read_block_group(&element)? {
                        return self.start_block(padding);
                    }
                }
                _ => self.reader.skip(&element)?,
            }
        }
    }

    /// The next element inside the current segment, leaving clusters as they
    /// end. `None` at the end of the segment or of the input.
    fn next_element(&mut self) -> Result<Option<Element>, Error> {
        if let Some(Some(end)) = self.cluster
            && self.reader.position() >= end
        {
            self.cluster = None;
        }
        if self
            .segment_end
            .is_some_and(|end| self.reader.position() >= end)
        {
            return Ok(None);
        }
        let Some(element) = self.reader.next()? else {
            return Ok(None);
        };
        if self.cluster == Some(None) && SEGMENT_CHILDREN.contains(&element.id) {
            self.cluster = None;
        }
        let parent_end = match self.cluster {
            Some(Some(end)) => Some(end),
            _ => self.segment_end,
        };
        check_bounds(&element, parent_end)?;
        let top_level = self.cluster.is_none() && matches!(element.id, CLUSTER | SEGMENT);
        if element.size.is_none() && !top_level {
            return Err(unknown_size());
        }
        Ok(Some(element))
    }

    /// Reads a `BlockGroup`, returning the padding of its block when the block
    /// belongs to the Opus track.
    fn read_block_group(&mut self, group: &Element) -> Result<Option<i64>, Error> {
        let end = group.end.ok_or_else(unknown_size)?;
        let mut found = false;
        let mut padding = 0;
        while self.reader.position() < end {
            let element = self.reader.next_required()?;
            check_bounds(&element, Some(end))?;
            match element.id {
                BLOCK => found |= self.read_block_body(&element)?,
                DISCARD_PADDING => padding = self.reader.read_int(&element)?,
                _ => self.reader.skip(&element)?,
            }
        }
        Ok(found.then_some(padding))
    }

    /// Reads a `SimpleBlock` or `Block` into `block` when it belongs to the
    /// Opus track, and skips it otherwise.
    fn read_block_body(&mut self, element: &Element) -> Result<bool, Error> {
        let size = element.size.ok_or_else(unknown_size)?;
        let (track, track_len) = self.reader.read_vint()?;
        let rest = size
            .checked_sub(track_len)
            .ok_or_else(|| invalid("Block shorter than its track number"))?;
        if track != Some(self.track) {
            self.reader.skip_bytes(rest)?;
            return Ok(false);
        }
        if size > self.max_block_bytes as u64 {
            return Err(Error::LimitExceeded(
                "WebM block larger than max_packet_bytes",
            ));
        }
        self.block.resize(rest as usize, 0);
        self.reader.read_exact(&mut self.block)?;
        Ok(true)
    }

    /// Splits the block just read into packets. `padding` is the block's
    /// `DiscardPadding` in nanoseconds.
    fn start_block(&mut self, padding: i64) -> Result<bool, Error> {
        self.next_packet = 0;
        self.block_end = None;
        let started = self.split_block(padding);
        if started.is_err() {
            self.packets.clear();
        }
        started.map(|()| true)
    }

    fn split_block(&mut self, padding: i64) -> Result<(), Error> {
        // Timestamp (2 bytes), then flags.
        let flags = *self
            .block
            .get(2)
            .ok_or_else(|| invalid("truncated Block header"))?;
        split_laced(&self.block, flags, &mut self.packets)?;
        // Negative padding marks the start of the block, used for seeking only.
        if padding <= 0 {
            return Ok(());
        }
        let mut frames = 0;
        for packet in &self.packets {
            frames += self.decoder.packet_frames(&self.block[packet.clone()])?;
        }
        // Rounding the end position as Ogg does keeps both containers equal
        // below 48 kHz. Scaling up is exact: 48 kHz is a multiple of every
        // Opus decode rate.
        let rate = self.decoder.sample_rate();
        let block_end = (self.decoder.position() + frames) * (48_000 / rate) as u64;
        let end = block_end.saturating_sub(nanoseconds_to_48k(padding as u64));
        self.block_end = Some(to_decode_rate(end, rate));
        Ok(())
    }

    /// Decodes the next packet of the current block. Returns whether samples
    /// were kept, or `None` when the block has no packets left.
    fn decode_next_packet(&mut self) -> Result<Option<bool>, Error> {
        let Some(packet) = self.packets.get(self.next_packet).cloned() else {
            return Ok(None);
        };
        self.next_packet += 1;
        let end = self.block_end.map_or(End::Open, End::At);
        self.decoder.decode(&self.block[packet], end).map(Some)
    }
}

impl<R: Read> fmt::Debug for WebmOpusSource<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebmOpusSource")
            .field("decoder", &self.decoder)
            .field("track", &self.track)
            .field("finished", &self.finished)
            .finish_non_exhaustive()
    }
}

impl<R: Read> PcmSource for WebmOpusSource<R> {
    fn sample_rate(&self) -> u32 {
        self.decoder.sample_rate()
    }

    fn channels(&self) -> u16 {
        self.decoder.channels()
    }

    fn next_block(&mut self) -> Result<Option<&[i16]>, Error> {
        loop {
            match self.decode_next_packet()? {
                Some(true) => return Ok(Some(self.decoder.output())),
                Some(false) => {}
                None if self.finished => return Ok(None),
                None => self.finished = !self.read_block()?,
            }
        }
    }
}

/// The fields of the chosen track entry.
struct OpusTrack {
    number: u64,
    codec_private: Vec<u8>,
    codec_delay: Option<u64>,
}

/// Reads `Tracks` and returns its first Opus track.
fn read_opus_track<R: Read>(
    reader: &mut EbmlReader<R>,
    tracks: &Element,
) -> Result<OpusTrack, Error> {
    let end = tracks
        .end
        .ok_or_else(|| invalid("Tracks of unknown size"))?;
    let mut opus = None;
    let mut other_codec = None;
    let mut encoded_opus = false;
    while reader.position() < end {
        let entry = reader.next_required()?;
        check_bounds(&entry, Some(end))?;
        if entry.id != TRACK_ENTRY || opus.is_some() {
            reader.skip(&entry)?;
            continue;
        }
        let entry_end = entry
            .end
            .ok_or_else(|| invalid("TrackEntry of unknown size"))?;
        let mut number = None;
        let mut codec_id = Vec::new();
        let mut codec_private = None;
        let mut codec_delay = None;
        let mut encoded = false;
        let mut oversized_private = false;
        while reader.position() < entry_end {
            let field = reader.next_required()?;
            check_bounds(&field, Some(entry_end))?;
            match field.id {
                TRACK_NUMBER => number = Some(reader.read_uint(&field)?),
                CODEC_ID => codec_id = read_string(reader, &field)?,
                CODEC_PRIVATE if field.size.is_some_and(|size| size <= MAX_HEADER_BYTES) => {
                    codec_private = Some(reader.read_bytes(&field, MAX_HEADER_BYTES)?);
                }
                CODEC_PRIVATE => {
                    oversized_private = true;
                    reader.skip(&field)?;
                }
                CODEC_DELAY => codec_delay = Some(reader.read_uint(&field)?),
                CONTENT_ENCODINGS => {
                    encoded = true;
                    reader.skip(&field)?;
                }
                _ => reader.skip(&field)?,
            }
        }
        if codec_id != b"A_OPUS" {
            other_codec.get_or_insert(codec_id);
            continue;
        }
        if encoded {
            encoded_opus = true;
            continue;
        }
        opus = Some(OpusTrack {
            number: number
                .filter(|&number| number != 0)
                .ok_or_else(|| invalid("Opus track without a track number"))?,
            codec_private: codec_private.ok_or_else(|| {
                invalid(if oversized_private {
                    "oversized OpusHead"
                } else {
                    "Opus track without an OpusHead"
                })
            })?,
            codec_delay,
        });
    }
    opus.ok_or_else(|| match other_codec {
        _ if encoded_opus => Error::Unsupported("compressed or encrypted Opus track".into()),
        Some(codec) => Error::Unsupported(format!(
            "no Opus track (found {:?})",
            String::from_utf8_lossy(&codec)
        )),
        None => invalid("no tracks"),
    })
}

/// Reads a short string, without the zero bytes EBML allows as padding.
fn read_string<R: Read>(reader: &mut EbmlReader<R>, field: &Element) -> Result<Vec<u8>, Error> {
    let mut bytes = reader.read_bytes(field, 64)?;
    while bytes.last() == Some(&0) {
        bytes.pop();
    }
    Ok(bytes)
}

/// Reads the EBML header and returns its document type.
fn read_doc_type<R: Read>(reader: &mut EbmlReader<R>, header: &Element) -> Result<Vec<u8>, Error> {
    let end = header
        .end
        .filter(|_| header.size.is_some_and(|size| size <= MAX_HEADER_BYTES))
        .ok_or_else(|| invalid("oversized EBML header"))?;
    // The EBML specification's default.
    let mut doc_type = b"matroska".to_vec();
    while reader.position() < end {
        let field = reader.next_required()?;
        check_bounds(&field, Some(end))?;
        match field.id {
            DOC_TYPE => doc_type = read_string(reader, &field)?,
            _ => reader.skip(&field)?,
        }
    }
    Ok(doc_type)
}

/// Fills `packets` with the ranges of `block` holding each laced frame. The
/// lacing data starts after the 4-byte block header (track number already
/// read).
fn split_laced(block: &[u8], flags: u8, packets: &mut Vec<Range<usize>>) -> Result<(), Error> {
    let bad_lacing = || invalid("malformed Block lacing");
    packets.clear();
    let lacing = (flags >> 1) & 0b11;
    if lacing == 0 {
        packets.push(3..block.len());
        return Ok(());
    }
    let count = *block.get(3).ok_or_else(bad_lacing)? as usize + 1;
    let mut position = 4;
    // The sizes of all frames but the last, held as `0..size` until the
    // offset of the first frame is known.
    match lacing {
        // Xiph: each size as a run of 255s ended by a smaller byte.
        0b01 => {
            for _ in 1..count {
                let mut size = 0;
                loop {
                    let byte = *block.get(position).ok_or_else(bad_lacing)?;
                    position += 1;
                    size += byte as usize;
                    if byte != 255 {
                        break;
                    }
                }
                packets.push(0..size);
            }
        }
        // Fixed: equal sizes, so nothing is stored.
        0b10 => {
            let total = block.len() - position;
            if !total.is_multiple_of(count) {
                return Err(bad_lacing());
            }
            packets.resize(count - 1, 0..total / count);
        }
        // EBML: the first size, then signed differences from the previous one.
        _ if count > 1 => {
            let (first, len) = vint(&block[position..]).ok_or_else(bad_lacing)?;
            position += len;
            let mut size = first as i64;
            packets.push(0..usize::try_from(first).map_err(|_| bad_lacing())?);
            for _ in 2..count {
                let (raw, len) = vint(&block[position..]).ok_or_else(bad_lacing)?;
                position += len;
                let bias = (1i64 << (7 * len - 1)) - 1;
                size = size.checked_add(raw as i64 - bias).ok_or_else(bad_lacing)?;
                packets.push(0..usize::try_from(size).map_err(|_| bad_lacing())?);
            }
        }
        _ => {}
    }
    for packet in packets.iter_mut() {
        let end = position
            .checked_add(packet.end)
            .filter(|&end| end <= block.len());
        let end = end.ok_or_else(bad_lacing)?;
        *packet = position..end;
        position = end;
    }
    packets.push(position..block.len());
    Ok(())
}

/// Decodes an EBML variable-size integer from the start of `bytes`, returning
/// its value and length.
fn vint(bytes: &[u8]) -> Option<(u64, usize)> {
    let first = *bytes.first()?;
    let len = first.leading_zeros() as usize + 1;
    if len > 8 || bytes.len() < len {
        return None;
    }
    let mut value = (first as u64) & (0xFF >> len);
    for &byte in &bytes[1..len] {
        value = value << 8 | byte as u64;
    }
    Some((value, len))
}

fn unknown_size() -> Error {
    invalid("unknown size on an element other than a Segment or Cluster")
}

fn nanoseconds_to_48k(nanoseconds: u64) -> u64 {
    ((nanoseconds as u128 * 48_000 + 500_000_000) / 1_000_000_000) as u64
}

fn check_bounds(element: &Element, parent_end: Option<u64>) -> Result<(), Error> {
    match (element.end, parent_end) {
        (Some(end), Some(parent_end)) if end > parent_end => {
            Err(invalid("element overruns its parent"))
        }
        _ => Ok(()),
    }
}

/// An element header.
#[derive(Debug, Clone, Copy)]
struct Element {
    id: u32,
    /// `None` for an unknown size.
    size: Option<u64>,
    /// Offset of the end of the element, when its size is known.
    end: Option<u64>,
}

/// Reads EBML element headers and values, tracking the offset so that
/// known-size parents can be closed.
struct EbmlReader<R> {
    input: CountingReader<R>,
}

impl<R: Read> EbmlReader<R> {
    fn position(&self) -> u64 {
        self.input.position()
    }

    /// Reads the next element header. Returns `None` at the end of the input
    /// when it falls between elements.
    fn next(&mut self) -> Result<Option<Element>, Error> {
        let mut first = [0u8; 1];
        if self.input.read_full(&mut first)? == 0 {
            return Ok(None);
        }
        let id_len = first[0].leading_zeros() as usize + 1;
        if id_len > 4 {
            return Err(invalid("malformed element ID"));
        }
        let mut id_bytes = [0u8; 4];
        self.input.read_exact(&mut id_bytes[..id_len - 1])?;
        let id = id_bytes[..id_len - 1]
            .iter()
            .fold(first[0] as u32, |id, &byte| id << 8 | byte as u32);

        let (size, _) = self.read_vint()?;
        let end = match size {
            Some(size) => Some(
                self.position()
                    .checked_add(size)
                    .ok_or_else(|| invalid("element size overflows"))?,
            ),
            None => None,
        };
        Ok(Some(Element { id, size, end }))
    }

    /// Like [`next`](Self::next), inside a parent that is not finished.
    fn next_required(&mut self) -> Result<Element, Error> {
        self.next()?.ok_or_else(|| invalid("truncated element"))
    }

    /// Reads a size-style variable integer, returning `None` for the
    /// reserved unknown-size value, and the byte count.
    fn read_vint(&mut self) -> Result<(Option<u64>, u64), Error> {
        let mut bytes = [0u8; 8];
        self.input.read_exact(&mut bytes[..1])?;
        let len = bytes[0].leading_zeros() as usize + 1;
        if len > 8 {
            return Err(invalid("malformed element size"));
        }
        self.input.read_exact(&mut bytes[1..len])?;
        let (value, _) = vint(&bytes[..len]).expect("length checked above");
        let unknown = value == (1u64 << (7 * len)) - 1;
        Ok(((!unknown).then_some(value), len as u64))
    }

    fn read_uint(&mut self, element: &Element) -> Result<u64, Error> {
        let bytes = self.read_bytes(element, 8)?;
        Ok(bytes
            .iter()
            .fold(0, |value, &byte| value << 8 | byte as u64))
    }

    fn read_int(&mut self, element: &Element) -> Result<i64, Error> {
        let bytes = self.read_bytes(element, 8)?;
        let unsigned = bytes
            .iter()
            .fold(0, |value, &byte| value << 8 | byte as u64);
        // Sign-extend from the element's width.
        let shift = 64 - 8 * bytes.len() as u32;
        Ok(if bytes.is_empty() {
            0
        } else {
            ((unsigned << shift) as i64) >> shift
        })
    }

    fn read_bytes(&mut self, element: &Element, max: u64) -> Result<Vec<u8>, Error> {
        let size = element
            .size
            .filter(|&size| size <= max)
            .ok_or_else(|| invalid("oversized header element"))?;
        let mut bytes = vec![0; size as usize];
        self.input.read_exact(&mut bytes)?;
        Ok(bytes)
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), Error> {
        self.input.read_exact(buf)
    }

    fn skip(&mut self, element: &Element) -> Result<(), Error> {
        let size = element.size.ok_or_else(unknown_size)?;
        self.input.skip(size)
    }

    fn skip_bytes(&mut self, len: u64) -> Result<(), Error> {
        self.input.skip(len)
    }
}
