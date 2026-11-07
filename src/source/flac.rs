//! FLAC files, read as libsndfile reads them for audiowaveform.

use std::fmt;
use std::io::{self, ErrorKind, Read};
use std::mem;

use claxon::frame::FrameReader;
use claxon::input::ReadBytes;

use super::{PcmSource, invalid, skip_id3v2};
use crate::Error;

const STREAMINFO: u8 = 0;
const STREAMINFO_BYTES: usize = 34;
const LAST_BLOCK: u8 = 0x80;
/// The channel count minus one, in byte 12 of `STREAMINFO`.
const CHANNEL_BITS: u8 = 0b1110;
/// A 6-byte header, a 2-byte constant subframe and the CRC-16.
const MIN_FRAME_BYTES: u64 = 10;
const MAX_BLOCK_SIZE: u64 = 65535;
/// With a 7-byte frame number, the block size, the sample rate and the CRC-8.
const MAX_HEADER_BYTES: usize = 16;
/// Silence is given in blocks of this many frames.
const SILENCE_FRAMES: u64 = 4096;
/// The samples that frames which do not decode can claim before any output:
/// 64 of the largest frames.
const FREE_FAILED_SAMPLES: u64 = 64 * 65536 * 8;

/// Audio from a FLAC file with 8-, 16- or 24-bit samples, decoded with
/// [claxon](https://crates.io/crates/claxon).
///
/// Samples are converted to 16 bits as audiowaveform converts them: they keep
/// their 16 most significant bits, by the sample width of each frame. ID3v2
/// tags before the `fLaC` marker are skipped, and so are all metadata blocks
/// other than `STREAMINFO`, without buffering them.
///
/// As with audiowaveform, decoding stops without an error after the number
/// of samples in `STREAMINFO`, when it is set, and at the end of the input.
/// A frame cut short by the end of the input is dropped, so a truncated
/// upload gives the frames before the cut. Decoding also stops at a frame
/// with another channel count, or gives [`Error::InvalidInput`] if it is the
/// first frame.
///
/// After a frame that does not decode, such as one with a CRC mismatch or
/// junk in its place, decoding continues at the next frame that decodes.
/// The frames lost in between are replaced with silence, so that the
/// waveform stays in time with the audio. audiowaveform instead stops within
/// a few frames, or gives no audio at all. A frame with features that claxon
/// does not decode, such as escape-coded residuals, is also replaced with
/// silence, because claxon reports these features for damage too. If no
/// frame decodes, decoding gives [`Error::Unsupported`] for these features,
/// and [`Error::InvalidInput`] otherwise.
pub struct FlacSource<R: Read> {
    input: Input<R>,
    sample_rate: u32,
    channels: u16,
    bits: u32,
    /// Frames left before the total in `STREAMINFO`, if it is set.
    remaining: Option<u64>,
    /// The block size of a fixed-block-size stream, from `STREAMINFO`, or
    /// else from the first frame.
    block_size: u32,
    /// The most frames that the bytes skipped after damage can hold.
    max_block_size: u64,
    min_frame_bytes: u64,
    /// The stream time after the last frame that decoded.
    next_time: Option<u64>,
    /// The start of the bytes skipped since a frame did not decode.
    damage: Option<u64>,
    damaged: bool,
    /// A feature that claxon does not decode, from a frame that did not
    /// decode.
    unsupported: Option<&'static str>,
    /// Samples given, with silence.
    given: u64,
    /// Samples that the headers of frames which did not decode claim. For
    /// each, claxon allocates and zeroes a buffer.
    failed: u64,
    done: bool,
    decoded_any: bool,
    /// Frames of silence to give before `samples`.
    silence: u64,
    /// Whether `samples` holds a frame to give.
    queued: bool,
    /// Claxon's planar output, reused for each frame.
    block: Vec<i32>,
    samples: Vec<i16>,
    zeros: Vec<i16>,
}

