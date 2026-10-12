//! AAC in MP4 (ISO base media file format), through Symphonia.

use std::collections::HashMap;
use std::fmt;
use std::io::Read;

use super::aac::{AacDecoder, MAX_FRAME_BYTES};
use super::{CountingReader, invalid};
use crate::{Error, PcmSource};

/// Largest box read whole that is not a sample table: track headers, sample
/// descriptions, edit lists, `iTunSMPB` items.
const MAX_HEADER_BYTES: u64 = 64 * 1024;

/// Most tracks whose fragment defaults (`trex`) are kept. Real files have a
/// handful.
const MAX_TRACK_DEFAULTS: usize = 1024;

/// Settings for [`Mp4AacSource`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Mp4Options {
    /// Largest MP4 metadata buffered: the sample tables of the AAC track, read
    /// before its media data, or each `moof` box of a fragmented file together
    /// with the sample list read from it. Bounds memory against crafted sizes.
    /// Other boxes, such as cover art, are read past without being buffered.
    /// The default, 64 MiB, holds the tables of several days of audio.
    pub max_table_bytes: usize,
}

impl Default for Mp4Options {
    fn default() -> Self {
        Mp4Options {
            max_table_bytes: 64 * 1024 * 1024,
        }
    }
}

/// Streams PCM out of the first AAC track of an MP4 file (`.m4a`, `.mp4`), such
/// as a Safari `MediaRecorder` recording.
///
/// Reads with plain `Read`, so an upload body can be decoded as it arrives.
/// Reads are buffered internally. This needs the `moov` box before the media
/// data, which holds for fragmented files (as recorders write) and for files
/// written with the `moov` box first ("fast start"). A file with its media data
/// first, as FFmpeg writes by default, is [`Error::Unsupported`]: it cannot be
/// decoded without holding or seeking back over all of its media data.
///
/// Memory is one frame and one decoded frame, plus the sample tables of the
/// AAC track, which a non-fragmented file stores before its media data (4
/// bytes per frame, about 700 KB per hour at 48 kHz), or the current `moof`
/// box of a fragmented one. Other tracks and metadata, such as cover art, are
/// read past without being buffered. Both are bounded by
/// [`Mp4Options::max_table_bytes`].
///
/// Trims the encoder priming and padding given by the track's edit list, or
/// without one by the `iTunSMPB` tag Apple encoders write. Handles AAC-LC in
/// mono and stereo; other profiles, channel layouts and encrypted tracks are
/// [`Error::Unsupported`]. Samples must be stored in file order, as muxers
/// write them.
///
/// A file cut between boxes is not an error: the samples present are decoded.
/// A cut inside a box is [`Error::InvalidInput`].
pub struct Mp4AacSource<R: Read> {
    reader: BoxReader<R>,
    decoder: AacDecoder,
    track_id: u32,
    defaults: HashMap<u32, TrackDefaults>,
    max_table_bytes: usize,
    table: Option<SampleTable>,
    /// Samples of the AAC track in the current `moof` box, as absolute file
    /// offsets and sizes, and the next one to read.
    fragment: Vec<(u64, u32)>,
    next_fragment_sample: usize,
    /// Set while inside an `mdat` box, to its end when not open-ended.
    mdat: Option<Option<u64>>,
    frame: Vec<u8>,
    finished: bool,
}

impl<R: Read> Mp4AacSource<R> {
    /// Reads up to the end of the `moov` box, with default options.
    ///
    /// # Errors
    ///
    /// As for [`with_options`](Self::with_options).
    pub fn new(reader: R) -> Result<Self, Error> {
        Self::with_options(reader, Mp4Options::default())
    }

    /// Reads up to the end of the `moov` box.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidInput`] when the input is not an MP4 file or its
    ///   boxes are malformed.
    /// - [`Error::Unsupported`] when the media data comes before the `moov`
    ///   box, when there is no AAC track, or for an encrypted track, a profile
    ///   other than AAC-LC, or more than two channels.
    /// - [`Error::LimitExceeded`] when the sample tables pass
    ///   [`Mp4Options::max_table_bytes`].
    pub fn with_options(reader: R, options: Mp4Options) -> Result<Self, Error> {
        let mut reader = BoxReader {
            input: CountingReader::new(reader, "truncated box"),
        };
        let movie = loop {
            let mp4_box = reader.next_box()?.ok_or_else(|| {
                if reader.position() == 0 {
                    invalid("empty input")
                } else {
                    invalid("no moov box")
                }
            })?;
            if !is_box_type(&mp4_box.kind) {
                return Err(invalid("not an MP4 file"));
            }
            match &mp4_box.kind {
                b"moov" => break read_movie(&mut reader, &mp4_box, options.max_table_bytes)?,
                b"mdat" if mp4_box.end != Some(reader.position()) => {
                    return Err(Error::Unsupported(
                        "MP4 media data before the moov box; write the file with \
                         the moov box first (fast start) or fragmented"
                            .into(),
                    ));
                }
                b"moof" => return Err(invalid("moof box before the moov box")),
                _ => reader.skip_box(&mp4_box)?,
            }
        };
        let track = movie.track;
        let mut decoder = AacDecoder::new(&track.config)?;
        let rate = decoder.sample_rate() as u128;
        let (start, end) = if let Some(edit) = track.edit {
            let start = edit.media_time as u128 * rate / track.timescale as u128;
            let end = (edit.duration > 0)
                .then(|| start + edit.duration as u128 * rate / movie.timescale as u128);
            let saturate = |frames: u128| u64::try_from(frames).unwrap_or(u64::MAX);
            (saturate(start), end.map(saturate))
        } else if let Some(gapless) = movie.gapless {
            let end = (gapless.valid > 0).then(|| gapless.priming.saturating_add(gapless.valid));
            (gapless.priming, end)
        } else {
            (0, None)
        };
        decoder.trim(start, end);

        Ok(Mp4AacSource {
            reader,
            decoder,
            track_id: track.id,
            defaults: movie.defaults,
            max_table_bytes: options.max_table_bytes,
            table: track.table,
            fragment: Vec::new(),
            next_fragment_sample: 0,
            mdat: None,
            frame: Vec::new(),
            finished: false,
        })
    }

