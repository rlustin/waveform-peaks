//! MP4 (ISO base media file format), with AAC and MPEG audio through Symphonia
//! and Opus through libopus.

use std::collections::HashMap;
use std::fmt;
use std::io::{Read, Seek};
#[cfg(feature = "opus")]
use std::sync::{Mutex, PoisonError};

#[cfg(feature = "aac")]
use super::aac::AacDecoder;
#[cfg(feature = "mp3")]
use super::mpa::{self, MpaDecoder};
#[cfg(feature = "opus")]
use super::opus::{End, OpusDecoder, OpusHead, OpusOptions, to_decode_rate};
use super::{CountingReader, invalid};
use crate::{Error, PcmSource};

/// Largest box read whole that is not a sample table: track headers, sample
/// descriptions, edit lists, `iTunSMPB` items.
const MAX_HEADER_BYTES: u64 = 64 * 1024;

/// Most tracks whose fragment defaults (`trex`) are kept. Real files have a
/// handful.
const MAX_TRACK_DEFAULTS: usize = 1024;

/// Largest sample read. A stereo AAC-LC frame is under 2 KiB, an MPEG audio
/// frame under 3 KiB, and an Opus packet at most 48 frames of 1,275 bytes.
const MAX_SAMPLE_BYTES: usize = 64 * 1024;

/// Settings for [`Mp4Source`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Mp4Options {
    /// Largest MP4 metadata buffered: the sample tables of the audio track,
    /// read from the `moov` box, or each `moof` box of a fragmented file
    /// together with the sample list read from it. Bounds memory against
    /// crafted sizes. Other boxes, such as cover art, are read past without
    /// being buffered. The default, 64 MiB, holds the tables of several days
    /// of audio.
    pub max_table_bytes: usize,
    /// Decoder settings for an Opus track. Its `max_packet_bytes` does not
    /// apply: MP4 samples are bounded by what the codecs allow.
    #[cfg(feature = "opus")]
    pub opus: OpusOptions,
}

impl Default for Mp4Options {
    fn default() -> Self {
        Mp4Options {
            max_table_bytes: 64 * 1024 * 1024,
            #[cfg(feature = "opus")]
            opus: OpusOptions::default(),
        }
    }
}

/// Streams PCM out of the first AAC, Opus or MPEG audio (MP1, MP2, MP3) track
/// of an MP4 file (`.m4a`, `.mp4`), such as a Safari or Chrome `MediaRecorder`
/// recording. AAC needs the `aac` feature, Opus the `opus` feature and MPEG
/// audio the `mp3` feature. Audio tracks that cannot be decoded, for want of
/// their feature or with a profile, channel layout or channel mapping family
/// this crate does not decode, are passed over.
///
/// [`new`](Self::new) reads with plain `Read`, so an upload body can be
/// decoded as it arrives. Reads are buffered internally. This needs the `moov`
/// box before the media data, which holds for fragmented files (as recorders
/// write) and for files written with the `moov` box first ("fast start"). A
/// file with its media data first, as FFmpeg and phones write by default, needs
/// [`new_seekable`](Self::new_seekable), which seeks past the media data to the
/// `moov` box and back, so that the media data is still read once.
///
/// Memory is one sample and one decoded sample, plus the sample tables of the
/// audio track, which a non-fragmented file stores in its `moov` box (4 bytes
/// per sample, about 700 KB per hour of AAC at 48 kHz), or the current
/// `moof` box of a fragmented one. Other tracks and metadata, such as cover
/// art, are read past without being buffered, or seeked past by
/// [`new_seekable`](Self::new_seekable). Both are bounded by
/// [`Mp4Options::max_table_bytes`].
///
/// Trims the encoder priming and padding given by the track's edit list.
/// Without one, AAC is trimmed by the `iTunSMPB` tag Apple encoders write, and
/// Opus by the pre-skip of its `dOps` box. Handles AAC-LC, Opus with channel
/// mapping family 0, and MPEG audio, in mono and stereo; a file with no other
/// audio track is [`Error::Unsupported`]. MPEG audio samples must each hold one
/// frame, and samples must be stored in file order, as muxers write them.
///
/// A file cut between boxes is not an error: the samples present are decoded.
/// A cut inside a box is [`Error::InvalidInput`]. Decoding stops at the end of
/// the edit list, so what follows is not read.
pub struct Mp4Source<R: Read> {
    media: Media<R>,
    decoder: Decoder,
    /// Whether `media.frame` holds a sample read but not decoded yet.
    pending: bool,
}

