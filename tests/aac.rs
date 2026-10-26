//! AAC framing on MP4 files built in memory around frames from small ADTS
//! fixtures.

#![cfg(feature = "aac")]

mod common;

use std::fs::File;
use std::io::{self, Cursor, Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::fd::OwnedFd;
#[cfg(windows)]
use std::os::windows::io::OwnedHandle;
use std::thread;

use common::*;
use waveform_peaks::{AdtsAacSource, Error, Mp4Options, Mp4Source, PcmSource};

const MONO: &[u8] = include_bytes!("data/sine-mono-44100.aac");
const STEREO: &[u8] = include_bytes!("data/chord-stereo-48000.aac");
/// Recorded by Chrome 155's `MediaRecorder`: fragmented and stereo, with the
/// sample tables before the sample description and no edit list.
const CHROME: &[u8] = include_bytes!("data/chrome-aac-stereo.mp4");
/// Recorded by Safari 27's `MediaRecorder`: fragmented and stereo, with an
/// edit list of unknown duration.
const SAFARI: &[u8] = include_bytes!("data/safari-aac-stereo.mp4");
/// HE-AAC from Apple's encoder, which signals SBR only inside the frames.
const HE_AAC: &[u8] = include_bytes!("data/he-aac-apple.m4a");

/// The fixture lasts 1.013 s at 44.1 kHz.
const MONO_LENGTH: u64 = 44_673;
const PRIMING: u64 = 1024;

fn mp4(bytes: Vec<u8>) -> Result<Vec<i16>, Error> {
    mp4_with(bytes, Mp4Options::default())
}

fn mp4_with(bytes: Vec<u8>, options: Mp4Options) -> Result<Vec<i16>, Error> {
    samples(Mp4Source::with_options(Cursor::new(bytes), options)?)
}

fn adts(bytes: &[u8]) -> Vec<i16> {
    samples(AdtsAacSource::new(Cursor::new(bytes.to_vec())).unwrap()).unwrap()
}

fn mono_track(per_chunk: usize) -> Mp4Track {
    let (config, frames) = adts_frames(MONO);
    Mp4Track::audio(&config, 1, 44_100, &frames, per_chunk)
}

#[test]
fn adts_decodes_every_frame() {
    let (_, frames) = adts_frames(MONO);
    let source = AdtsAacSource::new(Cursor::new(MONO.to_vec())).unwrap();
    assert_eq!((source.sample_rate(), source.channels()), (44_100, 1));
    assert_eq!(samples(source).unwrap().len(), frames.len() * 1024);

    let (_, frames) = adts_frames(STEREO);
    let source = AdtsAacSource::new(Cursor::new(STEREO.to_vec())).unwrap();
    assert_eq!((source.sample_rate(), source.channels()), (48_000, 2));
    assert_eq!(samples(source).unwrap().len(), frames.len() * 1024 * 2);
}

#[test]
fn adts_skips_a_leading_id3_tag() {
    let mut tagged = b"ID3\x04\0\0\0\0\x01\x00".to_vec();
    tagged.extend([0; 128]);
    tagged.extend(MONO);
    assert_same(&adts(&tagged), &adts(MONO));
}

/// Uploads can arrive in reads of any size.
#[test]
fn adts_reads_one_byte_at_a_time() {
    struct OneByte<R>(R);
    impl<R: Read> Read for OneByte<R> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let len = buf.len().min(1);
            self.0.read(&mut buf[..len])
        }
    }
    let mut tagged = b"ID3\x04\0\0\0\0\x01\x00".to_vec();
    tagged.extend([0; 128]);
    tagged.extend(MONO);
    let source = AdtsAacSource::new(OneByte(Cursor::new(tagged))).unwrap();
    assert_same(&samples(source).unwrap(), &adts(MONO));
}

#[test]
fn adts_stops_at_a_trailing_tag() {
    let mut id3v1 = MONO.to_vec();
    id3v1.extend(b"TAG");
    id3v1.extend([0; 125]);
    let mut ape = MONO.to_vec();
    ape.extend(b"APETAGEX");
    ape.extend([0; 24]);
    for tagged in [id3v1, ape] {
        assert_same(&adts(&tagged), &adts(MONO));
    }
}