    /// The offset and size of the next sample of the AAC track, reading
    /// `moof` boxes as needed. `None` at the end of the input.
    fn next_sample(&mut self) -> Result<Option<(u64, u32)>, Error> {
        if let Some(table) = &mut self.table {
            match table.next()? {
                Some(sample) => return Ok(Some(sample)),
                None => self.table = None,
            }
        }
        loop {
            if let Some(&sample) = self.fragment.get(self.next_fragment_sample) {
                self.next_fragment_sample += 1;
                return Ok(Some(sample));
            }
            self.leave_mdat()?;
            let Some(mp4_box) = self.reader.next_box()? else {
                return Ok(None);
            };
            match &mp4_box.kind {
                b"moof" => {
                    // Freed first, so the previous sample list and this moof
                    // box never take max_table_bytes each.
                    self.fragment = Vec::new();
                    self.next_fragment_sample = 0;
                    let moof = self
                        .reader
                        .read_body(&mp4_box, self.max_table_bytes as u64)?;
                    read_fragment(
                        &moof,
                        mp4_box.start,
                        self.track_id,
                        &self.defaults,
                        self.max_table_bytes - moof.len(),
                        &mut self.fragment,
                    )?;
                }
                _ => self.reader.skip_box(&mp4_box)?,
            }
        }
    }

    /// Reads the sample at `offset` into `frame`, moving through `mdat` boxes.
    /// Returns false when the input ends between boxes first.
    fn read_sample(&mut self, offset: u64, size: u32) -> Result<bool, Error> {
        if size as usize > MAX_FRAME_BYTES {
            return Err(invalid("AAC frame larger than the format allows"));
        }
        loop {
            if offset < self.reader.position() {
                return Err(Error::Unsupported(
                    "MP4 samples stored out of file order".into(),
                ));
            }
            if let Some(end) = self.mdat {
                let sample_end = offset
                    .checked_add(size as u64)
                    .ok_or_else(|| invalid("sample offset overflows"))?;
                if end.is_none_or(|end| sample_end <= end) {
                    self.reader.skip_bytes(offset - self.reader.position())?;
                    self.frame.resize(size as usize, 0);
                    self.reader.read_exact(&mut self.frame)?;
                    return Ok(true);
                }
                if end.is_some_and(|end| offset < end) {
                    return Err(invalid("sample crosses the end of its mdat box"));
                }
            }
            self.leave_mdat()?;
            let Some(mp4_box) = self.reader.next_box()? else {
                return Ok(false);
            };
            match &mp4_box.kind {
                b"mdat" => self.mdat = Some(mp4_box.end),
                b"moof" | b"moov" => return Err(invalid("sample outside the media data")),
                _ => self.reader.skip_box(&mp4_box)?,
            }
        }
    }

    /// Skips the rest of the current `mdat` box, if any.
    fn leave_mdat(&mut self) -> Result<(), Error> {
        match self.mdat.take() {
            Some(Some(end)) => self.reader.skip_bytes(end - self.reader.position()),
            // An open-ended mdat runs to the end of the input.
            Some(None) => self.reader.input.skip_to_end(),
            None => Ok(()),
        }
    }
}

impl<R: Read> fmt::Debug for Mp4AacSource<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mp4AacSource")
            .field("decoder", &self.decoder)
            .field("track_id", &self.track_id)
            .field("finished", &self.finished)
            .finish_non_exhaustive()
    }
}

impl<R: Read> PcmSource for Mp4AacSource<R> {
    fn sample_rate(&self) -> u32 {
        self.decoder.sample_rate()
    }

    fn channels(&self) -> u16 {
        self.decoder.channels()
    }