impl<R: Read> FlacSource<R> {
    /// Reads the FLAC header from `reader`, up to the first frame.
    ///
    /// # Errors
    ///
    /// - [`Error::Io`] when reading fails.
    /// - [`Error::InvalidInput`] when the input is not a FLAC file, has no
    ///   valid `STREAMINFO` block, or ends inside its metadata.
    /// - [`Error::Unsupported`] for samples of other widths than 8, 16 and
    ///   24 bits, which audiowaveform also rejects.
    pub fn new(mut reader: R) -> Result<Self, Error> {
        let (start, read) = skip_id3v2(&mut reader)?;
        let mut input = Input::new(reader);
        if &start[..read] != b"fLa"
            || input
                .read_u8_or_eof()
                .map_err(|err| input.header_error(err))?
                != Some(b'C')
        {
            return Err(invalid("not a FLAC file: no fLaC marker"));
        }

        // As with libsndfile, a later STREAMINFO block replaces an earlier
        // one, but cannot change the channel count.
        let mut streaminfo: Option<[u8; STREAMINFO_BYTES]> = None;
        loop {
            let mut header = [0u8; 4];
            input.header_bytes(&mut header)?;
            let length = u32::from_be_bytes([0, header[1], header[2], header[3]]);
            if header[0] & !LAST_BLOCK == STREAMINFO {
                if (length as usize) < STREAMINFO_BYTES {
                    return Err(invalid("FLAC STREAMINFO block shorter than 34 bytes"));
                }
                let mut block = [0u8; STREAMINFO_BYTES];
                input.header_bytes(&mut block)?;
                input.skip_header(length - STREAMINFO_BYTES as u32)?;
                if let Some(earlier) = streaminfo
                    && (earlier[12] ^ block[12]) & CHANNEL_BITS != 0
                {
                    return Err(invalid(
                        "FLAC file with STREAMINFO blocks of different channel counts",
                    ));
                }
                streaminfo = Some(block);
            } else {
                input.skip_header(length)?;
            }
            if header[0] & LAST_BLOCK != 0 {
                break;
            }
        }
        let Some(info) = streaminfo else {
            return Err(invalid("FLAC file without a STREAMINFO block"));
        };

        let sample_rate = u32::from_be_bytes([0, info[10], info[11], info[12]]) >> 4;
        let channels = ((info[12] & CHANNEL_BITS) >> 1) as u16 + 1;
        let bits = ((info[12] as u32 & 1) << 4 | info[13] as u32 >> 4) + 1;
        let total = u64::from_be_bytes([
            0,
            0,
            0,
            info[13] & 0x0F,
            info[14],
            info[15],
            info[16],
            info[17],
        ]);
        if sample_rate == 0 {
            return Err(invalid("FLAC file with a zero sample rate"));
        }
        if !matches!(bits, 8 | 16 | 24) {
            return Err(Error::Unsupported(format!(
                "FLAC file with {bits}-bit samples"
            )));
        }
        let min_block_size = u16::from_be_bytes([info[0], info[1]]);
        let max_block_size = u16::from_be_bytes([info[2], info[3]]);
        let min_frame_bytes = u32::from_be_bytes([0, info[4], info[5], info[6]]);
        Ok(FlacSource {
            input,
            sample_rate,
            channels,
            bits,
            remaining: (total != 0).then_some(total),
            block_size: if min_block_size == max_block_size {
                min_block_size.into()
            } else {
                0
            },
            max_block_size: if max_block_size >= 16 {
                max_block_size.into()
            } else {
                MAX_BLOCK_SIZE
            },
            min_frame_bytes: u64::from(min_frame_bytes).max(MIN_FRAME_BYTES),
            next_time: None,
            damage: None,
            damaged: false,
            unsupported: None,
            given: 0,
            failed: 0,
            done: false,
            decoded_any: false,
            silence: 0,
            queued: false,
            block: Vec::new(),
            samples: Vec::new(),
            zeros: Vec::new(),
        })
    }

    /// Takes up to `frames` frames from the total in `STREAMINFO`.
    fn take(&mut self, frames: u64) -> u64 {
        match &mut self.remaining {
            Some(remaining) => {
                let frames = frames.min(*remaining);
                *remaining -= frames;
                frames
            }
            None => frames,
        }
    }

    /// The most frames that `skipped` bytes can hold: a bound for a wrong
    /// frame number, or a stream that does not start at time 0.
    fn max_lost(&self, skipped: u64) -> u64 {
        skipped
            .div_ceil(self.min_frame_bytes)
            .saturating_mul(self.max_block_size)
    }