#[test]
fn mp4_matches_adts_with_the_same_frames() {
    for per_chunk in [1, 7, 1000] {
        let file = faststart_mp4(&[mono_track(per_chunk)], 1000, &[], &[]);
        assert_same(&mp4(file).unwrap(), &adts(MONO));
    }
    let (config, frames) = adts_frames(STEREO);
    let track = Mp4Track::audio(&config, 2, 48_000, &frames, 5);
    let file = faststart_mp4(&[track], 1000, &[], &[]);
    assert_same(&mp4(file).unwrap(), &adts(STEREO));
}

#[test]
fn edit_list_trims_priming_and_padding() {
    let mut track = mono_track(10);
    track.edit = Some((1013, PRIMING as i64));
    let file = faststart_mp4(&[track], 1000, &[], &[]);
    let expected = &adts(MONO)[PRIMING as usize..(PRIMING + MONO_LENGTH) as usize];
    assert_same(&mp4(file).unwrap(), expected);
}

/// Apple's encoder records priming and length only in an `iTunSMPB` tag.
#[test]
fn itunsmpb_trims_without_an_edit_list() {
    let expected = &adts(MONO)[PRIMING as usize..(PRIMING + MONO_LENGTH) as usize];
    for iso_meta in [false, true] {
        let udta = itunsmpb_udta(PRIMING as u32, 383, MONO_LENGTH, iso_meta);
        let file = faststart_mp4(&[mono_track(10)], 1000, &udta, &[]);
        // The cover art in front of the tag is read past, not buffered.
        let mut options = Mp4Options::default();
        options.max_table_bytes = 10_000;
        assert_same(&mp4_with(file, options).unwrap(), expected);
    }
}

#[test]
fn edit_list_takes_precedence_over_itunsmpb() {
    let mut track = mono_track(10);
    track.edit = Some((1013, PRIMING as i64));
    let udta = itunsmpb_udta(2112, 0, 1000, true);
    let file = faststart_mp4(&[track], 1000, &udta, &[]);
    let expected = &adts(MONO)[PRIMING as usize..(PRIMING + MONO_LENGTH) as usize];
    assert_same(&mp4(file).unwrap(), expected);
}

#[test]
fn reads_chrome_recordings() {
    let source = Mp4Source::new(Cursor::new(CHROME)).unwrap();
    assert_eq!((source.sample_rate(), source.channels()), (48_000, 2));
    // 43 frames of 1,024, with nothing to trim them by, as FFmpeg also decodes
    // it.
    assert_eq!(samples(source).unwrap().len(), 44_032 * 2);
}

#[test]
fn reads_safari_recordings() {
    let source = Mp4Source::new(Cursor::new(SAFARI)).unwrap();
    assert_eq!((source.sample_rate(), source.channels()), (48_000, 2));
    // 49 frames of 1,024, less the edit list's 2,112 priming frames, as FFmpeg
    // also decodes it.
    assert_eq!(samples(source).unwrap().len(), 48_064 * 2);
}

/// The codec is unknown while the tables are read, so the limit applies once
/// the track turns out to be the one decoded.
#[test]
fn oversized_tables_before_the_sample_description() {
    let mut options = Mp4Options::default();
    options.max_table_bytes = 100;
    let file = stsd_last(&faststart_mp4(&[mono_track(1)], 1000, &[], &[]));
    let result = mp4_with(file, options);
    assert!(matches!(result, Err(Error::LimitExceeded(_))), "{result:?}");

    // Tables over the limit on a track that is passed over do not count.
    let mut ac3 = mono_track(1);
    ac3.id = 2;
    ac3.sample_entry = audio_sample_entry(b"ac-3", 1, 48_000, &[]);
    ac3.chunks = vec![vec![vec![0; 1]; 2000]];
    let file = stsd_last(&faststart_mp4(&[ac3, mono_track(1000)], 1000, &[], &[]));
    options.max_table_bytes = 1000;
    assert_same(&mp4_with(file, options).unwrap(), &adts(MONO));
}

/// Recorders write fragments, each with its own moof and mdat box.
#[test]
fn reads_fragmented_files() {
    let (_, frames) = adts_frames(MONO);
    let fragments: Vec<Vec<Vec<u8>>> = frames.chunks(13).map(<[_]>::to_vec).collect();
    for explicit_base in [false, true] {
        let file = fragmented_mp4(&mono_track(1), &fragments, explicit_base);
        assert_same(&mp4(file).unwrap(), &adts(MONO));
    }
}