/// Reads the samples of the chosen track out of the media data.
struct Media<R: Read> {
    reader: BoxReader<R>,
    moov_start: u64,
    track_id: u32,
    defaults: HashMap<u32, TrackDefaults>,
    max_table_bytes: usize,
    table: Option<SampleTable>,
    /// Samples of the audio track in the current `moof` box, as absolute file
    /// offsets and sizes, and the next one to read.
    fragment: Vec<(u64, u32)>,
    next_fragment_sample: usize,
    /// Set while inside an `mdat` box, to its end when not open-ended.
    mdat: Option<Option<u64>>,
    frame: Vec<u8>,
    finished: bool,
}

/// The name of [`Mp4Source`] before it read Opus.
#[cfg(feature = "aac")]
#[deprecated(since = "0.1.2", note = "renamed to `Mp4Source`")]
pub type Mp4AacSource<R> = Mp4Source<R>;

impl<R: Read> Mp4Source<R> {
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
    ///   boxes are malformed, including a `dOps` box with channel mapping
    ///   family 0 and more than two channels.
    /// - [`Error::Unsupported`] when the media data comes before the `moov`
    ///   box, which [`seekable_with_options`](Self::seekable_with_options)
    ///   reads, or when no audio track can be decoded: none is AAC, Opus or MPEG
    ///   audio with its feature enabled, or each is encrypted, AAC other than
    ///   AAC-LC in mono or stereo, or Opus with a channel mapping family other
    ///   than 0; or when the first MPEG audio sample is not one frame, or has
    ///   a free-format bit rate.
    /// - [`Error::LimitExceeded`] when the sample tables pass
    ///   [`Mp4Options::max_table_bytes`].
    pub fn with_options(reader: R, options: Mp4Options) -> Result<Self, Error> {
        let mut reader = BoxReader::new(reader);
        let movie = read_to_movie(&mut reader, options.max_table_bytes, |_, _, _| {
            Err(Error::Unsupported(
                "MP4 media data before the moov box; read the file with \
                 Mp4Source::new_seekable, or write it with the moov box first \
                 (fast start) or fragmented"
                    .into(),
            ))
        })?;
        Self::from_movie(reader, movie, options)
    }

