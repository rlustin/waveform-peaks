//! Ogg Vorbis decoding through lewton, trimmed as libsndfile trims it for
//! audiowaveform.

use std::collections::VecDeque;
use std::fmt;
use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};

use lewton::audio::{AudioReadError, PreviousWindowRight, read_audio_packet_generic};
use lewton::header::{
    HeaderReadError, IdentHeader, SetupHeader, read_header_ident, read_header_setup,
};
use lewton::samples::InterleavedSamples;

use super::ogg_pages::{Group, GroupReader};
use super::{Overflow, invalid};
use crate::{Error, PcmSource};

/// Held frames released per block at the end of the stream.
const TAIL_FRAMES: usize = 4096;

/// Decoder settings for [`OggVorbisSource`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct VorbisOptions {
    /// Handling of decoded samples beyond full scale.
    pub overflow: Overflow,
    /// Largest Ogg packet buffered before decoding, and largest size of the
    /// packets of consecutive pages without granule position. Bounds memory
    /// against crafted sizes. The source keeps the packets of two pages, so
    /// memory can reach twice this size, plus spare capacity of the buffers.
    /// The default, 16 MiB, leaves room for a comment header carrying cover
    /// art. Audio packets are far smaller.
    pub max_packet_bytes: usize,
    /// Largest sum, over the codebooks of the setup header, of entries
    /// times dimensions. Bounds the memory that the decoder allocates for
    /// the codebooks, which a small crafted header can make large: about
    /// 100 bytes per entry while it reads one codebook. libvorbis and FFmpeg
    /// write about 63,000. The default is 262,144.
    pub max_codebook_values: usize,
    /// Largest number of frames held back to end a stream where
    /// audiowaveform ends it, when the stream starts with a negative offset
    /// (see [`OggVorbisSource`]). Memory use is 2 bytes per sample, so at
    /// most this number times the channels times 2 bytes. Above this
    /// number, no frames are held back, and the output can end up to that
    /// many frames later than the audiowaveform output. The default,
    /// 480,000, is 10 seconds at 48 kHz: 1.9 MB in stereo, and 245 MB at
    /// the Vorbis maximum of 255 channels.
    pub max_held_frames: usize,
}

impl Default for VorbisOptions {
    fn default() -> Self {
        VorbisOptions {
            overflow: Overflow::Saturate,
            max_packet_bytes: 16 * 1024 * 1024,
            max_codebook_values: 1 << 18,
            max_held_frames: 480_000,
        }
    }
}