/// A `moof` box whose run lists no sample sizes, only a count of samples of
/// `default_size` set by the track fragment header, then its `mdat` box.
fn default_size_fragment(track_id: u32, default_size: u32, count: u32, data: &[u8]) -> Vec<u8> {
    let build = |data_offset: u32| {
        let mut tfhd = track_id.to_be_bytes().to_vec();
        tfhd.extend(default_size.to_be_bytes());
        let mut trun = count.to_be_bytes().to_vec();
        trun.extend(data_offset.to_be_bytes());
        let traf = [
            full_box(b"tfhd", 0, 0x2_0010, &tfhd),
            full_box(b"trun", 0, 0x1, &trun),
        ]
        .concat();
        mp4_box(
            b"moof",
            &[
                full_box(b"mfhd", 0, 0, &1u32.to_be_bytes()),
                mp4_box(b"traf", &traf),
            ]
            .concat(),
        )
    };
    let moof_len = build(0).len() as u32;
    [build(moof_len + 8), mp4_box(b"mdat", data)].concat()
}

/// A run without sample sizes can claim millions of samples in a few bytes,
/// which must not cost time when they hold no audio of the track.
#[test]
fn runs_without_sample_sizes() {
    let (_, frames) = adts_frames(MONO);
    let frame = &frames[10];
    let header = fragmented_mp4(&mono_track(1), &[], false);
    let mut file = header.clone();
    for _ in 0..500 {
        file.extend(default_size_fragment(1, 0, 4_000_000, &[]));
        file.extend(default_size_fragment(2, 1000, 4_000_000, &[]));
    }
    for _ in 0..2 {
        file.extend(default_size_fragment(
            1,
            frame.len() as u32,
            5,
            &frame.repeat(5),
        ));
    }
    let expected = fragmented_mp4(&mono_track(1), &vec![vec![frame.clone(); 5]; 2], false);
    assert_same(&mp4(file).unwrap(), &mp4(expected).unwrap());
}

/// Other tracks' chunks sit between the AAC track's in the media data.
#[test]
fn skips_other_tracks_and_boxes() {
    let video = Mp4Track {
        id: 2,
        handler: *b"vide",
        sample_entry: mp4_box(b"avc1", &[0; 78]),
        timescale: 90_000,
        edit: None,
        chunks: vec![vec![vec![0xEE; 50_000]]; 8],
        wide_offsets: false,
        quicktime: false,
    };
    let mut audio = mono_track(6);
    audio.id = 1;
    audio.wide_offsets = true;
    let between = mp4_box(b"free", &[0; 300]);
    let file = faststart_mp4(&[video, audio], 1000, &[], &between);
    assert_same(&mp4(file).unwrap(), &adts(MONO));
}

/// QuickTime files have a second `hdlr` box, for the data handler, in `minf`.
#[test]
fn reads_quicktime_files() {
    let mut track = mono_track(10);
    track.quicktime = true;
    let file = faststart_mp4(&[track], 1000, &[], &[]);
    assert_same(&mp4(file).unwrap(), &adts(MONO));
}

#[test]
fn media_data_before_moov_needs_seek() {
    let file = media_first_mp4(&[mono_track(10)], 1000, &[]);
    let mdat_start = file.windows(4).position(|w| w == b"mdat").unwrap() - 4;
    let result = mp4(file.clone());
    assert!(
        matches!(&result, Err(Error::Unsupported(m)) if m.contains("new_seekable")),
        "{result:?}"
    );
    // Seeking would not help: no moov box can follow.
    let mut open_ended = file;
    open_ended[mdat_start..mdat_start + 4].copy_from_slice(&0u32.to_be_bytes());
    let result = mp4(open_ended);
    assert!(
        matches!(&result, Err(Error::InvalidInput(m)) if m.contains("open-ended")),
        "{result:?}"
    );
}

fn seekable(bytes: Vec<u8>) -> Result<Vec<i16>, Error> {
    samples(Mp4Source::new_seekable(Cursor::new(bytes))?)
}