    fn from_movie(reader: BoxReader<R>, movie: Movie, options: Mp4Options) -> Result<Self, Error> {
        let track = movie.track;
        let edit_range = |rate| {
            track
                .edit
                .map(|edit| edit.range(track.timescale, movie.timescale, rate))
        };
        // Only an MPEG audio track reads from the media here.
        #[cfg_attr(not(feature = "mp3"), allow(unused_mut))]
        let mut media = Media {
            reader,
            moov_start: movie.moov_start,
            track_id: track.id,
            defaults: movie.defaults,
            max_table_bytes: options.max_table_bytes,
            table: track.table,
            fragment: Vec::new(),
            next_fragment_sample: 0,
            mdat: None,
            frame: Vec::new(),
            finished: false,
        };
        let (decoder, pending) = match track.config {
            #[cfg(feature = "aac")]
            Config::Aac(mut decoder) => {
                let (start, end) = edit_range(decoder.sample_rate())
                    .or_else(|| {
                        movie.gapless.map(|gapless| {
                            let end = (gapless.valid > 0)
                                .then(|| gapless.priming.saturating_add(gapless.valid));
                            (gapless.priming, end)
                        })
                    })
                    .unwrap_or((0, None));
                decoder.trim(start, end);
                (Decoder::Aac(decoder), false)
            }
            #[cfg(feature = "opus")]
            Config::Opus(head) => {
                // The edit list governs trimming; the dOps pre-skip is
                // informative only.
                let (pre_skip, end) = edit_range(48_000).unwrap_or((head.pre_skip as u64, None));
                let decoder = OpusDecoder::new(&head, pre_skip, &options.opus)?;
                let sample_rate = decoder.sample_rate();
                let decoder = Decoder::Opus {
                    sample_rate,
                    channels: decoder.channels(),
                    end: end.map(|end| to_decode_rate(end, sample_rate)),
                    decoder: Mutex::new(decoder),
                };
                (decoder, false)
            }
            // The rate and channels are in the frame headers, so the first
            // sample is read now and decoded with the others.
            #[cfg(feature = "mp3")]
            Config::Mpa {
                sample_rate,
                channels,
            } => {
                if !media.next_frame()? {
                    if sample_rate == 0 || channels == 0 {
                        return Err(invalid("empty MPEG audio track without a rate"));
                    }
                    let empty = Decoder::Empty {
                        sample_rate,
                        channels,
                    };
                    return Ok(Mp4Source {
                        media,
                        decoder: empty,
                        pending: false,
                    });
                }
                // Old QuickTime files count MPEG audio in samples of one byte.
                let header = media.frame.first_chunk().ok_or_else(|| {
                    Error::Unsupported("MPEG audio samples shorter than a frame".into())
                })?;
                let header = mpa::Header::parse(*header)?;
                header.check_length(media.frame.len())?;
                let mut decoder = MpaDecoder::new(header)?;
                let (start, end) = edit_range(decoder.sample_rate()).unwrap_or((0, None));
                decoder.trim(start, end);
                (Decoder::Mpa(Box::new(decoder)), true)
            }
        };

        Ok(Mp4Source {
            media,
            decoder,
            pending,
        })
    }
}

impl<R: Read + Seek> Mp4Source<R> {
    /// Reads up to the end of the `moov` box, wherever it is, with default
    /// options.
    ///
    /// # Errors
    ///
    /// As for [`seekable_with_options`](Self::seekable_with_options).
    pub fn new_seekable(reader: R) -> Result<Self, Error> {
        Self::seekable_with_options(reader, Mp4Options::default())
    }

    /// Reads up to the end of the `moov` box, seeking past the media data that
    /// comes before it, then seeks back to the start of that media data. The
    /// MP4 file starts at the current position of `reader`.
    ///
    /// Seeks past what it does not decode, such as the samples of a video
    /// track, so the file must not grow while it is read. If `reader` cannot
    /// seek, as for a pipe, a file with the `moov` box first is read through.
    ///
    /// # Errors
    ///
    /// As for [`with_options`](Self::with_options), except that media data
    /// before the `moov` box is read, and [`Error::Io`] when seeking fails.
    pub fn seekable_with_options(reader: R, options: Mp4Options) -> Result<Self, Error> {
        let mut reader = BoxReader::new(reader);
        reader.input.seek_to_skip()?;
        let mut media_start = None;
        let movie = read_to_movie(
            &mut reader,
            options.max_table_bytes,
            |reader, start, end| {
                media_start.get_or_insert(start);
                reader.seek_to(end)
            },
        )?;
        if let Some(start) = media_start {
            reader.seek_to(start)?;
        }
        Self::from_movie(reader, movie, options)
    }
}

/// Reads the top-level boxes up to the end of the `moov` box. `media_first`
/// handles an `mdat` box with data that comes before it, given its start and
/// end.
fn read_to_movie<R: Read>(
    reader: &mut BoxReader<R>,
    max_table_bytes: usize,
    mut media_first: impl FnMut(&mut BoxReader<R>, u64, u64) -> Result<(), Error>,
) -> Result<Movie, Error> {
    loop {
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
            b"moov" => return read_movie(reader, &mp4_box, max_table_bytes),
            b"mdat" if mp4_box.end != Some(reader.position()) => {
                // An open-ended mdat box leaves no room for a moov box.
                let end = mp4_box
                    .end
                    .ok_or_else(|| invalid("open-ended mdat box before the moov box"))?;
                media_first(reader, mp4_box.start, end)?;
            }
            b"moof" => return Err(invalid("moof box before the moov box")),
            _ => reader.skip_box(&mp4_box)?,
        }
    }
}