    fn next_block(&mut self) -> Result<Option<&[i16]>, Error> {
        while !self.finished {
            let Some((offset, size)) = self.next_sample()? else {
                self.finished = true;
                break;
            };
            if size == 0 {
                continue;
            }
            if !self.read_sample(offset, size)? {
                self.finished = true;
                break;
            }
            if self.decoder.decode(&self.frame)? {
                return Ok(Some(self.decoder.output()));
            }
        }
        Ok(None)
    }
}

/// What the `moov` box says about the chosen track and its fragments.
struct Movie {
    timescale: u32,
    track: Track,
    defaults: HashMap<u32, TrackDefaults>,
    gapless: Option<Gapless>,
}

struct Track {
    id: u32,
    /// Media timescale, from `mdhd`.
    timescale: u32,
    config: Vec<u8>,
    edit: Option<Edit>,
    table: Option<SampleTable>,
}

/// The first edit of an edit list that shows media.
#[derive(Clone, Copy)]
struct Edit {
    /// In the movie timescale; 0 when unknown, as in fragmented files.
    duration: u64,
    /// In the media timescale.
    media_time: u64,
}

/// Encoder priming and length from Apple's `iTunSMPB` tag, in samples.
#[derive(Clone, Copy)]
struct Gapless {
    priming: u64,
    valid: u64,
}

/// Default sample size of a track's fragments, from `trex`.
#[derive(Clone, Copy, Default)]
struct TrackDefaults {
    sample_size: u32,
}

fn read_movie<R: Read>(
    reader: &mut BoxReader<R>,
    moov: &Mp4Box,
    max_table_bytes: usize,
) -> Result<Movie, Error> {
    let end = moov.end.ok_or_else(|| invalid("open-ended moov box"))?;
    let mut timescale = None;
    let mut track = None;
    let mut other_codec = None;
    let mut defaults = HashMap::new();
    let mut gapless = None;
    while reader.position() < end {
        let child = reader.next_child(end)?;
        match &child.kind {
            b"mvhd" => {
                let body = reader.read_body(&child, MAX_HEADER_BYTES)?;
                // Version 1 widens the creation and modification times.
                let at = if body.first() == Some(&1) { 20 } else { 12 };
                timescale = Some(be_u32(&body, at)?);
            }
            b"trak" if track.is_none() => match read_track(reader, &child, max_table_bytes)? {
                TrackKind::Aac(aac) => track = Some(aac),
                TrackKind::OtherAudio(codec) => {
                    other_codec.get_or_insert(codec);
                }
                TrackKind::Other => {}
            },
            b"mvex" => read_track_defaults(reader, &child, &mut defaults)?,
            b"udta" | b"meta" => {
                if let Some(found) = read_gapless(reader, &child)? {
                    gapless = Some(found);
                }
            }
            _ => reader.skip_box(&child)?,
        }
    }
    let track = track.ok_or_else(|| match other_codec {
        Some(TrackCodec::Encrypted) => Error::Unsupported("encrypted AAC track".into()),
        Some(TrackCodec::Other(codec)) => {
            Error::Unsupported(format!("no AAC track (found {codec})"))
        }
        None => invalid("no audio track"),
    })?;
    let timescale = timescale
        .filter(|&timescale| timescale != 0)
        .ok_or_else(|| invalid("missing movie timescale"))?;
    Ok(Movie {
        timescale,
        track,
        defaults,
        gapless,
    })
}

enum TrackKind {
    Aac(Track),
    OtherAudio(TrackCodec),
    Other,
}

enum TrackCodec {
    Encrypted,
    Other(String),
}

/// Reads a `trak` box, keeping the sample tables only for an AAC track.
fn read_track<R: Read>(
    reader: &mut BoxReader<R>,
    trak: &Mp4Box,
    max_table_bytes: usize,
) -> Result<TrackKind, Error> {
    let mut state = TrackState::default();
    read_track_boxes(reader, trak, max_table_bytes, &mut state)?;
    if !state.sound {
        return Ok(TrackKind::Other);
    }
    let config = match state.codec {
        Some(Codec::Aac(config)) => config,
        Some(Codec::Encrypted) => return Ok(TrackKind::OtherAudio(TrackCodec::Encrypted)),
        Some(Codec::Other(name)) => return Ok(TrackKind::OtherAudio(TrackCodec::Other(name))),
        None => return Err(invalid("audio track without a sample description")),
    };
    let timescale = state
        .timescale
        .filter(|&timescale| timescale != 0)
        .ok_or_else(|| invalid("missing media timescale"))?;
    let id = state.id.ok_or_else(|| invalid("missing track header"))?;
    let table = state.tables.into_table()?;
    Ok(TrackKind::Aac(Track {
        id,
        timescale,
        config,
        edit: state.edit,
        table,
    }))
}

#[derive(Default)]
struct TrackState {
    id: Option<u32>,
    timescale: Option<u32>,
    sound: bool,
    codec: Option<Codec>,
    edit: Option<Edit>,
    tables: RawTables,
}

enum Codec {
    Aac(Vec<u8>),
    Encrypted,
    Other(String),
}

