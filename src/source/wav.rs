//! WAV files, read as libsndfile reads them for audiowaveform.

use std::fmt;
use std::io::{self, Read};

use super::{Overflow, PcmSource, invalid, read_full};
use crate::Error;

/// Decoder settings for [`WavSource`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct WavOptions {
    /// Handling of floating-point samples beyond full scale. Integer samples
    /// cannot overflow.
    pub overflow: Overflow,
}

/// Sample encodings, named by their width in the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Encoding {
    U8,
    I16,
    I24,
    I32,
    F32,
    F64,
}

impl Encoding {
    fn width(self) -> usize {
        match self {
            Encoding::U8 => 1,
            Encoding::I16 => 2,
            Encoding::I24 => 3,
            Encoding::I32 | Encoding::F32 => 4,
            Encoding::F64 => 8,
        }
    }
}

const WAVE_FORMAT_PCM: u16 = 0x0001;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// The `KSDATAFORMAT_SUBTYPE_*` GUIDs after their first four bytes, which
/// hold the format tag.
const SUBTYPE_SUFFIX: [u8; 12] = [
    0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
];

/// The libsndfile limit.
const MAX_CHANNELS: u16 = 1024;

/// Bytes of a `fmt ` chunk that are read: the extensible layout. The rest
/// is skipped.
const FMT_BYTES: usize = 40;

/// The `fmt ` chunk fields used for decoding.
struct Format {
    channels: u16,
    sample_rate: u32,
    encoding: Encoding,
}

/// Audio from a WAV file: 8-bit unsigned, 16-, 24- and 32-bit signed integer
/// and 32- and 64-bit floating-point PCM, in `WAVE_FORMAT_PCM`,
/// `WAVE_FORMAT_IEEE_FLOAT` and `WAVE_FORMAT_EXTENSIBLE` layouts, in RIFF
/// and RF64 files.
///
/// Samples are converted to 16 bits as audiowaveform converts them: integer
/// samples keep their 16 most significant bits, and floating-point samples
/// are multiplied by 32767 and truncated toward zero.
///
/// Reading stops at the end of the `data` chunk, so chunks after it are not
/// read. As with audiowaveform, a `data` chunk that is longer than the file
/// is read up to the end of the file, such as one with the `0xFFFFFFFF` size
/// that FFmpeg writes to a pipe. A `data` size of 0 also reads to the end of
/// the file if the RIFF size is 8; any other empty `data` chunk gives no
/// samples, without an error.
pub struct WavSource<R: Read> {
    reader: R,
    sample_rate: u32,
    channels: u16,
    encoding: Encoding,
    overflow: Overflow,
    /// Bytes of the `data` chunk not read yet.
    remaining: u64,
    bytes: Vec<u8>,
    samples: Vec<i16>,
}

impl<R: Read> WavSource<R> {
    const BLOCK_BYTES: usize = 64 * 1024;

    /// Reads the WAV header from `reader`, with the default options.
    ///
    /// # Errors
    ///
    /// As for [`with_options`](Self::with_options).
    pub fn new(reader: R) -> Result<Self, Error> {
        Self::with_options(reader, WavOptions::default())
    }