impl<R: Read> Media<R> {
    /// Reads the next sample that is not empty into `frame`. Returns false at
    /// the end of the input.
    fn next_frame(&mut self) -> Result<bool, Error> {
        while !self.finished {
            let Some((offset, size)) = self.next_sample()? else {
                self.finished = true;
                break;
            };
            if size == 0 {
                continue;
            }
            if self.read_sample(offset, size)? {
                return Ok(true);
            }
            self.finished = true;
        }
        Ok(false)
    }

    /// The offset and size of the next sample of the audio track, reading
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
        if size as usize > MAX_SAMPLE_BYTES {
            return Err(invalid("MP4 sample larger than its codec allows"));
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
                // Met again after seeking back to the media data before it.
                b"moov" if mp4_box.start == self.moov_start => self.reader.skip_box(&mp4_box)?,
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

impl<R: Read> fmt::Debug for Mp4Source<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mp4Source")
            .field("decoder", &self.decoder)
            .field("track_id", &self.media.track_id)
            .field("finished", &self.media.finished)
            .finish_non_exhaustive()
    }
}

impl<R: Read> PcmSource for Mp4Source<R> {
    fn sample_rate(&self) -> u32 {
        self.decoder.sample_rate()
    }

    fn channels(&self) -> u16 {
        self.decoder.channels()
    }

    fn next_block(&mut self) -> Result<Option<&[i16]>, Error> {
        while !self.decoder.past_end() {
            if !self.pending && !self.media.next_frame()? {
                break;
            }
            self.pending = false;
            if self.decoder.decode(&self.media.frame)? {
                return Ok(Some(self.decoder.output()));
            }
        }
        Ok(None)
    }
}

/// The decoder of the chosen track, with what it needs to trim its output.
#[derive(Debug)]
enum Decoder {
    #[cfg(feature = "aac")]
    Aac(Box<AacDecoder>),
    #[cfg(feature = "opus")]
    Opus {
        sample_rate: u32,
        channels: u16,
        /// Where the audio ends, in frames at the decode rate from the start
        /// of the stream, pre-skip included.
        end: Option<u64>,
        /// Locked never, only reached through `get_mut`: the `Mutex` keeps the
        /// source `Sync`, which libopus's decoder is not.
        decoder: Mutex<OpusDecoder>,
    },
    #[cfg(feature = "mp3")]
    Mpa(Box<MpaDecoder>),
    /// An MPEG audio track with no samples, described by its sample entry.
    #[cfg(feature = "mp3")]
    Empty { sample_rate: u32, channels: u16 },
}

impl Decoder {
    fn sample_rate(&self) -> u32 {
        match self {
            #[cfg(feature = "aac")]
            Decoder::Aac(decoder) => decoder.sample_rate(),
            #[cfg(feature = "opus")]
            Decoder::Opus { sample_rate, .. } => *sample_rate,
            #[cfg(feature = "mp3")]
            Decoder::Mpa(decoder) => decoder.sample_rate(),
            #[cfg(feature = "mp3")]
            Decoder::Empty { sample_rate, .. } => *sample_rate,
        }
    }

    fn channels(&self) -> u16 {
        match self {
            #[cfg(feature = "aac")]
            Decoder::Aac(decoder) => decoder.channels(),
            #[cfg(feature = "opus")]
            Decoder::Opus { channels, .. } => *channels,
            #[cfg(feature = "mp3")]
            Decoder::Mpa(decoder) => decoder.channels(),
            #[cfg(feature = "mp3")]
            Decoder::Empty { channels, .. } => *channels,
        }
    }