    /// Stops decoding, with an error if no frame decoded after damage.
    fn stop(&mut self) -> Result<(), Error> {
        self.done = true;
        if self.damaged && !self.decoded_any {
            return Err(match self.unsupported {
                Some(feature) => Error::Unsupported(format!("FLAC frames with {feature}")),
                None => invalid("no frame of the FLAC file decodes"),
            });
        }
        Ok(())
    }

    /// Decodes the next frame into `samples`, after the frames of silence
    /// that replace the frames lost to damage.
    fn decode(&mut self) -> Result<(), Error> {
        loop {
            if let Some(damage) = self.damage
                && !self.input.find_frame(self.channels)?
            {
                // The total in `STREAMINFO` gives the frames lost at the end.
                if let Some(remaining) = self.remaining {
                    self.silence = remaining.min(self.max_lost(self.input.offset() - damage));
                    self.take(self.silence);
                }
                return self.stop();
            }
            let start = self.input.offset();
            // Claxon does not report the channel count, sample width and
            // block size strategy of a frame header, so they are read
            // before it decodes the frame.
            let mut bits = self.bits;
            let mut variable = false;
            let mut claimed = 0;
            // Without a sync code, claxon rejects the bytes as a frame.
            if let &[0xFF, sync @ (0xF8 | 0xF9), sizes, byte] = self.input.peek(4)? {
                variable = sync == 0xF9;
                let channels = channel_count(byte >> 4);
                claimed = max_block_size(sizes >> 4) * u64::from(channels);
                // claxon rejects the reserved values.
                let channels = if channels == 0 {
                    self.channels
                } else {
                    channels
                };
                if channels != self.channels {
                    self.done = true;
                    if !self.decoded_any {
                        return Err(Error::InvalidInput(format!(
                            "first FLAC frame has {channels} channels, STREAMINFO {}",
                            self.channels
                        )));
                    }
                    return Ok(());
                }
                bits = match (byte >> 1) & 0b111 {
                    0b001 => 8,
                    0b010 => 12,
                    0b100 => 16,
                    0b101 => 20,
                    0b110 => 24,
                    // claxon rejects the frame.
                    _ => bits,
                };
            }

            let buffer = mem::take(&mut self.block);
            let block = match FrameReader::new(&mut self.input).read_next_or_eof(buffer) {
                Ok(Some(block)) => block,
                Ok(None) => return self.stop(),
                // The reader can return its end and then an error.
                Err(claxon::Error::IoError(err))
                    if !self.input.at_end || err.kind() != ErrorKind::UnexpectedEof =>
                {
                    self.done = true;
                    return Err(err.into());
                }
                Err(claxon::Error::IoError(_)) => return self.stop(),
                Err(err) => {
                    // Claxon reports these features before it checks the CRC,
                    // so damage often looks like them.
                    if let claxon::Error::Unsupported(feature) = err {
                        self.unsupported.get_or_insert(feature);
                    }
                    self.failed = self.failed.saturating_add(claimed);
                    if self.failed > self.given.saturating_add(FREE_FAILED_SAMPLES) {
                        self.damaged = true;
                        return self.stop();
                    }
                    self.damage.get_or_insert(start);
                    self.damaged = true;
                    continue;
                }
            };

            let duration = block.duration();
            if self.block_size == 0 {
                self.block_size = duration;
            }
            let time = if variable {
                block.time()
            } else {
                // Claxon multiplies the frame number by the size of this
                // block, which is shorter for the last one.
                (block.time() / u64::from(duration)).saturating_mul(self.block_size.into())
            };
            if let Some(damage) = self.damage.take() {
                let gap = time.saturating_sub(self.next_time.unwrap_or(0));
                self.silence = self.take(gap.min(self.max_lost(start - damage)));
            }
            let channels = self.channels as usize;
            let frames = self.take(duration.into()) as usize;
            self.given = self
                .given
                .saturating_add((self.silence + frames as u64) * channels as u64);
            self.next_time = Some(time.saturating_add(duration.into()));

            self.samples.clear();
            self.samples.resize(frames * channels, 0);
            for channel in 0..channels {
                let decoded = &block.channel(channel as u32)[..frames];
                let out = self.samples.iter_mut().skip(channel).step_by(channels);
                if bits > 16 {
                    let shift = bits - 16;
                    for (out, &sample) in out.zip(decoded) {
                        *out = (sample >> shift) as i16;
                    }
                } else {
                    let shift = 16 - bits;
                    for (out, &sample) in out.zip(decoded) {
                        *out = (sample << shift) as i16;
                    }
                }
            }
            self.block = block.into_buffer();
            self.decoded_any = true;
            self.queued = frames > 0;
            return Ok(());
        }
    }
}