/// Streams PCM out of an Ogg Vorbis file.
///
/// Reads with plain `Read`, so an upload body can be decoded as it arrives.
/// Memory is two Ogg pages, the packets of two pages (see
/// [`VorbisOptions::max_packet_bytes`]), one decoded packet, the codebooks
/// and the frames held back (see below), whatever the input length.
///
/// Only the logical stream of the first page is decoded, up to its
/// end-of-stream page, so chained files yield their first link, as with
/// audiowaveform. Bytes that are not part of a page with a valid checksum
/// are skipped, and a page cut short at the end of the input is dropped.
/// As with audiowaveform, the stream also ends at a page whose version is
/// not 0, and at a false page header that claims more bytes than are left.
/// After a missing page, decoding continues at the next whole packet.
///
/// The output is trimmed as libvorbis and libsndfile trim it for
/// audiowaveform. If the first granule position is lower than the frames
/// before it, the start of its packet loses the difference, at most the
/// whole packet. The granule position of the end-of-stream page trims the end
/// of its last packet. The output stops at the last granule position of the
/// stream, less the start offset of a stream that starts later than its first
/// page says.
///
/// A file that FFmpeg cuts without re-encoding has a first granule position
/// lower than the frames before it, so its output stops before the end of
/// its audio. To stop there, the source holds back up to
/// [`VorbisOptions::max_held_frames`] frames. If the granule positions do not
/// agree with each other, the output can end later than audiowaveform's,
/// which reads the last granule position before it decodes. A stream with
/// no granule position is decoded to its end, where audiowaveform fails.
///
/// lewton refuses some damaged packets that libvorbis decodes, for example
/// a packet whose window flags do not match the previous block. Silence of
/// the same length replaces such a packet and the next one, so the output
/// stays in time with audiowaveform's.
///
/// lewton panics on some crafted packets in builds with overflow checks.
/// The source catches the panic and handles the packet as a refused one,
/// unless the build sets `panic = "abort"`. The panic hook still runs, and
/// the default one prints a message for each such packet.
pub struct OggVorbisSource<R: Read> {
    groups: GroupReader<R>,
    ident: IdentHeader,
    setup: SetupHeader,
    window: PreviousWindowRight,
    options: VorbisOptions,
    current: Group,
    next_packet: usize,
    lookahead: Option<Group>,
    /// Whether `current` is the last group of the stream.
    last_page: bool,
    /// Granule position of the last group that has one.
    last_granule: Option<i64>,
    /// The block flag of each mode of the setup header.
    long_modes: Vec<bool>,
    /// Frames since the start or the last reset, counted as libvorbis
    /// returns them. `None` when libvorbis has lost the count, after a
    /// packet that it does not decode: the next packet then counts as 0.
    sample_count: Option<u64>,
    /// Frames since the start or the last reset, counted as libsndfile
    /// counts them for the start offset: without losing the count.
    duration: u64,
    /// Position of the last packet decoded, known from a granule position
    /// since the start or the last reset.
    position: Option<i64>,
    /// Block size of the last packet that libvorbis decodes, or `None` after
    /// a reset.
    previous_block: Option<u64>,
    /// Decoded samples that libvorbis returns with the next packet: lewton
    /// returns the samples up to the next window, libvorbis up to the
    /// middle of the overlap.
    early: VecDeque<i16>,
    /// Granule position of the first page less its frames, clamped to 0 if
    /// the granule position is not negative. Moves the end of the output.
    start_offset: Option<i128>,
    /// Output frames allowed in total, known from the last page.
    limit: Option<u64>,
    emitted: u64,
    /// Frames held back, while the last page is unknown.
    hold: usize,
    held: VecDeque<i16>,
    pcm: Vec<i16>,
    finished: bool,
}

impl<R: Read> OggVorbisSource<R> {
    /// Reads the identification, comment and setup headers with default
    /// options.
    ///
    /// # Errors
    ///
    /// As for [`with_options`](Self::with_options).
    pub fn new(reader: R) -> Result<Self, Error> {
        Self::with_options(reader, VorbisOptions::default())
    }

    /// Reads the identification, comment and setup headers.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidInput`] when the input is not an Ogg Vorbis stream,
    ///   for example when its first page belongs to a Skeleton stream.
    /// - [`Error::Unsupported`] for a Vorbis version other than 0.
    /// - [`Error::LimitExceeded`] when a header passes
    ///   [`VorbisOptions::max_packet_bytes`] or
    ///   [`VorbisOptions::max_codebook_values`].
    pub fn with_options(reader: R, options: VorbisOptions) -> Result<Self, Error> {
        let mut groups = GroupReader::new(reader, options.max_packet_bytes);
        let mut headers = Headers {
            group: Group::default(),
            next: 0,
        };
        let ident = headers.next(&mut groups, "missing Vorbis identification header")?;
        if !is_header(&ident, 1) {
            return Err(invalid("not an Ogg Vorbis stream"));
        }
        let ident = guarded(|| read_header_ident(&ident))
            .ok_or_else(|| invalid("Vorbis identification header does not decode"))?
            .map_err(header_error)?;
        let comment = headers.next(&mut groups, "missing Vorbis comment header")?;
        if !is_header(&comment, 3) {
            return Err(invalid("second packet is not a Vorbis comment header"));
        }
        let setup = headers.next(&mut groups, "missing Vorbis setup header")?;
        if !is_header(&setup, 5) {
            return Err(invalid("third packet is not a Vorbis setup header"));
        }
        let long_modes = read_modes(&setup, ident.audio_channels, options.max_codebook_values)?;
        let setup = guarded(|| {
            read_header_setup(
                &setup,
                ident.audio_channels,
                (ident.blocksize_0, ident.blocksize_1),
            )
        })
        .ok_or_else(|| invalid("Vorbis setup header does not decode"))?
        .map_err(header_error)?;

        // libsndfile starts at the page after the setup header, and drops
        // audio packets that share its page.
        let Headers { group, .. } = headers;
        let next_packet = group.packets.len();
        let mut source = OggVorbisSource {
            groups,
            ident,
            setup,
            window: PreviousWindowRight::new(),
            options,
            current: group,
            next_packet,
            lookahead: None,
            last_page: false,
            last_granule: None,
            long_modes,
            sample_count: None,
            duration: 0,
            position: None,
            previous_block: None,
            early: VecDeque::new(),
            start_offset: None,
            limit: None,
            emitted: 0,
            hold: 0,
            held: VecDeque::new(),
            pcm: Vec::new(),
            finished: false,
        };
        if source.next_packet == source.current.packets.len() {
            // The audio starts on a new page, as the specification requires.
            match source.groups.next_group()? {
                Some(group) => source.current = group,
                None => source.finished = true,
            }
            source.next_packet = 0;
        }
        source.read_lookahead()?;
        Ok(source)
    }