    /// Whether every frame up to the end of the trimmed audio is decoded.
    fn past_end(&mut self) -> bool {
        match self {
            #[cfg(feature = "aac")]
            Decoder::Aac(decoder) => decoder.past_end(),
            #[cfg(feature = "opus")]
            Decoder::Opus { decoder, end, .. } => {
                end.is_some_and(|end| opus(decoder).position() >= end)
            }
            #[cfg(feature = "mp3")]
            Decoder::Mpa(decoder) => decoder.past_end(),
            #[cfg(feature = "mp3")]
            Decoder::Empty { .. } => true,
        }
    }

    /// Decodes a sample. Returns whether any samples are left after trimming;
    /// they are then in [`output`](Self::output).
    fn decode(&mut self, sample: &[u8]) -> Result<bool, Error> {
        match self {
            #[cfg(feature = "aac")]
            Decoder::Aac(decoder) => decoder.decode(sample),
            #[cfg(feature = "opus")]
            Decoder::Opus { decoder, end, .. } => {
                opus(decoder).decode(sample, end.map_or(End::Open, End::At))
            }
            #[cfg(feature = "mp3")]
            Decoder::Mpa(decoder) => decoder.decode(sample),
            #[cfg(feature = "mp3")]
            Decoder::Empty { .. } => Ok(false),
        }
    }

    fn output(&mut self) -> &[i16] {
        match self {
            #[cfg(feature = "aac")]
            Decoder::Aac(decoder) => decoder.output(),
            #[cfg(feature = "opus")]
            Decoder::Opus { decoder, .. } => opus(decoder).output(),
            #[cfg(feature = "mp3")]
            Decoder::Mpa(decoder) => decoder.output(),
            #[cfg(feature = "mp3")]
            Decoder::Empty { .. } => &[],
        }
    }
}

#[cfg(feature = "opus")]
fn opus(decoder: &mut Mutex<OpusDecoder>) -> &mut OpusDecoder {
    decoder.get_mut().unwrap_or_else(PoisonError::into_inner)
}

/// What the `moov` box says about the chosen track and its fragments.
struct Movie {
    moov_start: u64,
    timescale: u32,
    track: Track,
    defaults: HashMap<u32, TrackDefaults>,
    #[cfg(feature = "aac")]
    gapless: Option<Gapless>,
}

struct Track {
    id: u32,
    /// Media timescale, from `mdhd`.
    timescale: u32,
    config: Config,
    edit: Option<Edit>,
    table: Option<SampleTable>,
}

/// The decoder configuration of a track whose codec is enabled.
enum Config {
    /// Built while choosing the track, so that one it cannot decode is passed
    /// over.
    #[cfg(feature = "aac")]
    Aac(Box<AacDecoder>),
    #[cfg(feature = "opus")]
    Opus(OpusHead),
    /// Configured from the first frame. The sample entry's rate and channels
    /// serve only for a track without samples.
    #[cfg(feature = "mp3")]
    Mpa { sample_rate: u32, channels: u16 },
}

/// The first edit of an edit list that shows media.
#[derive(Clone, Copy)]
struct Edit {
    /// In the movie timescale; 0 when unknown, as in fragmented files.
    duration: u64,
    /// In the media timescale.
    media_time: u64,
}

impl Edit {
    /// The start and, when the duration is known, the end of the media shown,
    /// in frames at `rate`.
    fn range(self, media_timescale: u32, movie_timescale: u32, rate: u32) -> (u64, Option<u64>) {
        let rate = rate as u128;
        let start = self.media_time as u128 * rate / media_timescale as u128;
        let end = (self.duration > 0)
            .then(|| start + self.duration as u128 * rate / movie_timescale as u128);
        let saturate = |frames: u128| u64::try_from(frames).unwrap_or(u64::MAX);
        (saturate(start), end.map(saturate))
    }
}