#[test]
fn seekable_reads_media_data_before_moov() {
    let video = Mp4Track {
        id: 2,
        handler: *b"vide",
        sample_entry: mp4_box(b"avc1", &[0; 78]),
        timescale: 90_000,
        edit: None,
        chunks: vec![vec![vec![0xEE; 50_000]]; 8],
        wide_offsets: false,
        quicktime: false,
    };
    let file = media_first_mp4(&[video, mono_track(6)], 1000, &[]);
    assert_same(&seekable(file).unwrap(), &adts(MONO));

    // The edit list and the gapless tag are in the moov box, read first.
    let mut track = mono_track(10);
    track.edit = Some((1000, PRIMING as i64));
    let file = media_first_mp4(&[track], 1000, &[]);
    assert_eq!(seekable(file).unwrap().len() as u64, 44_100);
    let udta = itunsmpb_udta(PRIMING as u32, 0, MONO_LENGTH, false);
    let file = media_first_mp4(&[mono_track(10)], 1000, &udta);
    assert_eq!(seekable(file).unwrap().len() as u64, MONO_LENGTH);
}

/// Counts the bytes read and the seeks, and fails every seek unless
/// `can_seek`.
struct Counted<R> {
    inner: R,
    read: usize,
    seeks: usize,
    can_seek: bool,
}

impl<R> Counted<R> {
    fn new(inner: R, can_seek: bool) -> Self {
        Counted {
            inner,
            read: 0,
            seeks: 0,
            can_seek,
        }
    }
}

impl<R: Read> Read for Counted<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buf)?;
        self.read += read;
        Ok(read)
    }
}

impl<R: Seek> Seek for Counted<R> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.seeks += 1;
        if !self.can_seek {
            return Err(io::ErrorKind::NotSeekable.into());
        }
        self.inner.seek(position)
    }
}

fn video_track(chunks: Vec<usize>) -> Mp4Track {
    Mp4Track {
        id: 2,
        handler: *b"vide",
        sample_entry: mp4_box(b"avc1", &[0; 78]),
        timescale: 90_000,
        edit: None,
        chunks: chunks
            .into_iter()
            .map(|len| vec![vec![0xEE; len]])
            .collect(),
        wide_offsets: false,
        quicktime: false,
    }
}

/// Seeks past the other tracks rather than reading them, which matters for
/// the video of a phone recording.
#[test]
fn seekable_seeks_past_other_tracks() {
    let tracks = [video_track(vec![200_000; 10]), mono_track(6)];
    for file in [
        media_first_mp4(&tracks, 1000, &[]),
        faststart_mp4(&tracks, 1000, &[], &[]),
    ] {
        let mut reader = Counted::new(Cursor::new(&file), true);
        let source = Mp4Source::new_seekable(&mut reader).unwrap();
        assert_same(&samples(source).unwrap(), &adts(MONO));
        assert!(reader.read < file.len() / 10, "{} bytes read", reader.read);
    }

    // A seek past the end fails as reading through would.
    let file = faststart_mp4(&tracks, 1000, &[], &[]);
    let last_video = file.windows(4).rposition(|w| w == [0xEE; 4]).unwrap();
    let result = seekable(file[..last_video].to_vec());
    assert!(
        matches!(&result, Err(Error::InvalidInput(m)) if m.contains("truncated")),
        "{result:?}"
    );

    // Also when the file starts after other data, and the cut is in data
    // that is only skipped.
    let mut chunks = vec![1000; 7];
    chunks.push(200_000);
    let tracks = [mono_track(6), video_track(chunks)];
    let prefix = vec![0xAB; 1000];
    let file = [&prefix[..], &faststart_mp4(&tracks, 1000, &[], &[])].concat();
    let mut reader = Cursor::new(&file[..file.len() - 3]);
    reader.seek(SeekFrom::Start(prefix.len() as u64)).unwrap();
    let result = Mp4Source::new_seekable(reader).and_then(samples);
    assert!(
        matches!(&result, Err(Error::InvalidInput(m)) if m.contains("truncated")),
        "{result:?}"
    );
}

/// Short skips read through, so that a reader over the network does not make
/// a request for each chunk of another track.
#[test]
fn seekable_reads_through_short_skips() {
    let tracks = [video_track(vec![10_000; 10]), mono_track(6)];
    let file = faststart_mp4(&tracks, 1000, &[], &[]);
    let mut reader = Counted::new(Cursor::new(&file), true);
    let source = Mp4Source::new_seekable(&mut reader).unwrap();
    assert_same(&samples(source).unwrap(), &adts(MONO));
    // Only the seeks that measure the input.
    assert_eq!(reader.seeks, 3);
}