    /// The block size of an audio packet, or `None` if libvorbis rejects
    /// the packet before decoding it: on its type, its mode, or its missing
    /// window flags.
    fn block_size(&self, packet: &[u8]) -> Option<u64> {
        let mut bits = BitReader {
            data: packet,
            position: 0,
        };
        if bits.read(1)? != 0 {
            return None;
        }
        let mode_bits = u32::BITS - (self.long_modes.len() as u32 - 1).leading_zeros();
        let long = *self.long_modes.get(bits.read(mode_bits)? as usize)?;
        if long {
            bits.skip(2)?;
        }
        let exponent = if long {
            self.ident.blocksize_1
        } else {
            self.ident.blocksize_0
        };
        Some(1 << exponent)
    }

    fn channels_usize(&self) -> usize {
        self.ident.audio_channels as usize
    }

    /// Reads the group after `current`, which tells whether `current` is
    /// the last.
    fn read_lookahead(&mut self) -> Result<(), Error> {
        self.lookahead = if self.current.end_of_stream || self.finished {
            None
        } else {
            self.groups.next_group()?
        };
        self.last_page = self.lookahead.is_none();
        self.update_limit();
        Ok(())
    }

    /// On the last page, sets the output length that libsndfile computes
    /// from the last granule position.
    fn update_limit(&mut self) {
        if !self.last_page {
            return;
        }
        let last = match self.current.granule {
            -1 => self.last_granule,
            granule => Some(granule),
        };
        let Some(last) = last.filter(|&granule| granule >= 0) else {
            return;
        };
        // Until the first page is known, which is then this page, the
        // offset is at least 0.
        let offset = self.start_offset.unwrap_or(0);
        self.limit = Some((last as i128 - offset).clamp(0, u64::MAX as i128) as u64);
    }

    /// Moves to the next group. Returns false at the end of the stream.
    fn advance(&mut self) -> Result<bool, Error> {
        if self.last_page {
            return Ok(false);
        }
        self.current = self
            .lookahead
            .take()
            .expect("a group after a page that is not the last");
        self.next_packet = 0;
        self.read_lookahead()?;
        Ok(true)
    }