/// Encoder priming and length from Apple's `iTunSMPB` tag, in samples.
#[cfg(feature = "aac")]
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
    #[cfg(feature = "aac")]
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
                TrackKind::Decodable(audio) => track = Some(audio),
                TrackKind::OtherAudio(codec) => {
                    other_codec.get_or_insert(codec);
                }
                TrackKind::Other => {}
            },
            b"mvex" => read_track_defaults(reader, &child, &mut defaults)?,
            #[cfg(feature = "aac")]
            b"udta" | b"meta" => {
                if let Some(found) = read_gapless(reader, &child)? {
                    gapless = Some(found);
                }
            }
            _ => reader.skip_box(&child)?,
        }
    }
    let track = track.ok_or_else(|| match other_codec {
        Some(TrackCodec::Encrypted) => Error::Unsupported("encrypted audio track".into()),
        Some(TrackCodec::Other(codec)) => {
            Error::Unsupported(format!("no supported audio track (found {codec})"))
        }
        None => invalid("no audio track"),
    })?;
    let timescale = timescale
        .filter(|&timescale| timescale != 0)
        .ok_or_else(|| invalid("missing movie timescale"))?;
    Ok(Movie {
        moov_start: moov.start,
        timescale,
        track,
        defaults,
        #[cfg(feature = "aac")]
        gapless,
    })
}

enum TrackKind {
    Decodable(Track),
    OtherAudio(TrackCodec),
    Other,
}

enum TrackCodec {
    Encrypted,
    Other(String),
}