/// Walks the boxes of a track, descending into the containers on the way to
/// the sample tables.
fn read_track_boxes<R: Read>(
    reader: &mut BoxReader<R>,
    parent: &Mp4Box,
    max_table_bytes: usize,
    state: &mut TrackState,
) -> Result<(), Error> {
    let end = parent.end.ok_or_else(|| invalid("open-ended box"))?;
    while reader.position() < end {
        let child = reader.next_child(end)?;
        match &child.kind {
            kind if leads_to_tables(&parent.kind, kind) => {
                read_track_boxes(reader, &child, max_table_bytes, state)?;
            }
            b"tkhd" => {
                let body = reader.read_body(&child, MAX_HEADER_BYTES)?;
                let at = if body.first() == Some(&1) { 20 } else { 12 };
                state.id = Some(be_u32(&body, at)?);
            }
            b"mdhd" => {
                let body = reader.read_body(&child, MAX_HEADER_BYTES)?;
                let at = if body.first() == Some(&1) { 20 } else { 12 };
                state.timescale = Some(be_u32(&body, at)?);
            }
            // QuickTime adds a data handler (`dhlr`) in `minf`, which says
            // nothing about the media.
            b"hdlr" if &parent.kind == b"mdia" => {
                let body = reader.read_body(&child, MAX_HEADER_BYTES)?;
                state.sound = body.get(8..12) == Some(b"soun");
            }
            b"elst" => {
                let body = reader.read_body(&child, MAX_HEADER_BYTES)?;
                state.edit = first_edit(&body)?;
            }
            b"stsd" if state.sound => {
                let body = reader.read_body(&child, MAX_HEADER_BYTES)?;
                state.codec = Some(sample_description(&body)?);
            }
            b"stsz" | b"stz2" | b"stco" | b"co64" | b"stsc"
                if matches!(state.codec, Some(Codec::Aac(_))) =>
            {
                let budget = max_table_bytes.saturating_sub(state.tables.bytes());
                if child.size().is_none_or(|size| size > budget as u64) {
                    return Err(Error::LimitExceeded(
                        "MP4 sample tables larger than max_table_bytes",
                    ));
                }
                let body = reader.read_body(&child, u64::MAX)?;
                state.tables.set(&child.kind, body);
            }
            _ => reader.skip_box(&child)?,
        }
    }
    Ok(())
}

/// Whether `child` is a container on the way from `trak` to the sample tables
/// or the edit list. Entering no other box bounds the recursion.
fn leads_to_tables(parent: &[u8; 4], child: &[u8; 4]) -> bool {
    matches!(
        (parent, child),
        (b"trak", b"mdia" | b"edts") | (b"mdia", b"minf") | (b"minf", b"stbl")
    )
}

/// The first edit showing media, from an `elst` body.
fn first_edit(body: &[u8]) -> Result<Option<Edit>, Error> {
    let wide = body.first() == Some(&1);
    let count = be_u32(body, 4)? as usize;
    let entry_len = if wide { 20 } else { 12 };
    for index in 0..count {
        let at = 8 + index * entry_len;
        let (duration, media_time) = if wide {
            (be_u64(body, at)?, be_u64(body, at + 8)? as i64)
        } else {
            (
                be_u32(body, at)? as u64,
                be_u32(body, at + 4)? as i32 as i64,
            )
        };
        // A media time of -1 marks an empty edit, which shows nothing.
        if media_time >= 0 {
            return Ok(Some(Edit {
                duration,
                media_time: media_time as u64,
            }));
        }
    }
    Ok(None)
}

/// Reads the first sample entry of an `stsd` body.
fn sample_description(body: &[u8]) -> Result<Codec, Error> {
    let entry = body.get(8..).ok_or_else(|| invalid("truncated stsd box"))?;
    let size = be_u32(entry, 0)? as usize;
    let kind = entry
        .get(4..8)
        .ok_or_else(|| invalid("truncated stsd box"))?;
    let entry = entry
        .get(8..size.max(8))
        .ok_or_else(|| invalid("truncated sample entry"))?;
    match kind {
        b"mp4a" => {}
        b"enca" => return Ok(Codec::Encrypted),
        other => return Ok(Codec::Other(String::from_utf8_lossy(other).into_owned())),
    }
    // Audio sample entry: 28 bytes, plus 16 or 36 more for QuickTime sound
    // description versions 1 and 2.
    let extra = match be_u16(entry, 8)? {
        1 => 16,
        2 => 36,
        _ => 0,
    };
    let children = entry
        .get(28 + extra..)
        .ok_or_else(|| invalid("truncated sample entry"))?;
    let esds = find_child(children, b"esds")
        .or_else(|| find_child(children, b"wave").and_then(|wave| find_child(wave, b"esds")))
        .ok_or_else(|| invalid("mp4a sample entry without an esds box"))?;
    audio_specific_config(esds)
}