/// Seeks as a Windows pipe does: without an error, but neither seeking nor
/// reading moves it, and its end is the data waiting in it.
struct WindowsPipe<R> {
    inner: R,
    position: u64,
}

impl<R: Read> Read for WindowsPipe<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl<R> Seek for WindowsPipe<R> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.position = match position {
            SeekFrom::Start(offset) => offset,
            SeekFrom::End(offset) => 4096_u64.saturating_add_signed(offset),
            SeekFrom::Current(offset) => self.position.saturating_add_signed(offset),
        };
        Ok(self.position)
    }
}

#[test]
fn seekable_reads_through_an_input_that_only_says_it_seeks() {
    let mut chunks = vec![1000; 7];
    chunks.push(200_000);
    for tracks in [
        [video_track(vec![100_000; 10]), mono_track(6)],
        // The mdat box is then made open-ended, so the only long skip is to
        // its end.
        [mono_track(6), video_track(chunks)],
    ] {
        let mut file = faststart_mp4(&tracks, 1000, &[], &[]);
        if tracks[0].handler == *b"soun" {
            let mdat = file.windows(4).rposition(|w| w == b"mdat").unwrap() - 4;
            file[mdat..mdat + 4].copy_from_slice(&0u32.to_be_bytes());
        }
        let reader = WindowsPipe {
            inner: Cursor::new(&file),
            position: 0,
        };
        let source = Mp4Source::new_seekable(reader).unwrap();
        assert_same(&samples(source).unwrap(), &adts(MONO));
    }
}

/// A pipe opened as a file is `Seek`, but cannot seek.
#[test]
fn seekable_reads_a_fast_start_file_from_a_pipe() {
    let tracks = [video_track(vec![100_000; 10]), mono_track(6)];
    let file = faststart_mp4(&tracks, 1000, &[], &[]);
    let (reader, mut writer) = io::pipe().unwrap();
    let writing = thread::spawn({
        let file = file.clone();
        move || writer.write_all(&file)
    });
    #[cfg(unix)]
    let reader = File::from(OwnedFd::from(reader));
    #[cfg(windows)]
    let reader = File::from(OwnedHandle::from(reader));
    let decoded = samples(Mp4Source::new_seekable(reader).unwrap()).unwrap();
    writing.join().unwrap().unwrap();
    assert_same(&decoded, &adts(MONO));
}

/// An open-ended mdat box runs to the end of the file, past the last audio
/// sample.
#[test]
fn seekable_seeks_to_the_end_of_an_open_ended_mdat() {
    let mut chunks = vec![1000; 7];
    chunks.push(2_000_000);
    let tracks = [mono_track(6), video_track(chunks)];
    let mut file = faststart_mp4(&tracks, 1000, &[], &[]);
    let mdat = file.windows(4).rposition(|w| w == b"mdat").unwrap() - 4;
    file[mdat..mdat + 4].copy_from_slice(&0u32.to_be_bytes());

    let mut reader = Counted::new(Cursor::new(&file), true);
    let source = Mp4Source::new_seekable(&mut reader).unwrap();
    assert_same(&samples(source).unwrap(), &adts(MONO));
    assert!(reader.read < 100_000, "{} bytes read", reader.read);
}

/// An input that is `Seek`, but whose seeks all fail.
#[test]
fn seekable_reads_through_an_input_that_cannot_seek() {
    let tracks = [video_track(vec![1000; 10]), mono_track(6)];
    let file = faststart_mp4(&tracks, 1000, &[], &[]);
    let source = Mp4Source::new_seekable(Counted::new(Cursor::new(&file), false)).unwrap();
    assert_same(&samples(source).unwrap(), &adts(MONO));

    let file = media_first_mp4(&tracks, 1000, &[]);
    let result = Mp4Source::new_seekable(Counted::new(Cursor::new(&file), false));
    assert!(matches!(result, Err(Error::Io(_))), "{result:?}");
}