/// Reads a `trak` box, keeping the sample tables only for an audio track
/// whose codec is enabled.
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
        Some(Codec::Decodable(config)) => config,
        Some(Codec::Encrypted) => return Ok(TrackKind::OtherAudio(TrackCodec::Encrypted)),
        Some(Codec::Other(name)) => return Ok(TrackKind::OtherAudio(TrackCodec::Other(name))),
        None => return Err(invalid("audio track without a sample description")),
    };
    let timescale = state
        .timescale
        .filter(|&timescale| timescale != 0)
        .ok_or_else(|| invalid("missing media timescale"))?;
    let id = state.id.ok_or_else(|| invalid("missing track header"))?;
    if state.oversized_tables {
        return Err(Error::LimitExceeded(
            "MP4 sample tables larger than max_table_bytes",
        ));
    }
    let table = state.tables.into_table()?;
    Ok(TrackKind::Decodable(Track {
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
    /// Set when a table did not fit in `max_table_bytes` and was skipped.
    oversized_tables: bool,
}

enum Codec {
    Decodable(Config),
    Encrypted,
    /// A codec this build does not decode, named for the error.
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
            // Chrome writes the tables before the sample description, so
            // they are kept while the codec is unknown.
            b"stsz" | b"stz2" | b"stco" | b"co64" | b"stsc"
                if state.sound
                    && !matches!(state.codec, Some(Codec::Encrypted | Codec::Other(_))) =>
            {
                let budget = max_table_bytes.saturating_sub(state.tables.bytes());
                if child.size().is_none_or(|size| size > budget as u64) {
                    state.oversized_tables = true;
                    reader.skip_box(&child)?;
                } else {
                    let body = reader.read_body(&child, u64::MAX)?;
                    state.tables.set(&child.kind, body);
                }
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
    // Codecs this build does not decode are passed over unparsed, so that a
    // malformed track of theirs cannot reject the file.
    let parse: fn(&[u8]) -> Result<Codec, Error> = match kind {
        #[cfg(any(feature = "aac", feature = "mp3"))]
        b"mp4a" => mp4a_config,
        #[cfg(feature = "opus")]
        b"Opus" => opus_config,
        // QuickTime's MPEG audio sample entries, with no decoder
        // configuration.
        #[cfg(feature = "mp3")]
        b".mp3" | b".mp2" => |_| Ok(Codec::Decodable(MPA)),
        #[cfg(not(any(feature = "aac", feature = "mp3")))]
        b"mp4a" => return Ok(Codec::Other("mp4a, without the aac or mp3 feature".into())),
        #[cfg(not(feature = "opus"))]
        b"Opus" => return Ok(Codec::Other("Opus, without the opus feature".into())),
        #[cfg(not(feature = "mp3"))]
        b".mp3" | b".mp2" => {
            return Ok(Codec::Other("MPEG audio, without the mp3 feature".into()));
        }
        b"enca" => return Ok(Codec::Encrypted),
        other => return Ok(Codec::Other(String::from_utf8_lossy(other).into_owned())),
    };
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
    #[cfg_attr(not(feature = "mp3"), allow(unused_mut))]
    let mut codec = parse(children)?;
    #[cfg(feature = "mp3")]
    if let Codec::Decodable(Config::Mpa {
        sample_rate,
        channels,
    }) = &mut codec
    {
        *channels = be_u16(entry, 16)?;
        *sample_rate = be_u32(entry, 24)? >> 16;
    }
    Ok(codec)
}

/// An MPEG audio track, whose sample entry fields are filled in after.
#[cfg(feature = "mp3")]
const MPA: Config = Config::Mpa {
    sample_rate: 0,
    channels: 0,
};

/// Reads the `dOps` box of an `Opus` sample entry.
#[cfg(feature = "opus")]
fn opus_config(children: &[u8]) -> Result<Codec, Error> {
    let dops = find_child(children, b"dOps")
        .ok_or_else(|| invalid("Opus sample entry without a dOps box"))?;
    match OpusHead::from_dops(dops) {
        Ok(head) => Ok(Codec::Decodable(Config::Opus(head))),
        Err(Error::Unsupported(reason)) => Ok(Codec::Other(format!("Opus with {reason}"))),
        Err(err) => Err(err),
    }
}

/// Reads the `esds` box of an `mp4a` sample entry.
#[cfg(any(feature = "aac", feature = "mp3"))]
fn mp4a_config(children: &[u8]) -> Result<Codec, Error> {
    let esds = find_child(children, b"esds")
        .or_else(|| find_child(children, b"wave").and_then(|wave| find_child(wave, b"esds")))
        .ok_or_else(|| invalid("mp4a sample entry without an esds box"))?;
    esds_config(esds)
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

/// Reads the decoder configuration of an `esds` body.
#[cfg(any(feature = "aac", feature = "mp3"))]
fn esds_config(esds: &[u8]) -> Result<Codec, Error> {
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
    match object_type {
        // MPEG-4 audio, and the MPEG-2 AAC profiles.
        #[cfg(feature = "aac")]
        0x40 | 0x66..=0x68 => {
            let mut specific = config.get(13..).ok_or_else(bad)?;
            let (tag, asc) = descriptor(&mut specific).ok_or_else(bad)?;
            if tag != 0x05 {
                return Err(bad());
            }
            match AacDecoder::new(asc) {
                Ok(decoder) => Ok(Codec::Decodable(Config::Aac(Box::new(decoder)))),
                Err(Error::Unsupported(reason)) => Ok(Codec::Other(reason)),
                Err(err) => Err(err),
            }
        }
        #[cfg(not(feature = "aac"))]
        0x40 | 0x66..=0x68 => Ok(Codec::Other("AAC, without the aac feature".into())),
        // MPEG-2 and MPEG-1 audio.
        #[cfg(feature = "mp3")]
        0x69 | 0x6B => Ok(Codec::Decodable(MPA)),
        #[cfg(not(feature = "mp3"))]
        0x69 | 0x6B => Ok(Codec::Other("MPEG audio, without the mp3 feature".into())),
        _ => Ok(Codec::Other(format!("object type {object_type:#04x}"))),
    }
}

/// Reads an MPEG-4 descriptor: a tag and a size of up to four 7-bit bytes.
#[cfg(any(feature = "aac", feature = "mp3"))]
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
#[cfg(feature = "aac")]
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
#[cfg(feature = "aac")]
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

/// The raw sample table boxes of the audio track.
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
    fn new(reader: R) -> Self {
        BoxReader {
            input: CountingReader::new(reader, "truncated box"),
        }
    }

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

impl<R: Read + Seek> BoxReader<R> {
    fn seek_to(&mut self, position: u64) -> Result<(), Error> {
        self.input.seek_to(position)
    }
}