    /// Decodes the next packet of `current` into `pcm`, through `held`.
    fn decode_next(&mut self) -> Result<(), Error> {
        let index = self.next_packet;
        self.next_packet += 1;
        let packet = &self.current.packets[index];
        if packet.after_gap {
            self.window = PreviousWindowRight::new();
            self.previous_block = None;
            self.early.clear();
            self.sample_count = None;
            self.duration = 0;
            self.position = None;
        }
        let Some(block) = self.block_size(&packet.data) else {
            // libvorbis skips the packet. The next packet number then skips
            // one, so libvorbis loses its position and count.
            self.sample_count = None;
            self.position = None;
            return Ok(());
        };
        let (ident, setup, window) = (&self.ident, &self.setup, &mut self.window);
        let decoded = guarded(|| {
            read_audio_packet_generic::<InterleavedSamples<f32>>(ident, setup, &packet.data, window)
        });
        let samples = match decoded {
            Some(Ok(decoded)) => decoded.samples,
            Some(Err(AudioReadError::BufferNotAddressable)) => {
                return Err(Error::LimitExceeded("Vorbis packet too large to decode"));
            }
            // libvorbis decodes what lewton refuses, for example window
            // flags that do not match the previous block. Silence then
            // replaces this packet and the next, whose overlap is lost.
            Some(Err(_)) | None => {
                self.window = PreviousWindowRight::new();
                Vec::new()
            }
        };
        let returned = self
            .previous_block
            .map_or(0, |previous| previous / 4 + block / 4);
        self.previous_block = Some(block);

        let overflow = self.options.overflow;
        self.early.extend(samples.iter().map(|&value| {
            let scaled = (value * 32767.0).round_ties_even();
            match overflow {
                // Float-to-int casts saturate.
                Overflow::Saturate => scaled as i16,
                Overflow::Wrap => scaled as i32 as i16,
            }
        }));
        let channels = self.channels_usize();
        // lewton returns no fewer frames than libvorbis, unless it refused a
        // packet.
        let missing = (returned as usize * channels).saturating_sub(self.early.len());
        self.early.extend(std::iter::repeat_n(0, missing));
        let frames = returned;
        let last_in_page = self.next_packet == self.current.packets.len();
        let end_of_stream = last_in_page && self.current.end_of_stream;
        let sample_count = self.sample_count.map_or(0, |count| count + frames);
        self.sample_count = Some(sample_count);
        self.duration += frames;
        if let Some(position) = &mut self.position {
            *position = position.saturating_add(frames as i64);
        }

        // As libvorbis: the first granule position trims the start of its
        // packet, or its end on the last page, and the last one trims the
        // end of its packet.
        let mut start = 0;
        let mut end = frames;
        let granule = self.current.granule;
        if last_in_page && granule != -1 {
            let excess = match self.position {
                None => sample_count as i128 - granule as i128,
                Some(position) if end_of_stream => position as i128 - granule as i128,
                Some(_) => 0,
            }
            .clamp(0, frames as i128) as u64;
            if end_of_stream {
                end -= excess;
            } else if self.position.is_none() {
                start = excess;
            }
            if self.start_offset.is_none() {
                let offset = granule as i128 - self.duration as i128;
                self.start_offset = Some(if granule >= 0 { offset.max(0) } else { offset });
                let hold = if granule >= 0 {
                    (-offset - start as i128).max(0) as u64
                } else {
                    0
                };
                if hold <= self.options.max_held_frames as u64 {
                    self.hold = hold as usize;
                }
            }
            self.position = Some(granule);
            self.last_granule = Some(granule);
            self.update_limit();
        }

        let (start, end) = (start as usize * channels, end as usize * channels);
        self.early.drain(..start);
        self.held.extend(self.early.drain(..end - start));
        self.early.drain(..frames as usize * channels - end);
        // Bounds what a stream with inconsistent window flags leaves behind.
        let excess = self
            .early
            .len()
            .saturating_sub((1usize << self.ident.blocksize_1) / 2 * channels);
        self.early.drain(..excess);
        self.release(self.hold);
        Ok(())
    }

    /// Moves held frames beyond the newest `keep` into `pcm`, up to the
    /// limit.
    fn release(&mut self, keep: usize) {
        let channels = self.channels_usize();
        let frames = (self.held.len() / channels).saturating_sub(keep);
        let allowed = match self.limit {
            Some(limit) => frames.min(limit.saturating_sub(self.emitted) as usize),
            None => frames,
        };
        self.pcm.extend(self.held.drain(..allowed * channels));
        self.held.drain(..(frames - allowed) * channels);
        self.emitted += allowed as u64;
    }
}

impl<R: Read> fmt::Debug for OggVorbisSource<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OggVorbisSource")
            .field("sample_rate", &self.ident.audio_sample_rate)
            .field("channels", &self.ident.audio_channels)
            .field("options", &self.options)
            .field("emitted", &self.emitted)
            .field("finished", &self.finished)
            .finish_non_exhaustive()
    }
}

impl<R: Read> PcmSource for OggVorbisSource<R> {
    fn sample_rate(&self) -> u32 {
        self.ident.audio_sample_rate
    }

    fn channels(&self) -> u16 {
        self.ident.audio_channels as u16
    }