#[test]
fn seekable_reads_from_the_current_position() {
    let prefix = vec![0xAB; 1000];
    let file = [&prefix[..], &media_first_mp4(&[mono_track(10)], 1000, &[])].concat();
    let mut reader = Cursor::new(file);
    reader.seek(SeekFrom::Start(prefix.len() as u64)).unwrap();
    let decoded = samples(Mp4Source::new_seekable(reader).unwrap()).unwrap();
    assert_same(&decoded, &adts(MONO));
}

#[test]
fn seekable_reads_streamable_layouts() {
    let file = faststart_mp4(&[mono_track(10)], 1000, &[], &[]);
    assert_same(&seekable(file).unwrap(), &adts(MONO));
    let (_, frames) = adts_frames(MONO);
    let fragments: Vec<Vec<Vec<u8>>> = frames.chunks(10).map(<[_]>::to_vec).collect();
    let file = fragmented_mp4(&mono_track(1), &fragments, false);
    assert_same(&seekable(file).unwrap(), &adts(MONO));
    assert_same(
        &seekable(CHROME.to_vec()).unwrap(),
        &mp4(CHROME.to_vec()).unwrap(),
    );
}

#[test]
fn seekable_rejects_media_data_without_moov() {
    let file = media_first_mp4(&[mono_track(10)], 1000, &[]);
    let moov_start = file.windows(4).rposition(|w| w == b"moov").unwrap() - 4;
    let mdat_start = file.windows(4).position(|w| w == b"mdat").unwrap() - 4;
    let mut open_ended = file[..moov_start].to_vec();
    open_ended[mdat_start..mdat_start + 4].copy_from_slice(&0u32.to_be_bytes());
    let large_mdat = |size: u64| {
        let mut large = file[..mdat_start].to_vec();
        large.extend(1u32.to_be_bytes());
        large.extend(b"mdat");
        large.extend(size.to_be_bytes());
        large.extend(&file[mdat_start + 8..]);
        large
    };
    for (name, file) in [
        ("cut before moov", file[..moov_start].to_vec()),
        ("open-ended mdat", open_ended),
        ("mdat past the end", large_mdat(1 << 63)),
        ("mdat past the seek range", large_mdat((1 << 63) + 1000)),
    ] {
        let result = seekable(file);
        assert!(
            matches!(result, Err(Error::InvalidInput(_))),
            "{name}: {result:?}"
        );
    }
}

#[test]
fn seekable_reads_media_data_after_moov_too() {
    for rounds_after in [1, 3] {
        let file = media_around_moov(&[mono_track(10)], 1000, &[], rounds_after);
        assert_eq!(file.windows(4).filter(|w| w == b"mdat").count(), 2);
        assert_same(&seekable(file).unwrap(), &adts(MONO));
    }
}

#[test]
fn seekable_rejects_samples_past_a_second_moov_box() {
    let mut file = media_around_moov(&[mono_track(10)], 1000, &[], 1);
    let kind = file.windows(4).rposition(|w| w == b"mdat").unwrap();
    file[kind..kind + 4].copy_from_slice(b"moov");
    let result = seekable(file);
    assert!(
        matches!(&result, Err(Error::InvalidInput(m)) if m.contains("outside")),
        "{result:?}"
    );
}

#[test]
fn cut_between_boxes_decodes_what_is_there() {
    let (_, frames) = adts_frames(MONO);
    let fragments: Vec<Vec<Vec<u8>>> = frames.chunks(10).map(<[_]>::to_vec).collect();
    let file = fragmented_mp4(&mono_track(1), &fragments, false);
    let cut = file.windows(4).rposition(|w| w == b"moof").unwrap() - 4;
    let decoded = mp4(file[..cut].to_vec()).unwrap();
    let kept = (fragments.len() - 1) * 10 * 1024;
    assert_same(&decoded, &adts(MONO)[..kept]);
}

#[test]
fn cut_inside_a_box_is_rejected() {
    let file = faststart_mp4(&[mono_track(10)], 1000, &[], &[]);
    for cut in [file.len() - 100, 200] {
        let result = mp4(file[..cut].to_vec());
        assert!(
            matches!(result, Err(Error::InvalidInput(_))),
            "{cut}: {result:?}"
        );
    }
    let result =
        samples(AdtsAacSource::new(Cursor::new(MONO[..MONO.len() - 10].to_vec())).unwrap());
    assert!(matches!(result, Err(Error::InvalidInput(_))), "{result:?}");
}

