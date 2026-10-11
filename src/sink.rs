use std::io::{BufWriter, Cursor, Seek, SeekFrom, Write};

use crate::{Bits, Error, Header, Peak};

/// Receives the header, then each point as soon as it completes, then the
/// final point count.
pub trait PeakSink {
    /// Value returned by [`finish`](Self::finish).
    type Output;

    /// Called once, before any point.
    ///
    /// # Errors
    ///
    /// Any error stops generation and is returned to the caller, typically
    /// [`Error::Io`] when writing fails.
    fn begin(&mut self, header: &Header) -> Result<(), Error>;

    /// Called once per output point with one [`Peak`] per output channel.
    ///
    /// # Errors
    ///
    /// As for [`begin`](Self::begin).
    fn point(&mut self, peaks: &[Peak]) -> Result<(), Error>;

    /// Called once, after the last point, with the number of points.
    ///
    /// # Errors
    ///
    /// As for [`begin`](Self::begin).
    fn finish(self, length: u32) -> Result<Self::Output, Error>;
}

/// Writes the audiowaveform binary `.dat` format.
///
/// The header goes first with a zero length, the points follow as they
/// complete, and [`finish`](PeakSink::finish) seeks back to fill in the
/// length. That needs `Seek`, but nothing is held in memory beyond a write
/// buffer. Writes are buffered internally, so pass a `File` directly.
///
/// If generation fails, the output holds a header with a zero length and the
/// points written so far. Write to a temporary file and rename it on success.
///
/// The version follows audiowaveform: 1 for one output channel (20-byte
/// header), 2 otherwise (24-byte header with a channel count).
#[derive(Debug)]
pub struct DatWriter<W: Write + Seek> {
    inner: BufWriter<W>,
    start: u64,
    bits: Bits,
}

impl<W: Write + Seek> DatWriter<W> {
    /// Writes to `inner`, starting at its current position.
    pub fn new(inner: W) -> Self {
        DatWriter {
            inner: BufWriter::new(inner),
            start: 0,
            bits: Bits::Sixteen,
        }
    }
}

impl<W: Write + Seek> PeakSink for DatWriter<W> {
    type Output = W;

    fn begin(&mut self, header: &Header) -> Result<(), Error> {
        self.start = self.inner.stream_position()?;
        self.bits = header.bits;
        let version: i32 = if header.channels == 1 { 1 } else { 2 };
        let flags: u32 = if header.bits == Bits::Eight { 1 } else { 0 };
        let w = &mut self.inner;
        w.write_all(&version.to_le_bytes())?;
        w.write_all(&flags.to_le_bytes())?;
        w.write_all(&header.sample_rate.to_le_bytes())?;
        w.write_all(&header.samples_per_pixel.to_le_bytes())?;
        w.write_all(&0u32.to_le_bytes())?;
        if version == 2 {
            w.write_all(&(header.channels as u32).to_le_bytes())?;
        }
        Ok(())
    }

    fn point(&mut self, peaks: &[Peak]) -> Result<(), Error> {
        for peak in peaks {
            match self.bits {
                Bits::Eight => self
                    .inner
                    .write_all(&[peak.min as i8 as u8, peak.max as i8 as u8])?,
                Bits::Sixteen => {
                    self.inner.write_all(&peak.min.to_le_bytes())?;
                    self.inner.write_all(&peak.max.to_le_bytes())?;
                }
            }
        }
        Ok(())
    }

    fn finish(mut self, length: u32) -> Result<W, Error> {
        let end = self.inner.stream_position()?;
        self.inner.seek(SeekFrom::Start(self.start + 16))?;
        self.inner.write_all(&length.to_le_bytes())?;
        self.inner.seek(SeekFrom::Start(end))?;
        self.inner
            .into_inner()
            .map_err(|err| Error::Io(err.into_error()))
    }
}

/// Writes the audiowaveform JSON format.
///
/// The object keys are the ones audiowaveform writes, but `length` comes after
/// `data` so the output can stream without `Seek`. JSON readers do not depend
/// on key order. `version` is always 2, as in audiowaveform.
///
/// If generation fails, the output is truncated JSON. Write to a temporary
/// file and rename it on success.
#[derive(Debug)]
pub struct JsonWriter<W: Write> {
    inner: BufWriter<W>,
    first: bool,
}