/// The body of the first child box of `kind` in `boxes`.
fn find_child<'a>(mut boxes: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    while boxes.len() >= 8 {
        let size = u32::from_be_bytes(boxes[..4].try_into().ok()?) as usize;
        if size < 8 || size > boxes.len() {
            return None;
        }
        if &boxes[4..8] == kind {
            return Some(&boxes[8..size]);
        }
        boxes = &boxes[size..];
    }
    None
}

/// Extracts the `AudioSpecificConfig` from an `esds` body.
fn audio_specific_config(esds: &[u8]) -> Result<Codec, Error> {
    let bad = || invalid("malformed esds box");
    // Version and flags, then the ES descriptor.
    let mut data = esds.get(4..).ok_or_else(bad)?;
    let (tag, es) = descriptor(&mut data).ok_or_else(bad)?;
    if tag != 0x03 {
        return Err(bad());
    }
    let flags = *es.get(2).ok_or_else(bad)?;
    let mut at = 3;
    if flags & 0x80 != 0 {
        at += 2;
    }
    if flags & 0x40 != 0 {
        at += 1 + *es.get(at).ok_or_else(bad)? as usize;
    }
    if flags & 0x20 != 0 {
        at += 2;
    }
    let mut es = es.get(at..).ok_or_else(bad)?;
    let (tag, config) = descriptor(&mut es).ok_or_else(bad)?;
    if tag != 0x04 {
        return Err(bad());
    }
    let object_type = *config.first().ok_or_else(bad)?;
    // MPEG-4 audio, and the MPEG-2 AAC profiles.
    if !matches!(object_type, 0x40 | 0x66..=0x68) {
        return Ok(Codec::Other(format!("object type {object_type:#04x}")));
    }
    let mut specific = config.get(13..).ok_or_else(bad)?;
    let (tag, asc) = descriptor(&mut specific).ok_or_else(bad)?;
    if tag != 0x05 {
        return Err(bad());
    }
    Ok(Codec::Aac(asc.to_vec()))
}

/// Reads an MPEG-4 descriptor: a tag and a size of up to four 7-bit bytes.
fn descriptor<'a>(data: &mut &'a [u8]) -> Option<(u8, &'a [u8])> {
    let (&tag, mut rest) = data.split_first()?;
    let mut size = 0usize;
    for _ in 0..4 {
        let (&byte, tail) = rest.split_first()?;
        rest = tail;
        size = size << 7 | (byte & 0x7F) as usize;
        if byte & 0x80 == 0 {
            break;
        }
    }
    let body = rest.get(..size)?;
    *data = &rest[size..];
    Some((tag, body))
}

/// Reads the `trex` boxes of `mvex`.
fn read_track_defaults<R: Read>(
    reader: &mut BoxReader<R>,
    mvex: &Mp4Box,
    defaults: &mut HashMap<u32, TrackDefaults>,
) -> Result<(), Error> {
    let end = mvex.end.ok_or_else(|| invalid("open-ended mvex box"))?;
    while reader.position() < end {
        let child = reader.next_child(end)?;
        if &child.kind != b"trex" {
            reader.skip_box(&child)?;
            continue;
        }
        let body = reader.read_body(&child, MAX_HEADER_BYTES)?;
        let track_id = be_u32(&body, 4)?;
        let sample_size = be_u32(&body, 16)?;
        if defaults.len() == MAX_TRACK_DEFAULTS && !defaults.contains_key(&track_id) {
            return Err(Error::Unsupported(format!(
                "more than {MAX_TRACK_DEFAULTS} fragmented tracks"
            )));
        }
        defaults.insert(track_id, TrackDefaults { sample_size });
    }
    Ok(())
}

/// Looks for Apple's `iTunSMPB` tag in `udta` or `meta`, reading past every
/// other item, such as cover art.
fn read_gapless<R: Read>(
    reader: &mut BoxReader<R>,
    parent: &Mp4Box,
) -> Result<Option<Gapless>, Error> {
    let end = parent.end.ok_or_else(|| invalid("open-ended box"))?;
    let mut first = None;
    if &parent.kind == b"meta" {
        // ISO `meta` is a full box. QuickTime's is not: what would be its
        // version and flags is the size of its first child.
        if end - reader.position() < 4 {
            return Err(invalid("truncated meta box"));
        }
        let mut version = [0u8; 4];
        reader.read_exact(&mut version)?;
        if version != [0; 4] {
            let start = reader.position() - 4;
            let mut kind = [0u8; 4];
            reader.read_exact(&mut kind)?;
            let size = u32::from_be_bytes(version) as u64;
            if size < 8 || start + size > end {
                return Err(invalid("malformed meta box"));
            }
            first = Some(Mp4Box {
                kind,
                start,
                end: Some(start + size),
            });
        }
    }
    let mut found = None;
    while first.is_some() || reader.position() < end {
        let child = match first.take() {
            Some(child) => child,
            None => reader.next_child(end)?,
        };
        match &child.kind {
            // Entering only these boxes bounds the recursion.
            b"meta" | b"ilst"
                if matches!(
                    (&parent.kind, &child.kind),
                    (b"udta", b"meta") | (b"meta", b"ilst")
                ) =>
            {
                found = read_gapless(reader, &child)?.or(found);
            }
            b"----"
                if &parent.kind == b"ilst"
                    && child.size().is_some_and(|size| size <= MAX_HEADER_BYTES) =>
            {
                let item = reader.read_body(&child, MAX_HEADER_BYTES)?;
                found = parse_itunsmpb(&item).or(found);
            }
            _ => reader.skip_box(&child)?,
        }
    }
    Ok(found)
}