#[test]
fn oversized_tables_are_rejected() {
    let mut options = Mp4Options::default();
    options.max_table_bytes = 100;
    let file = faststart_mp4(&[mono_track(1)], 1000, &[], &[]);
    let result = mp4_with(file, options);
    assert!(matches!(result, Err(Error::LimitExceeded(_))), "{result:?}");

    // A trun box can claim billions of samples without listing them.
    let (_, frames) = adts_frames(MONO);
    let mut file = fragmented_mp4(&mono_track(1), &[frames[..2].to_vec()], false);
    let trun = file.windows(4).position(|w| w == b"trun").unwrap();
    file[trun + 5..trun + 8].copy_from_slice(&[0, 0, 0x01]);
    file[trun + 8..trun + 12].copy_from_slice(&u32::MAX.to_be_bytes());
    let result = mp4(file);
    assert!(matches!(result, Err(Error::LimitExceeded(_))), "{result:?}");
}

/// `depth` boxes of `kind`, each the only child of the one before.
fn nested_boxes(kind: &[u8; 4], depth: usize) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(depth * 8);
    for level in 0..depth {
        bytes.extend((((depth - level) * 8) as u32).to_be_bytes());
        bytes.extend(kind);
    }
    bytes
}

/// Recursing into every nested container would overflow the stack, which
/// aborts the process rather than returning an error.
#[test]
fn deep_nesting_is_not_followed() {
    let depth = 200_000;
    let trak = mp4_box(b"trak", &nested_boxes(b"mdia", depth));
    let result = mp4(mp4_box(b"moov", &trak));
    assert!(matches!(result, Err(Error::InvalidInput(_))), "{result:?}");

    let udta = mp4_box(b"udta", &nested_boxes(b"meta", depth));
    let file = faststart_mp4(&[mono_track(10)], 1000, &udta, &[]);
    assert_same(&mp4(file).unwrap(), &adts(MONO));
}

/// Each `trex` box adds a track's defaults, so their number must be bounded
/// for memory not to grow with the input.
#[test]
fn track_defaults_are_bounded() {
    let trex: Vec<u8> = (0..10_000u32)
        .flat_map(|id| full_box(b"trex", 0, 0, &[&id.to_be_bytes()[..], &[0; 16]].concat()))
        .collect();
    let file = faststart_mp4(&[mono_track(10)], 1000, &mp4_box(b"mvex", &trex), &[]);
    let result = mp4(file);
    assert!(matches!(result, Err(Error::Unsupported(_))), "{result:?}");
}

#[test]
fn extreme_values_do_not_overflow() {
    // An iTunSMPB length of u64::MAX leaves nothing to trim at the end.
    let udta = itunsmpb_udta(PRIMING as u32, 0, u64::MAX, true);
    let file = faststart_mp4(&[mono_track(10)], 1000, &udta, &[]);
    assert_same(&mp4(file).unwrap(), &adts(MONO)[PRIMING as usize..]);

    // A chunk offset near u64::MAX puts the next sample past it.
    let mut track = mono_track(10);
    track.wide_offsets = true;
    let mut file = faststart_mp4(&[track], 1000, &[], &[]);
    let co64 = file.windows(4).position(|w| w == b"co64").unwrap();
    file[co64 + 12..co64 + 20].copy_from_slice(&(u64::MAX - 10).to_be_bytes());
    let result = mp4(file);
    assert!(matches!(result, Err(Error::InvalidInput(_))), "{result:?}");
}

#[test]
fn rejects_other_inputs_and_codecs() {
    let ogg = opus_like_garbage();
    assert!(matches!(mp4(ogg.clone()), Err(Error::InvalidInput(_))));
    assert!(matches!(
        AdtsAacSource::new(Cursor::new(ogg)),
        Err(Error::InvalidInput(_))
    ));

    let mut ac3 = mono_track(10);
    ac3.sample_entry = audio_sample_entry(b"ac-3", 1, 48_000, &[]);
    let result = mp4(faststart_mp4(&[ac3], 1000, &[], &[]));
    assert!(
        matches!(&result, Err(Error::Unsupported(m)) if m.contains("ac-3")),
        "{result:?}"
    );

    // 960-sample frames, which Symphonia does not decode.
    let (config, frames) = adts_frames(MONO);
    let mut long_frames = config;
    long_frames[1] |= 0b100;
    let track = Mp4Track::audio(&long_frames, 1, 44_100, &frames, 10);
    let result = mp4(faststart_mp4(&[track], 1000, &[], &[]));
    assert!(matches!(result, Err(Error::Unsupported(_))), "{result:?}");

    let meta = mp4_box(b"meta", &[]);
    let result = mp4(faststart_mp4(&[mono_track(10)], 1000, &meta, &[]));
    assert!(matches!(result, Err(Error::InvalidInput(_))), "{result:?}");

    let mut encrypted = mono_track(10);
    encrypted.sample_entry[4..8].copy_from_slice(b"enca");
    let result = mp4(faststart_mp4(&[encrypted], 1000, &[], &[]));
    assert!(matches!(result, Err(Error::Unsupported(_))), "{result:?}");
}