impl<R: Read> fmt::Debug for FlacSource<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FlacSource")
            .field("sample_rate", &self.sample_rate)
            .field("channels", &self.channels)
            .field("bits", &self.bits)
            .finish_non_exhaustive()
    }
}

impl<R: Read> PcmSource for FlacSource<R> {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn next_block(&mut self) -> Result<Option<&[i16]>, Error> {
        loop {
            if self.silence > 0 {
                let frames = self.silence.min(SILENCE_FRAMES);
                self.silence -= frames;
                let len = frames as usize * self.channels as usize;
                if self.zeros.len() < len {
                    self.zeros.resize(len, 0);
                }
                return Ok(Some(&self.zeros[..len]));
            }
            if mem::take(&mut self.queued) {
                return Ok(Some(&self.samples));
            }
            if self.done || self.remaining == Some(0) {
                return Ok(None);
            }
            self.decode()?;
        }
    }
}

/// A buffered reader that claxon decodes frames from, and that can show the
/// next frame header before claxon reads it.
struct Input<R> {
    reader: R,
    buf: Box<[u8]>,
    /// The input offset of `buf[0]`.
    base: u64,
    pos: usize,
    end: usize,
    /// Whether `reader` returned its end, so an `UnexpectedEof` error from
    /// claxon is a truncated frame and not an error from `reader`.
    at_end: bool,
}

impl<R: Read> Input<R> {
    const CAPACITY: usize = 16 * 1024;

    fn new(reader: R) -> Self {
        Input {
            reader,
            buf: vec![0; Self::CAPACITY].into_boxed_slice(),
            base: 0,
            pos: 0,
            end: 0,
            at_end: false,
        }
    }

    /// Reads more into the empty buffer. Returns `false` at the end.
    #[cold]
    fn refill(&mut self) -> io::Result<bool> {
        self.base += self.end as u64;
        self.pos = 0;
        self.end = 0;
        let read = read_retrying(&mut self.reader, &mut self.buf)?;
        self.end = read;
        self.at_end |= read == 0;
        Ok(read > 0)
    }

    /// The next `len` bytes, or fewer at the end, without consuming them.
    fn peek(&mut self, len: usize) -> io::Result<&[u8]> {
        if self.end - self.pos < len {
            self.base += self.pos as u64;
            self.buf.copy_within(self.pos..self.end, 0);
            self.end -= self.pos;
            self.pos = 0;
            while self.end < len {
                let read = read_retrying(&mut self.reader, &mut self.buf[self.end..])?;
                if read == 0 {
                    self.at_end = true;
                    break;
                }
                self.end += read;
            }
        }
        Ok(&self.buf[self.pos..self.end.min(self.pos + len)])
    }

    fn offset(&self) -> u64 {
        self.base + self.pos as u64
    }

    /// Moves to the next frame header with `channels` channels. Returns
    /// `false` at the end of the input.
    fn find_frame(&mut self, channels: u16) -> io::Result<bool> {
        loop {
            match self.buf[self.pos..self.end]
                .iter()
                .position(|&byte| byte == 0xFF)
            {
                Some(at) => self.pos += at,
                None => {
                    self.pos = self.end;
                    if !self.refill()? {
                        return Ok(false);
                    }
                    continue;
                }
            }
            if is_frame_header(self.peek(MAX_HEADER_BYTES)?, channels) {
                return Ok(true);
            }
            self.pos += 1;
        }
    }