    /// Reads the WAV header from `reader`, up to the start of the audio.
    ///
    /// # Errors
    ///
    /// - [`Error::Io`] when reading fails.
    /// - [`Error::InvalidInput`] when the input is not a WAV file, or its
    ///   header is malformed or ends before the `data` chunk.
    /// - [`Error::Unsupported`] for other sample formats, such as A-law or
    ///   ADPCM, for big-endian RIFX files, and for more than 1024 channels.
    pub fn with_options(mut reader: R, options: WavOptions) -> Result<Self, Error> {
        let mut header = [0u8; 12];
        if read_full(&mut reader, &mut header)? < header.len() {
            return Err(invalid("not a WAV file: too short"));
        }
        let rf64 = match &header[..4] {
            b"RIFF" => false,
            b"RF64" | b"BW64" => true,
            b"RIFX" => return Err(Error::Unsupported("big-endian RIFX WAV file".to_owned())),
            _ => return Err(invalid("not a WAV file: no RIFF header")),
        };
        if &header[8..] != b"WAVE" {
            return Err(invalid("not a WAV file: no WAVE form type"));
        }
        let riff_size = u32_at(&header, 4);

        let mut format = None;
        let mut ds64_data_size = None;
        let (format, remaining) = loop {
            let mut chunk = [0u8; 8];
            if read_full(&mut reader, &mut chunk)? < chunk.len() {
                return Err(invalid("WAV file without a data chunk"));
            }
            let size = u32_at(&chunk, 4);
            let pad = (size & 1) as u64;
            match &chunk[..4] {
                // libsndfile loses its place in the file at a second one.
                b"fmt " if format.is_some() => {
                    return Err(invalid("WAV file with two fmt chunks"));
                }
                b"fmt " => format = Some(read_fmt(&mut reader, size)?),
                b"ds64" if rf64 && ds64_data_size.is_none() => {
                    let mut ds64 = [0u8; 16];
                    if size < 16 {
                        return Err(invalid("ds64 chunk shorter than 16 bytes"));
                    }
                    read_exact(&mut reader, &mut ds64)?;
                    skip(&mut reader, size as u64 - 16 + pad)?;
                    ds64_data_size = Some(u64::from_le_bytes(ds64[8..].try_into().unwrap()));
                }
                b"data" => {
                    let Some(format) = format else {
                        return Err(invalid("WAV data chunk before the fmt chunk"));
                    };
                    let remaining = match (ds64_data_size, size) {
                        // libsndfile ignores the size in the chunk once it has a ds64.
                        (Some(ds64_size), _) => ds64_size,
                        (None, u32::MAX) if rf64 => {
                            return Err(invalid("RF64 file without a ds64 chunk"));
                        }
                        // libsndfile reads a file left with its first header to the end.
                        (None, 0) if riff_size == 8 => u64::MAX,
                        (None, _) => size as u64,
                    };
                    break (format, remaining);
                }
                _ => skip(&mut reader, size as u64 + pad)?,
            }
        };
        let Format {
            channels,
            sample_rate,
            encoding,
        } = format;

        let frame_bytes = channels as usize * encoding.width();
        let block_bytes = (Self::BLOCK_BYTES / frame_bytes).max(1) * frame_bytes;
        Ok(WavSource {
            reader,
            sample_rate,
            channels,
            encoding,
            overflow: options.overflow,
            remaining,
            bytes: vec![0; block_bytes],
            samples: Vec::with_capacity(block_bytes / encoding.width()),
        })
    }
}