#[cfg(not(feature = "opus"))]
#[test]
fn opus_needs_its_feature() {
    let mut opus = mono_track(10);
    opus.sample_entry = audio_sample_entry(b"Opus", 1, 48_000, &dops(1, 312, 48_000, 0));
    let result = mp4(faststart_mp4(&[opus], 1000, &[], &[]));
    assert!(
        matches!(&result, Err(Error::Unsupported(m)) if m.contains("opus feature")),
        "{result:?}"
    );
}

/// A track that cannot be decoded is passed over for the next one.
#[cfg(feature = "opus")]
#[test]
fn passes_over_opus_it_cannot_decode() {
    let mut surround = dops(1, 312, 48_000, 0);
    surround[18] = 1;
    let mut opus = Mp4Track::opus(&surround, &[vec![0; 10]], 1);
    opus.id = 2;
    let file = faststart_mp4(&[opus, mono_track(10)], 1000, &[], &[]);
    assert_same(&mp4(file).unwrap(), &adts(MONO));
}

/// Without the feature, an `Opus` track is not parsed, so its missing `dOps`
/// box does not reject the file.
#[cfg(not(feature = "opus"))]
#[test]
fn passes_over_opus_without_its_feature() {
    let mut opus = mono_track(1);
    opus.id = 2;
    opus.sample_entry = audio_sample_entry(b"Opus", 1, 48_000, &[]);
    let file = faststart_mp4(&[opus, mono_track(10)], 1000, &[], &[]);
    assert_same(&mp4(file).unwrap(), &adts(MONO));
}

/// Nothing after the end of the edit is read, so a file cut there decodes.
#[test]
fn stops_at_the_end_of_the_edit() {
    let mut track = mono_track(10);
    track.edit = Some((500, PRIMING as i64));
    let file = faststart_mp4(&[track], 1000, &[], &[]);
    let cut = file[..file.len() - 100].to_vec();
    let expected = &adts(MONO)[PRIMING as usize..PRIMING as usize + 22_050];
    assert_same(&mp4(cut).unwrap(), expected);
}

fn opus_like_garbage() -> Vec<u8> {
    let mut bytes = b"OggS\0\x02".to_vec();
    bytes.extend([0; 60]);
    bytes
}

/// Symphonia decodes AAC-LC only, so HE-AAC yields its core: half the rate,
/// without the SBR layer. Signalling HE-AAC in the config, which Symphonia
/// would reject, gives the same result.
#[test]
fn he_aac_decodes_its_core() {
    let source = Mp4Source::new(Cursor::new(HE_AAC.to_vec())).unwrap();
    assert_eq!((source.sample_rate(), source.channels()), (22_050, 1));
    let core = samples(source).unwrap();
    // iTunSMPB: 1.013 s at the core rate.
    assert_eq!(core.len(), 22_337);

    let (config, frames) = mp4_samples(HE_AAC);
    let untrimmed = mp4(faststart_mp4(
        &[Mp4Track::audio(&config, 1, 22_050, &frames, 10)],
        1000,
        &[],
        &[],
    ))
    .unwrap();
    // Object type 5 at 44.1 kHz around an AAC-LC core at 22.05 kHz, mono.
    let explicit = [0b0010_1011, 0b1000_1010, 0b0000_1000];
    let signalled = mp4(faststart_mp4(
        &[Mp4Track::audio(&explicit, 1, 44_100, &frames, 10)],
        1000,
        &[],
        &[],
    ))
    .unwrap();
    assert_same(&signalled, &untrimmed);
}