    fn next_block(&mut self) -> Result<Option<&[i16]>, Error> {
        self.pcm.clear();
        while self.pcm.is_empty() {
            if self.finished {
                // A block at a time, so that `pcm` does not grow to `held`.
                let frames = self.held.len() / self.channels_usize();
                if frames == 0 {
                    break;
                }
                self.release(frames.saturating_sub(TAIL_FRAMES));
            } else if self.next_packet == self.current.packets.len() {
                self.finished = !self.advance()?;
            } else {
                self.decode_next()?;
            }
        }
        Ok((!self.pcm.is_empty()).then_some(&self.pcm[..]))
    }
}

/// Reads the header packets, which may share pages with each other and with
/// audio packets.
struct Headers {
    group: Group,
    next: usize,
}

impl Headers {
    fn next<R: Read>(
        &mut self,
        groups: &mut GroupReader<R>,
        missing: &'static str,
    ) -> Result<Vec<u8>, Error> {
        while self.next == self.group.packets.len() {
            self.group = groups.next_group()?.ok_or_else(|| invalid(missing))?;
            self.next = 0;
        }
        self.next += 1;
        let packet = std::mem::take(&mut self.group.packets[self.next - 1]);
        if packet.after_gap {
            return Err(invalid("missing Ogg page in the Vorbis headers"));
        }
        Ok(packet.data)
    }
}

/// Runs lewton code that can panic on crafted input, such as on an integer
/// overflow in a build with overflow checks.
fn guarded<T>(decode: impl FnOnce() -> T) -> Option<T> {
    catch_unwind(AssertUnwindSafe(decode)).ok()
}

fn is_header(packet: &[u8], kind: u8) -> bool {
    packet.len() >= 7 && packet[0] == kind && &packet[1..7] == b"vorbis"
}

fn header_error(err: HeaderReadError) -> Error {
    match err {
        HeaderReadError::UnsupportedVorbisVersion => {
            Error::Unsupported("Vorbis version other than 0".to_owned())
        }
        HeaderReadError::BufferNotAddressable => {
            Error::LimitExceeded("Vorbis header too large to decode")
        }
        other => Error::InvalidInput(format!("Vorbis: {other}")),
    }
}