/// Parses a `----` item named `iTunSMPB`: hexadecimal fields, of which the
/// second is the priming, the third the padding, and the fourth the length.
fn parse_itunsmpb(item: &[u8]) -> Option<Gapless> {
    let name = find_child(item, b"name")?;
    if name.get(4..)? != b"iTunSMPB" {
        return None;
    }
    // Data box: version and flags, locale, then the text.
    let text = std::str::from_utf8(find_child(item, b"data")?.get(8..)?).ok()?;
    let fields: Vec<u64> = text
        .split_ascii_whitespace()
        .take(4)
        .map(|field| u64::from_str_radix(field, 16).ok())
        .collect::<Option<_>>()?;
    Some(Gapless {
        priming: *fields.get(1)?,
        valid: *fields.get(3)?,
    })
}

/// The raw sample table boxes of the AAC track.
#[derive(Default)]
struct RawTables {
    sizes: Option<Vec<u8>>,
    compact_sizes: bool,
    chunk_offsets: Option<(Vec<u8>, bool)>,
    chunks: Option<Vec<u8>>,
}

impl RawTables {
    fn bytes(&self) -> usize {
        self.sizes.as_ref().map_or(0, Vec::len)
            + self
                .chunk_offsets
                .as_ref()
                .map_or(0, |(offsets, _)| offsets.len())
            + self.chunks.as_ref().map_or(0, Vec::len)
    }

    fn set(&mut self, kind: &[u8; 4], body: Vec<u8>) {
        match kind {
            b"stsz" => self.sizes = Some(body),
            b"stz2" => self.compact_sizes = true,
            b"stco" => self.chunk_offsets = Some((body, false)),
            b"co64" => self.chunk_offsets = Some((body, true)),
            _ => self.chunks = Some(body),
        }
    }

    /// The table, or `None` when the track has no samples outside fragments.
    fn into_table(self) -> Result<Option<SampleTable>, Error> {
        if self.compact_sizes {
            return Err(Error::Unsupported("compact sample sizes (stz2)".into()));
        }
        let sizes = self.sizes.ok_or_else(|| invalid("missing stsz box"))?;
        let constant_size = be_u32(&sizes, 4)?;
        let count = be_u32(&sizes, 8)?;
        if count == 0 {
            return Ok(None);
        }
        if constant_size == 0 && (sizes.len() - 12) / 4 < count as usize {
            return Err(invalid("truncated stsz box"));
        }
        let (offsets, wide) = self
            .chunk_offsets
            .ok_or_else(|| invalid("missing stco box"))?;
        let chunks = self.chunks.ok_or_else(|| invalid("missing stsc box"))?;
        let chunk_count = be_u32(&offsets, 4)?;
        let width = if wide { 8 } else { 4 };
        if (offsets.len() - 8) / width < chunk_count as usize {
            return Err(invalid("truncated stco box"));
        }
        let run_count = be_u32(&chunks, 4)?;
        if (chunks.len() - 8) / 12 < run_count as usize || run_count == 0 {
            return Err(invalid("malformed stsc box"));
        }
        if be_u32(&chunks, 8)? != 1 {
            return Err(invalid("stsc box not starting at the first chunk"));
        }
        Ok(Some(SampleTable {
            sizes,
            constant_size,
            count,
            offsets,
            wide,
            chunk_count,
            chunks,
            run_count,
            sample: 0,
            chunk: 0,
            run: 0,
            left_in_chunk: 0,
            offset: 0,
        }))
    }
}

/// Walks a non-fragmented sample table in storage order.
struct SampleTable {
    sizes: Vec<u8>,
    constant_size: u32,
    count: u32,
    offsets: Vec<u8>,
    wide: bool,
    chunk_count: u32,
    chunks: Vec<u8>,
    run_count: u32,
    sample: u32,
    /// The next chunk to start, 0-based.
    chunk: u32,
    run: u32,
    left_in_chunk: u32,
    offset: u64,
}