impl<W: Write> JsonWriter<W> {
    /// Writes to `inner`. Writes are buffered internally.
    pub fn new(inner: W) -> Self {
        JsonWriter {
            inner: BufWriter::new(inner),
            first: true,
        }
    }
}

impl<W: Write> PeakSink for JsonWriter<W> {
    type Output = W;

    fn begin(&mut self, header: &Header) -> Result<(), Error> {
        let bits = match header.bits {
            Bits::Eight => 8,
            Bits::Sixteen => 16,
        };
        write!(
            self.inner,
            r#"{{"version":2,"channels":{},"sample_rate":{},"samples_per_pixel":{},"bits":{},"data":["#,
            header.channels, header.sample_rate, header.samples_per_pixel, bits
        )?;
        Ok(())
    }

    fn point(&mut self, peaks: &[Peak]) -> Result<(), Error> {
        for peak in peaks {
            let separator = if self.first { "" } else { "," };
            self.first = false;
            write!(self.inner, "{separator}{},{}", peak.min, peak.max)?;
        }
        Ok(())
    }

    fn finish(mut self, length: u32) -> Result<W, Error> {
        write!(self.inner, r#"],"length":{length}}}"#)?;
        self.inner
            .into_inner()
            .map_err(|err| Error::Io(err.into_error()))
    }
}

/// Collects the waveform in memory.
///
/// Output size is `length × channels × 2` values, so prefer [`DatWriter`] or
/// [`JsonWriter`] for long inputs at fine zoom levels.
///
/// # Panics
///
/// [`finish`](PeakSink::finish) panics if [`begin`](PeakSink::begin) was not
/// called first. [`generate`](crate::generate) and
/// [`PeakBuilder`](crate::PeakBuilder) always call it.
#[derive(Debug, Default)]
pub struct MemorySink {
    header: Option<Header>,
    data: Vec<i16>,
}

impl MemorySink {
    /// An empty sink.
    #[must_use]
    pub fn new() -> Self {
        MemorySink::default()
    }
}

impl PeakSink for MemorySink {
    type Output = Waveform;

    fn begin(&mut self, header: &Header) -> Result<(), Error> {
        self.header = Some(*header);
        Ok(())
    }

    fn point(&mut self, peaks: &[Peak]) -> Result<(), Error> {
        for peak in peaks {
            self.data.push(peak.min);
            self.data.push(peak.max);
        }
        Ok(())
    }

    fn finish(self, length: u32) -> Result<Waveform, Error> {
        let header = self
            .header
            .expect("PeakSink::begin is called before finish");
        Ok(Waveform {
            header,
            length,
            data: self.data,
        })
    }
}

/// A waveform collected by [`MemorySink`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Waveform {
    header: Header,
    length: u32,
    data: Vec<i16>,
}

impl Waveform {
    /// Sample rate, zoom, bit depth and output channel count.
    #[must_use]
    pub fn header(&self) -> &Header {
        &self.header
    }

    /// Number of points.
    #[must_use]
    pub fn length(&self) -> u32 {
        self.length
    }

    /// Interleaved by point, then channel, then `(min, max)`.
    #[must_use]
    pub fn data(&self) -> &[i16] {
        &self.data
    }

    /// Takes [`data`](Self::data) without copying it.
    #[must_use]
    pub fn into_data(self) -> Vec<i16> {
        self.data
    }

    /// Encodes the waveform in the `.dat` format.
    #[must_use]
    pub fn to_dat(&self) -> Vec<u8> {
        let mut writer = DatWriter::new(Cursor::new(Vec::new()));
        self.replay(&mut writer)
            .and_then(|()| writer.finish(self.length))
            .map(Cursor::into_inner)
            .expect("writing to a Vec does not fail")
    }

    fn replay<S: PeakSink>(&self, sink: &mut S) -> Result<(), Error> {
        sink.begin(&self.header)?;
        let channels = self.header.channels as usize;
        let mut peaks = Vec::with_capacity(channels);
        for point in self.data.chunks_exact(channels * 2) {
            peaks.clear();
            peaks.extend(
                point
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|&[min, max]| Peak { min, max }),
            );
            sink.point(&peaks)?;
        }
        Ok(())
    }
}