/// Checks the codebook sizes of a setup header before lewton allocates
/// them, rejects what libvorbis rejects and lewton could panic on, and
/// returns the block flag of each mode.
fn read_modes(setup: &[u8], channels: u8, max_values: usize) -> Result<Vec<bool>, Error> {
    let truncated = || invalid("truncated Vorbis setup header");
    let mut bits = BitReader {
        data: &setup[7..],
        position: 0,
    };
    let count = bits.read(8).ok_or_else(truncated)? + 1;
    let mut total: u64 = 0;
    for _ in 0..count {
        if bits.read(24).ok_or_else(truncated)? != 0x56_4342 {
            return Err(invalid("missing Vorbis codebook sync pattern"));
        }
        let dimensions = bits.read(16).ok_or_else(truncated)?;
        let entries = bits.read(24).ok_or_else(truncated)?;
        total += entries as u64 * dimensions.max(1) as u64;
        if total > max_values as u64 {
            return Err(Error::LimitExceeded(
                "Vorbis codebooks larger than max_codebook_values",
            ));
        }
        if bits.read(1).ok_or_else(truncated)? == 0 {
            if bits.read(1).ok_or_else(truncated)? == 1 {
                for _ in 0..entries {
                    if bits.read(1).ok_or_else(truncated)? == 1 {
                        bits.skip(5).ok_or_else(truncated)?;
                    }
                }
            } else {
                bits.skip(5 * entries as u64).ok_or_else(truncated)?;
            }
        } else {
            let mut length = bits.read(5).ok_or_else(truncated)? + 1;
            let mut current = 0;
            while current < entries {
                if length > 32 {
                    return Err(invalid("Vorbis codeword longer than 32 bits"));
                }
                let width = u32::BITS - (entries - current).leading_zeros();
                current += bits.read(width).ok_or_else(truncated)?;
                if current > entries {
                    return Err(invalid("too many Vorbis codeword lengths"));
                }
                length += 1;
            }
        }
        let lookup = bits.read(4).ok_or_else(truncated)?;
        if lookup == 0 {
            continue;
        }
        if lookup > 2 {
            return Err(invalid("Vorbis codebook lookup type above 2"));
        }
        bits.skip(64).ok_or_else(truncated)?;
        let value_bits = bits.read(4).ok_or_else(truncated)? as u64 + 1;
        bits.skip(1).ok_or_else(truncated)?;
        let values = if lookup == 1 {
            if dimensions == 0 {
                return Err(invalid("Vorbis codebook with lookup and no dimensions"));
            }
            lookup1_values(entries, dimensions)
        } else {
            entries as u64 * dimensions as u64
        };
        bits.skip(values * value_bits).ok_or_else(truncated)?;
    }

    let mut read = |count| bits.read(count).ok_or_else(truncated);
    let malformed = || invalid("malformed Vorbis setup header");
    for _ in 0..read(6)? + 1 {
        if read(16)? != 0 {
            return Err(malformed());
        }
    }
    for _ in 0..read(6)? + 1 {
        match read(16)? {
            0 => {
                read(8)?;
                read(16)?;
                read(16)?;
                read(6)?;
                read(8)?;
                for _ in 0..read(4)? + 1 {
                    read(8)?;
                }
            }
            1 => {
                let classes: Vec<u32> = (0..read(5)?).map(|_| read(4)).collect::<Result<_, _>>()?;
                let mut dimensions = Vec::new();
                for _ in 0..classes.iter().max().map_or(0, |&max| max + 1) {
                    dimensions.push(read(3)? + 1);
                    let subclasses = read(2)?;
                    if subclasses > 0 {
                        read(8)?;
                    }
                    for _ in 0..1 << subclasses {
                        read(8)?;
                    }
                }
                read(2)?;
                let range_bits = read(4)?;
                for class in classes {
                    for _ in 0..dimensions[class as usize] {
                        read(range_bits)?;
                    }
                }
            }
            _ => return Err(malformed()),
        }
    }
    for _ in 0..read(6)? + 1 {
        if read(16)? > 2 {
            return Err(malformed());
        }
        read(24)?;
        read(24)?;
        read(24)?;
        let classifications = read(6)? + 1;
        read(8)?;
        let mut books = 0;
        for _ in 0..classifications {
            let low = read(3)?;
            let high = if read(1)? == 1 { read(5)? } else { 0 };
            books += (high << 3 | low).count_ones();
        }
        for _ in 0..books {
            read(8)?;
        }
    }
    let channel_bits = u8::BITS - (channels.max(1) - 1).leading_zeros();
    for _ in 0..read(6)? + 1 {
        if read(16)? != 0 {
            return Err(malformed());
        }
        let submaps = if read(1)? == 1 { read(4)? + 1 } else { 1 };
        if read(1)? == 1 {
            for _ in 0..2 * (read(8)? + 1) {
                read(channel_bits)?;
            }
        }
        if read(2)? != 0 {
            return Err(malformed());
        }
        if submaps > 1 {
            for _ in 0..channels {
                read(4)?;
            }
        }
        for _ in 0..submaps {
            read(24)?;
        }
    }
    let modes = read(6)? + 1;
    (0..modes)
        .map(|_| {
            let long = read(1)? == 1;
            read(16)?;
            read(16)?;
            read(8)?;
            Ok(long)
        })
        .collect()
}

/// The largest integer whose `dimensions`-th power is at most `entries`.
fn lookup1_values(entries: u32, dimensions: u32) -> u64 {
    let power = |base: u64| {
        (0..dimensions).try_fold(1u64, |product, _| {
            product.checked_mul(base).filter(|&p| p <= entries as u64)
        })
    };
    let mut root = (entries as f64).powf(1.0 / dimensions as f64) as u64;
    while root > 0 && power(root).is_none() {
        root -= 1;
    }
    while power(root + 1).is_some() {
        root += 1;
    }
    root
}

/// Reads a Vorbis bitstream: least significant bit first.
struct BitReader<'a> {
    data: &'a [u8],
    position: u64,
}

impl BitReader<'_> {
    fn read(&mut self, count: u32) -> Option<u32> {
        let mut value = 0;
        for i in 0..count {
            let byte = *self.data.get((self.position / 8) as usize)?;
            value |= ((byte >> (self.position % 8)) as u32 & 1) << i;
            self.position += 1;
        }
        Some(value)
    }

    fn skip(&mut self, count: u64) -> Option<()> {
        let position = self.position.checked_add(count)?;
        (position <= self.data.len() as u64 * 8).then(|| self.position = position)
    }
}