impl SampleTable {
    fn next(&mut self) -> Result<Option<(u64, u32)>, Error> {
        if self.sample == self.count {
            return Ok(None);
        }
        while self.left_in_chunk == 0 {
            if self.chunk == self.chunk_count {
                return Err(invalid("sample table has more samples than chunks hold"));
            }
            // Runs give a first chunk (1-based) and a sample count per chunk.
            while self.run + 1 < self.run_count
                && self.chunk + 1 >= be_u32(&self.chunks, 8 + (self.run as usize + 1) * 12)?
            {
                self.run += 1;
            }
            self.left_in_chunk = be_u32(&self.chunks, 12 + self.run as usize * 12)?;
            self.offset = if self.wide {
                be_u64(&self.offsets, 8 + self.chunk as usize * 8)?
            } else {
                be_u32(&self.offsets, 8 + self.chunk as usize * 4)? as u64
            };
            self.chunk += 1;
        }
        let size = if self.constant_size != 0 {
            self.constant_size
        } else {
            be_u32(&self.sizes, 12 + self.sample as usize * 4)?
        };
        let sample = (self.offset, size);
        self.offset = self
            .offset
            .checked_add(size as u64)
            .ok_or_else(|| invalid("sample offset overflows"))?;
        self.sample += 1;
        self.left_in_chunk -= 1;
        Ok(Some(sample))
    }
}

/// Collects the samples of `track_id` from a `moof` body starting at file
/// offset `moof_start`, in at most `budget` bytes.
fn read_fragment(
    moof: &[u8],
    moof_start: u64,
    track_id: u32,
    defaults: &HashMap<u32, TrackDefaults>,
    budget: usize,
    samples: &mut Vec<(u64, u32)>,
) -> Result<(), Error> {
    // A trun box without per-sample fields can claim any number of samples
    // in a few bytes, so the count is bounded like the tables.
    let max_samples = budget / size_of::<(u64, u32)>();
    let mut total = 0usize;
    // Without an explicit base, a track fragment's data follows the previous
    // one's, starting from the moof box.
    let mut data_end = moof_start;
    for traf in children(moof) {
        let (kind, traf) = traf?;
        if &kind != b"traf" {
            continue;
        }
        let tfhd = children(traf)
            .find_map(|child| match child {
                Ok((kind, body)) if &kind == b"tfhd" => Some(Ok(body)),
                Ok(_) => None,
                Err(err) => Some(Err(err)),
            })
            .ok_or_else(|| invalid("traf box without a tfhd box"))??;
        let flags = be_u32(tfhd, 0)? & 0xFF_FFFF;
        let traf_track = be_u32(tfhd, 4)?;
        let mut at = 8;
        let base = if flags & 0x1 != 0 {
            at += 8;
            be_u64(tfhd, 8)?
        } else if flags & 0x2_0000 != 0 {
            moof_start
        } else {
            data_end
        };
        if flags & 0x2 != 0 {
            at += 4;
        }
        if flags & 0x8 != 0 {
            at += 4;
        }
        let default_size = if flags & 0x10 != 0 {
            be_u32(tfhd, at)?
        } else {
            defaults
                .get(&traf_track)
                .copied()
                .unwrap_or_default()
                .sample_size
        };

        let mut next = base;
        for child in children(traf) {
            let (kind, trun) = child?;
            if &kind != b"trun" {
                continue;
            }
            let flags = be_u32(trun, 0)? & 0xFF_FFFF;
            let count = be_u32(trun, 4)?;
            total = total.saturating_add(count as usize);
            if total > max_samples {
                return Err(Error::LimitExceeded(
                    "MP4 fragment with more samples than max_table_bytes holds",
                ));
            }
            let mut at = 8;
            if flags & 0x1 != 0 {
                let data_offset = be_u32(trun, at)? as i32 as i64;
                next = base
                    .checked_add_signed(data_offset)
                    .ok_or_else(|| invalid("trun data offset out of range"))?;
                at += 4;
            }
            if flags & 0x4 != 0 {
                at += 4;
            }
            let fields = [0x100, 0x200, 0x400, 0x800];
            let entry_len = fields.iter().filter(|&&field| flags & field != 0).count() * 4;
            if entry_len == 0 {
                // Without per-sample fields the count costs no input bytes, so
                // the run is skipped in one step unless it holds audio to read.
                let end = (count as u64)
                    .checked_mul(default_size as u64)
                    .and_then(|len| next.checked_add(len))
                    .ok_or_else(|| invalid("trun data out of range"))?;
                if traf_track == track_id && default_size > 0 {
                    samples.reserve_exact(count as usize);
                    samples.extend(
                        (next..end)
                            .step_by(default_size as usize)
                            .map(|offset| (offset, default_size)),
                    );
                }
                next = end;
                continue;
            }
            if trun.len().saturating_sub(at) / entry_len < count as usize {
                return Err(invalid("truncated trun box"));
            }
            if traf_track == track_id {
                samples.reserve_exact(count as usize);
            }
            for _ in 0..count {
                if flags & 0x100 != 0 {
                    at += 4;
                }
                let size = if flags & 0x200 != 0 {
                    let size = be_u32(trun, at)?;
                    at += 4;
                    size
                } else {
                    default_size
                };
                at += 4 * [0x400, 0x800]
                    .iter()
                    .filter(|&&field| flags & field != 0)
                    .count();
                if traf_track == track_id {
                    samples.push((next, size));
                }
                next = next
                    .checked_add(size as u64)
                    .ok_or_else(|| invalid("trun data out of range"))?;
            }
        }
        data_end = next;
    }
    Ok(())
}