    fn header_error(&self, err: io::Error) -> Error {
        if self.at_end && err.kind() == ErrorKind::UnexpectedEof {
            invalid("FLAC file ends inside its metadata")
        } else {
            err.into()
        }
    }

    fn header_bytes(&mut self, buf: &mut [u8]) -> Result<(), Error> {
        self.read_into(buf).map_err(|err| self.header_error(err))
    }

    fn skip_header(&mut self, len: u32) -> Result<(), Error> {
        self.skip(len).map_err(|err| self.header_error(err))
    }
}

/// The most frames for a block size code: codes 6 and 7 take the size from
/// the bytes after the frame number.
fn max_block_size(code: u8) -> u64 {
    match code {
        0 => 0,
        1 => 192,
        2..=5 => 576 << (code - 2),
        6 => 256,
        7 => 65536,
        _ => 256 << (code - 8),
    }
}

/// The channel count for a channel code, or 0 for a reserved value.
fn channel_count(code: u8) -> u16 {
    match code {
        independent @ 0..=7 => independent as u16 + 1,
        8..=10 => 2,
        _ => 0,
    }
}

/// Whether `header` starts with a frame header with `channels` channels and
/// a valid CRC-8.
fn is_frame_header(header: &[u8], channels: u16) -> bool {
    let &[0xFF, 0xF8 | 0xF9, sizes, layout, number, ..] = header else {
        return false;
    };
    let (block_size, rate) = (sizes >> 4, sizes & 0x0F);
    if block_size == 0
        || rate == 0x0F
        || channel_count(layout >> 4) != channels
        || matches!((layout >> 1) & 0b111, 0b011 | 0b111)
        || layout & 1 != 0
    {
        return false;
    }
    let number_bytes = match number.leading_ones() {
        0 => 1,
        ones @ 2..=7 => ones as usize,
        _ => return false,
    };
    let len = 4
        + number_bytes
        + match block_size {
            6 => 1,
            7 => 2,
            _ => 0,
        }
        + match rate {
            12 => 1,
            13 | 14 => 2,
            _ => 0,
        };
    header.len() > len
        && header[5..4 + number_bytes]
            .iter()
            .all(|&byte| byte & 0xC0 == 0x80)
        && crc8(&header[..len]) == header[len]
}

fn crc8(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0, |crc, &byte| {
        (0..8).fold(crc ^ byte, |crc, _| {
            if crc & 0x80 != 0 {
                crc << 1 ^ 0x07
            } else {
                crc << 1
            }
        })
    })
}

fn read_retrying<R: Read>(reader: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        match reader.read(buf) {
            Err(err) if err.kind() == ErrorKind::Interrupted => {}
            result => return result,
        }
    }
}

fn end_of_input() -> io::Error {
    io::Error::new(ErrorKind::UnexpectedEof, "FLAC input ends inside a frame")
}

impl<R: Read> ReadBytes for Input<R> {
    #[inline(always)]
    fn read_u8(&mut self) -> io::Result<u8> {
        if self.pos == self.end && !self.refill()? {
            return Err(end_of_input());
        }
        let byte = self.buf[self.pos];
        self.pos += 1;
        Ok(byte)
    }

    fn read_u8_or_eof(&mut self) -> io::Result<Option<u8>> {
        if self.pos == self.end && !self.refill()? {
            return Ok(None);
        }
        let byte = self.buf[self.pos];
        self.pos += 1;
        Ok(Some(byte))
    }

    fn read_into(&mut self, mut buffer: &mut [u8]) -> io::Result<()> {
        while !buffer.is_empty() {
            if self.pos == self.end && !self.refill()? {
                return Err(end_of_input());
            }
            let count = buffer.len().min(self.end - self.pos);
            buffer[..count].copy_from_slice(&self.buf[self.pos..self.pos + count]);
            self.pos += count;
            buffer = &mut buffer[count..];
        }
        Ok(())
    }

    fn skip(&mut self, mut amount: u32) -> io::Result<()> {
        while amount > 0 {
            if self.pos == self.end && !self.refill()? {
                return Err(end_of_input());
            }
            let count = (amount as usize).min(self.end - self.pos);
            self.pos += count;
            amount -= count as u32;
        }
        Ok(())
    }
}
