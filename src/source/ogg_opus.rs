//! Ogg Opus (RFC 7845) decoding through libopus.

use std::fmt;
use std::io::{self, Read};

use ogg::Packet;
use ogg::reading::{BasePacketReader, OggReadError, PageParser};

use super::opus::{End, OpusDecoder, OpusHead, OpusOptions, to_decode_rate};
use super::{invalid, read_full};
use crate::{Error, PcmSource};

/// Ogg page header flag marking the last page of a logical stream.
const END_OF_STREAM: u8 = 0x04;

/// Streams PCM out of an Ogg Opus file.
///
/// Reads with plain `Read`, so an upload body can be decoded as it arrives.
/// Memory is one Ogg page, one packet, and one decoded packet, whatever the
/// input length. Applies the header's pre-skip, output gain, and end trimming.
///
/// Handles channel mapping family 0 (mono and stereo). Only the logical stream
/// of the first page is decoded: pages of other streams are discarded without
/// parsing, so they cannot use memory or fail the decode. Decoding stops at the
/// end of that stream, so chained files yield their first link.
///
/// A stream cut on a page boundary is not an error: the pages present are
/// decoded without end trimming, as audiowaveform does. A cut inside a page is
/// [`Error::InvalidInput`], where audiowaveform decodes what it can.
pub struct OggOpusSource<R: Read> {
    pages: PageReader<R>,
    decoder: OpusDecoder,
    finished: bool,
}

impl<R: Read> OggOpusSource<R> {
    /// Reads the `OpusHead` and `OpusTags` headers with default options.
    ///
    /// # Errors
    ///
    /// As for [`with_options`](Self::with_options).
    pub fn new(reader: R) -> Result<Self, Error> {
        Self::with_options(reader, OpusOptions::default())
    }

    /// Reads the `OpusHead` and `OpusTags` headers.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidInput`] when the input is not an Ogg Opus stream.
    /// - [`Error::Unsupported`] for a channel mapping family other than 0 or a
    ///   major version other than 0.
    pub fn with_options(reader: R, options: OpusOptions) -> Result<Self, Error> {
        let mut pages = PageReader {
            reader,
            packets: BasePacketReader::new(),
            serial: None,
            end_granule: None,
            max_packet_bytes: options.max_packet_bytes,
            pending_bytes: 0,
        };

        let head = pages.next_packet()?.ok_or_else(|| invalid("empty input"))?;
        if !head.first_in_stream() {
            return Err(invalid("first packet is not the start of a stream"));
        }
        let head = OpusHead::parse(&head.data)?;

        let tags = pages
            .next_packet()?
            .ok_or_else(|| invalid("missing OpusTags header"))?;
        if !tags.data.starts_with(b"OpusTags") {
            return Err(invalid("second packet is not OpusTags"));
        }

        Ok(OggOpusSource {
            pages,
            decoder: OpusDecoder::new(&head, head.pre_skip as u64, &options)?,
            finished: false,
        })
    }
}

impl<R: Read> fmt::Debug for OggOpusSource<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OggOpusSource")
            .field("decoder", &self.decoder)
            .field("finished", &self.finished)
            .finish_non_exhaustive()
    }
}

impl<R: Read> PcmSource for OggOpusSource<R> {
    fn sample_rate(&self) -> u32 {
        self.decoder.sample_rate()
    }

    fn channels(&self) -> u16 {
        self.decoder.channels()
    }

    fn next_block(&mut self) -> Result<Option<&[i16]>, Error> {
        while !self.finished {
            let Some(packet) = self.pages.next_packet()? else {
                // Truncated stream without an end-of-stream page: no end trimming.
                self.finished = true;
                break;
            };
            self.finished = packet.last_in_stream();
            // The end-of-stream granule position can end the audio inside any
            // packet of that page, not only the last.
            let end = match self.pages.end_granule {
                Some(granule) => End::At(to_decode_rate(granule, self.decoder.sample_rate())),
                None => End::Open,
            };
            if self.decoder.decode(&packet.data, end)? {
                return Ok(Some(self.decoder.output()));
            }
        }
        Ok(None)
    }
}

/// Reads Ogg pages from a plain `Read` and splits them into packets of one
/// logical stream.
struct PageReader<R: Read> {
    reader: R,
    packets: BasePacketReader,
    serial: Option<u32>,
    /// Granule position of the end-of-stream page, once it is read. Packets
    /// are drained before each page is read, so every packet returned after
    /// it ends on that page.
    end_granule: Option<u64>,
    max_packet_bytes: usize,
    pending_bytes: usize,
}

impl<R: Read> PageReader<R> {
    fn next_packet(&mut self) -> Result<Option<Packet>, Error> {
        loop {
            if let Some(packet) = self.packets.read_packet() {
                self.pending_bytes = 0;
                return Ok(Some(packet));
            }
            if !self.read_page()? {
                return Ok(None);
            }
        }
    }

    /// Reads one page into the packet reader. Returns false at the end of input.
    ///
    /// Unlike `ogg::PacketReader`, this needs no `Seek`: the page must start
    /// exactly where the previous one ended, which holds for any well-formed
    /// file. Garbage between pages is rejected instead of skipped.
    fn read_page(&mut self) -> Result<bool, Error> {
        let mut header = [0u8; 27];
        match read_full(&mut self.reader, &mut header)? {
            0 => return Ok(false),
            27 => {}
            _ => return Err(invalid("truncated Ogg page header")),
        }
        if &header[..4] != b"OggS" {
            return Err(invalid("missing Ogg capture pattern"));
        }
        let serial = u32::from_le_bytes([header[14], header[15], header[16], header[17]]);
        if serial != *self.serial.get_or_insert(serial) {
            // BasePacketReader keeps partial packets of every stream it sees,
            // outside the max_packet_bytes count, so other streams never reach it.
            return self.skip_page(header[26] as usize).map(|()| true);
        }
        let (mut parser, segment_count) = PageParser::new(header).map_err(ogg_error)?;
        let mut segments = vec![0; segment_count];
        self.read_exact(&mut segments)?;
        let mut body = vec![0; parser.parse_segments(segments)];
        self.read_exact(&mut body)?;

        self.pending_bytes += body.len();
        if self.pending_bytes > self.max_packet_bytes {
            return Err(Error::LimitExceeded(
                "Ogg packet larger than max_packet_bytes",
            ));
        }
        let page = parser.parse_packet_data(body).map_err(ogg_error)?;
        self.packets.push_page(page).map_err(ogg_error)?;
        if header[5] & END_OF_STREAM != 0 {
            self.end_granule = Some(u64::from_le_bytes(header[6..14].try_into().unwrap()));
        }
        Ok(true)
    }

    fn skip_page(&mut self, segment_count: usize) -> Result<(), Error> {
        let mut segments = [0u8; 255];
        self.read_exact(&mut segments[..segment_count])?;
        let body_len: u64 = segments[..segment_count]
            .iter()
            .map(|&len| len as u64)
            .sum();
        let skipped = io::copy(&mut (&mut self.reader).take(body_len), &mut io::sink())?;
        if skipped < body_len {
            return Err(invalid("truncated Ogg page"));
        }
        Ok(())
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), Error> {
        if read_full(&mut self.reader, buf)? < buf.len() {
            return Err(invalid("truncated Ogg page"));
        }
        Ok(())
    }
}

fn ogg_error(err: OggReadError) -> Error {
    match err {
        OggReadError::ReadError(err) => Error::Io(err),
        other => Error::InvalidInput(format!("Ogg: {other}")),
    }
}