/// Iterates over the child boxes of a box body held in memory.
fn children(mut body: &[u8]) -> impl Iterator<Item = Result<([u8; 4], &[u8]), Error>> {
    std::iter::from_fn(move || {
        if body.is_empty() {
            return None;
        }
        let parsed = (|| {
            let size = be_u32(body, 0)? as usize;
            let kind: [u8; 4] = body
                .get(4..8)
                .ok_or_else(|| invalid("truncated box"))?
                .try_into()
                .unwrap();
            let (header, size) = match size {
                1 => (
                    16,
                    usize::try_from(be_u64(body, 8)?)
                        .map_err(|_| invalid("box overruns its parent"))?,
                ),
                _ => (8, size),
            };
            if size < header || size > body.len() {
                return Err(invalid("box overruns its parent"));
            }
            let child = &body[header..size];
            body = &body[size..];
            Ok((kind, child))
        })();
        if parsed.is_err() {
            body = &[];
        }
        Some(parsed)
    })
}

fn is_box_type(kind: &[u8; 4]) -> bool {
    kind.iter()
        .all(|&byte| byte.is_ascii_graphic() || byte == b' ')
}

fn be_u16(data: &[u8], at: usize) -> Result<u16, Error> {
    let bytes = data
        .get(at..at + 2)
        .ok_or_else(|| invalid("truncated box"))?;
    Ok(u16::from_be_bytes(bytes.try_into().unwrap()))
}

fn be_u32(data: &[u8], at: usize) -> Result<u32, Error> {
    let bytes = data
        .get(at..at + 4)
        .ok_or_else(|| invalid("truncated box"))?;
    Ok(u32::from_be_bytes(bytes.try_into().unwrap()))
}

fn be_u64(data: &[u8], at: usize) -> Result<u64, Error> {
    let bytes = data
        .get(at..at + 8)
        .ok_or_else(|| invalid("truncated box"))?;
    Ok(u64::from_be_bytes(bytes.try_into().unwrap()))
}

/// A box header.
#[derive(Debug, Clone, Copy)]
struct Mp4Box {
    kind: [u8; 4],
    start: u64,
    /// Offset of the end of the box; `None` when it runs to the end of the
    /// input.
    end: Option<u64>,
}

impl Mp4Box {
    fn size(&self) -> Option<u64> {
        self.end.map(|end| end - self.start)
    }
}

/// Reads box headers and bodies, tracking the offset.
struct BoxReader<R> {
    input: CountingReader<R>,
}

impl<R: Read> BoxReader<R> {
    fn position(&self) -> u64 {
        self.input.position()
    }

    /// Reads the next box header. Returns `None` at the end of the input when
    /// it falls between boxes.
    fn next_box(&mut self) -> Result<Option<Mp4Box>, Error> {
        let start = self.position();
        let mut header = [0u8; 8];
        match self.input.read_full(&mut header)? {
            0 => return Ok(None),
            8 => {}
            _ => return Err(invalid("truncated box header")),
        }
        let kind: [u8; 4] = header[4..].try_into().unwrap();
        let end = match u32::from_be_bytes(header[..4].try_into().unwrap()) {
            0 => None,
            1 => {
                let mut large = [0u8; 8];
                self.input.read_exact(&mut large)?;
                Some(
                    start
                        .checked_add(u64::from_be_bytes(large))
                        .ok_or_else(|| invalid("box size overflows"))?,
                )
            }
            size => Some(start + size as u64),
        };
        if end.is_some_and(|end| end < self.position()) {
            return Err(invalid("box smaller than its header"));
        }
        Ok(Some(Mp4Box { kind, start, end }))
    }

    /// Reads the next child of a box ending at `parent_end`.
    fn next_child(&mut self, parent_end: u64) -> Result<Mp4Box, Error> {
        let child = self.next_box()?.ok_or_else(|| invalid("truncated box"))?;
        match child.end {
            Some(end) if end <= parent_end => Ok(child),
            _ => Err(invalid("box overruns its parent")),
        }
    }

    /// Reads the rest of a box, after its header.
    fn read_body(&mut self, mp4_box: &Mp4Box, max: u64) -> Result<Vec<u8>, Error> {
        let len = mp4_box
            .end
            .map(|end| end - self.position())
            .filter(|&len| len <= max)
            .and_then(|len| usize::try_from(len).ok())
            .ok_or_else(|| invalid("oversized box"))?;
        let mut body = vec![0; len];
        self.input.read_exact(&mut body)?;
        Ok(body)
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), Error> {
        self.input.read_exact(buf)
    }

    fn skip_box(&mut self, mp4_box: &Mp4Box) -> Result<(), Error> {
        let end = mp4_box.end.ok_or_else(|| invalid("open-ended box"))?;
        self.skip_bytes(end - self.position())
    }

    fn skip_bytes(&mut self, len: u64) -> Result<(), Error> {
        self.input.skip(len)
    }
}