/// Reads a `fmt ` chunk of `size` bytes and its pad byte.
fn read_fmt<R: Read>(reader: &mut R, size: u32) -> Result<Format, Error> {
    if size < 16 {
        return Err(invalid("WAV fmt chunk shorter than 16 bytes"));
    }
    let len = (size as usize).min(FMT_BYTES);
    let mut fmt = [0u8; FMT_BYTES];
    read_exact(reader, &mut fmt[..len])?;
    skip(reader, (size as usize - len) as u64 + (size & 1) as u64)?;

    let mut tag = u16_at(&fmt, 0);
    let channels = u16_at(&fmt, 2);
    let sample_rate = u32_at(&fmt, 4);
    let block_align = u16_at(&fmt, 12);
    let bits = u16_at(&fmt, 14);
    if tag == WAVE_FORMAT_EXTENSIBLE {
        if len < FMT_BYTES {
            return Err(invalid(
                "WAVE_FORMAT_EXTENSIBLE fmt chunk shorter than 40 bytes",
            ));
        }
        let sub_format = u32_at(&fmt, 24);
        tag = match u16::try_from(sub_format) {
            Ok(sub_tag) if fmt[28..] == SUBTYPE_SUFFIX => sub_tag,
            _ => {
                return Err(Error::Unsupported(
                    "WAVE_FORMAT_EXTENSIBLE sub-format with an unknown GUID".to_owned(),
                ));
            }
        };
    }
    if channels == 0 {
        return Err(invalid("WAV file with no channels"));
    }
    if channels > MAX_CHANNELS {
        return Err(Error::Unsupported(format!(
            "WAV file with {channels} channels"
        )));
    }
    if sample_rate == 0 {
        return Err(invalid("WAV file with a zero sample rate"));
    }
    if bits == 0 {
        return Err(invalid("WAV file with zero bits per sample"));
    }
    // As libsndfile does, the width comes from the bits per sample, not the
    // block alignment.
    let encoding = match (tag, bits.div_ceil(8)) {
        // libsndfile guesses what such a file holds from its samples.
        (WAVE_FORMAT_PCM, 3) if block_align == channels.saturating_mul(4) => {
            return Err(Error::Unsupported(
                "WAV file with 24-bit samples in 32-bit blocks".to_owned(),
            ));
        }
        (WAVE_FORMAT_PCM, 1) => Encoding::U8,
        (WAVE_FORMAT_PCM, 2) => Encoding::I16,
        (WAVE_FORMAT_PCM, 3) => Encoding::I24,
        (WAVE_FORMAT_PCM, 4) => Encoding::I32,
        (WAVE_FORMAT_IEEE_FLOAT, 4) => Encoding::F32,
        (WAVE_FORMAT_IEEE_FLOAT, 8) => Encoding::F64,
        (WAVE_FORMAT_PCM | WAVE_FORMAT_IEEE_FLOAT, _) => {
            return Err(Error::Unsupported(format!(
                "WAV file with {bits}-bit samples"
            )));
        }
        _ => {
            return Err(Error::Unsupported(format!("WAV format tag {tag:#06x}")));
        }
    };
    Ok(Format {
        channels,
        sample_rate,
        encoding,
    })
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn read_exact<R: Read>(reader: &mut R, buf: &mut [u8]) -> Result<(), Error> {
    if read_full(reader, buf)? < buf.len() {
        return Err(invalid("WAV file ends before its data chunk"));
    }
    Ok(())
}

fn skip<R: Read>(reader: &mut R, len: u64) -> Result<(), Error> {
    if io::copy(&mut reader.take(len), &mut io::sink())? < len {
        return Err(invalid("WAV file ends before its data chunk"));
    }
    Ok(())
}

impl<R: Read> fmt::Debug for WavSource<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WavSource")
            .field("sample_rate", &self.sample_rate)
            .field("channels", &self.channels)
            .field("encoding", &self.encoding)
            .finish_non_exhaustive()
    }
}

impl<R: Read> PcmSource for WavSource<R> {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn next_block(&mut self) -> Result<Option<&[i16]>, Error> {
        let wanted = (self.bytes.len() as u64).min(self.remaining) as usize;
        let filled = read_full(&mut self.reader, &mut self.bytes[..wanted])?;
        self.remaining = if filled < wanted {
            0
        } else {
            self.remaining - filled as u64
        };
        // A partial frame at the end is dropped, as libsndfile does.
        let frame_bytes = self.channels as usize * self.encoding.width();
        let bytes = &self.bytes[..filled - filled % frame_bytes];
        if bytes.is_empty() {
            return Ok(None);
        }
        let overflow = self.overflow;
        let float = move |value: f32| {
            let scaled = value * 32767.0;
            match overflow {
                // Float-to-int casts saturate.
                Overflow::Saturate => scaled as i16,
                Overflow::Wrap => scaled as i32 as i16,
            }
        };
        self.samples.clear();
        match self.encoding {
            Encoding::U8 => self
                .samples
                .extend(bytes.iter().map(|&byte| (byte as i16 - 128) << 8)),
            Encoding::I16 => self.samples.extend(
                bytes
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|&pair| i16::from_le_bytes(pair)),
            ),
            Encoding::I24 => self.samples.extend(
                bytes
                    .as_chunks::<3>()
                    .0
                    .iter()
                    .map(|&[_, low, high]| i16::from_le_bytes([low, high])),
            ),
            Encoding::I32 => self.samples.extend(
                bytes
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|&[_, _, low, high]| i16::from_le_bytes([low, high])),
            ),
            Encoding::F32 => self.samples.extend(
                bytes
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|&value| float(f32::from_le_bytes(value))),
            ),
            // audiowaveform rounds doubles to float before it scales them.
            Encoding::F64 => self.samples.extend(
                bytes
                    .as_chunks::<8>()
                    .0
                    .iter()
                    .map(|&value| float(f64::from_le_bytes(value) as f32)),
            ),
        }
        Ok(Some(&self.samples))
    }
}
