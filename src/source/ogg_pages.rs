//! Ogg pages and packets of one logical stream, read as libogg reads them:
//! bytes that do not form a page with a valid checksum are skipped, but a
//! false page header that claims more bytes than are left ends the input.

use std::io::{ErrorKind, Read};

use crate::Error;

const CONTINUED: u8 = 0x01;
const END_OF_STREAM: u8 = 0x04;

const HEADER_BYTES: usize = 27;
const MAX_PAGE_BYTES: usize = HEADER_BYTES + 255 + 255 * 255;

/// A packet, and whether pages of the stream are missing before it.
#[derive(Debug, Default)]
pub(crate) struct Packet {
    pub(crate) data: Vec<u8>,
    pub(crate) after_gap: bool,
}

/// Packets that end on one page, with that page's granule position. Pages
/// with a granule position of -1 belong to the group of the next page.
#[derive(Debug, Default)]
pub(crate) struct Group {
    pub(crate) packets: Vec<Packet>,
    /// -1 when no page of the group has one.
    pub(crate) granule: i64,
    pub(crate) end_of_stream: bool,
}

/// Splits the pages of the stream of the first page into [`Group`]s.
pub(crate) struct GroupReader<R> {
    pages: PageReader<R>,
    serial: Option<u32>,
    next_sequence: Option<u32>,
    partial: Vec<u8>,
    /// Whether `partial` holds the start of a packet, possibly empty.
    in_packet: bool,
    gap: bool,
    /// Packets of pages with a granule position of -1.
    pending: Vec<Packet>,
    pending_bytes: usize,
    finished: bool,
    max_packet_bytes: usize,
}

impl<R: Read> GroupReader<R> {
    pub(crate) fn new(reader: R, max_packet_bytes: usize) -> Self {
        GroupReader {
            pages: PageReader::new(reader),
            serial: None,
            next_sequence: None,
            partial: Vec::new(),
            in_packet: false,
            gap: false,
            pending: Vec::new(),
            pending_bytes: 0,
            finished: false,
            max_packet_bytes,
        }
    }

    /// The next page of the stream on which a packet ends. `None` at the end
    /// of the input, after the end-of-stream page, or at a page cut short.
    pub(crate) fn next_group(&mut self) -> Result<Option<Group>, Error> {
        let mut end_of_stream = false;
        while !self.finished {
            let Some(page) = self.pages.next_page()? else {
                self.finished = true;
                break;
            };
            let serial = page.serial();
            if serial != *self.serial.get_or_insert(serial) {
                continue;
            }
            // libogg refuses the page, and libsndfile stops there.
            if page.version() != 0 {
                self.finished = true;
                break;
            }
            if self
                .next_sequence
                .is_some_and(|expected| expected != page.sequence())
            {
                self.partial.clear();
                self.in_packet = false;
                self.gap = true;
            }
            self.next_sequence = Some(page.sequence().wrapping_add(1));
            end_of_stream = page.flags() & END_OF_STREAM != 0;
            self.finished = end_of_stream;
            let granule = page.granule();

            let lacing = page.lacing();
            let mut body = page.body();
            let mut segments = lacing.iter().map(|&len| len as usize);
            if page.flags() & CONTINUED == 0 {
                self.partial.clear();
                self.in_packet = false;
            } else if !self.in_packet {
                // The tail of a packet whose start is lost.
                for len in segments.by_ref() {
                    body = &body[len..];
                    if len < 255 {
                        break;
                    }
                }
            }
            for len in segments {
                if self.pending_bytes + self.partial.len() + len > self.max_packet_bytes {
                    return Err(Error::LimitExceeded(
                        "Ogg packet larger than max_packet_bytes",
                    ));
                }
                self.partial.extend_from_slice(&body[..len]);
                body = &body[len..];
                self.in_packet = true;
                if len < 255 {
                    // Counts empty packets too, which use memory.
                    self.pending_bytes += self.partial.len() + size_of::<Packet>();
                    self.pending.push(Packet {
                        data: std::mem::take(&mut self.partial),
                        after_gap: std::mem::take(&mut self.gap),
                    });
                    self.in_packet = false;
                }
            }
            if !self.pending.is_empty() && granule != -1 {
                return Ok(Some(self.take_pending(granule, end_of_stream)));
            }
        }
        Ok((!self.pending.is_empty()).then(|| self.take_pending(-1, end_of_stream)))
    }

    fn take_pending(&mut self, granule: i64, end_of_stream: bool) -> Group {
        self.pending_bytes = 0;
        Group {
            packets: std::mem::take(&mut self.pending),
            granule,
            end_of_stream,
        }
    }
}

/// A borrowed page whose checksum has been verified.
struct Page<'a>(&'a [u8]);

impl Page<'_> {
    fn version(&self) -> u8 {
        self.0[4]
    }

    fn flags(&self) -> u8 {
        self.0[5]
    }

    fn granule(&self) -> i64 {
        i64::from_le_bytes(self.0[6..14].try_into().unwrap())
    }

    fn serial(&self) -> u32 {
        u32::from_le_bytes(self.0[14..18].try_into().unwrap())
    }

    fn sequence(&self) -> u32 {
        u32::from_le_bytes(self.0[18..22].try_into().unwrap())
    }

    fn lacing(&self) -> &[u8] {
        &self.0[HEADER_BYTES..HEADER_BYTES + self.0[26] as usize]
    }

    fn body(&self) -> &[u8] {
        &self.0[HEADER_BYTES + self.0[26] as usize..]
    }
}

/// Reads pages from a plain `Read`, keeping at most one page of input.
struct PageReader<R> {
    reader: R,
    buf: Box<[u8]>,
    start: usize,
    end: usize,
    eof: bool,
}

impl<R: Read> PageReader<R> {
    fn new(reader: R) -> Self {
        PageReader {
            reader,
            buf: vec![0; 2 * MAX_PAGE_BYTES].into_boxed_slice(),
            start: 0,
            end: 0,
            eof: false,
        }
    }

    fn next_page(&mut self) -> Result<Option<Page<'_>>, Error> {
        loop {
            if !self.fill(HEADER_BYTES)? {
                return Ok(None);
            }
            let header = &self.buf[self.start..self.end];
            // As libogg, which reads the length of a page of any version.
            if &header[..4] != b"OggS" {
                self.skip_to_capture();
                continue;
            }
            let segment_count = header[26] as usize;
            if !self.fill(HEADER_BYTES + segment_count)? {
                return Ok(None);
            }
            let lacing = &self.buf[self.start + HEADER_BYTES..][..segment_count];
            let len = HEADER_BYTES
                + segment_count
                + lacing.iter().map(|&len| len as usize).sum::<usize>();
            if !self.fill(len)? {
                return Ok(None);
            }
            let page = &self.buf[self.start..self.start + len];
            if checksum(page) != u32::from_le_bytes(page[22..26].try_into().unwrap()) {
                self.start += 1;
                self.skip_to_capture();
                continue;
            }
            self.start += len;
            return Ok(Some(Page(&self.buf[self.start - len..self.start])));
        }
    }

    /// Moves `start` to the next `OggS` after it, or near the end of the data.
    fn skip_to_capture(&mut self) {
        let data = &self.buf[self.start..self.end];
        match data.windows(4).skip(1).position(|window| window == b"OggS") {
            Some(offset) => self.start += offset + 1,
            // The last three bytes may start a capture pattern.
            None => self.start = self.end - data.len().min(3),
        }
    }

    /// Makes `len` bytes available from `start`. Returns false if the input
    /// ends first.
    fn fill(&mut self, len: usize) -> Result<bool, Error> {
        if self.end - self.start >= len {
            return Ok(true);
        }
        if self.start + len > self.buf.len() {
            self.buf.copy_within(self.start..self.end, 0);
            self.end -= self.start;
            self.start = 0;
        }
        while self.end - self.start < len && !self.eof {
            match self.reader.read(&mut self.buf[self.end..]) {
                Ok(0) => self.eof = true,
                Ok(read) => self.end += read,
                Err(err) if err.kind() == ErrorKind::Interrupted => {}
                Err(err) => return Err(err.into()),
            }
        }
        Ok(self.end - self.start >= len)
    }
}

/// `CRC_TABLES[k][byte]` is the checksum of `byte` followed by `k` zero bytes,
/// so that `checksum` handles 8 bytes per step.
const CRC_TABLES: [[u32; 256]; 8] = {
    let mut tables = [[0; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut crc = (i as u32) << 24;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04C1_1DB7
            } else {
                crc << 1
            };
            bit += 1;
        }
        tables[0][i] = crc;
        i += 1;
    }
    let mut k = 1;
    while k < 8 {
        let mut i = 0;
        while i < 256 {
            let previous = tables[k - 1][i];
            tables[k][i] = (previous << 8) ^ tables[0][(previous >> 24) as usize];
            i += 1;
        }
        k += 1;
    }
    tables
};

/// The Ogg page checksum, computed with the checksum field as zeros.
fn checksum(page: &[u8]) -> u32 {
    let crc = update(0, &page[..22]);
    let crc = update(crc, &[0; 4]);
    update(crc, &page[26..])
}

fn update(mut crc: u32, bytes: &[u8]) -> u32 {
    let [t0, t1, t2, t3, t4, t5, t6, t7] = &CRC_TABLES;
    let (chunks, rest) = bytes.as_chunks::<8>();
    for chunk in chunks {
        let high = crc ^ u32::from_be_bytes(chunk[..4].try_into().unwrap());
        let [a, b, c, d] = high.to_be_bytes();
        crc = t7[a as usize]
            ^ t6[b as usize]
            ^ t5[c as usize]
            ^ t4[d as usize]
            ^ t3[chunk[4] as usize]
            ^ t2[chunk[5] as usize]
            ^ t1[chunk[6] as usize]
            ^ t0[chunk[7] as usize];
    }
    for &byte in rest {
        crc = (crc << 8) ^ t0[((crc >> 24) as u8 ^ byte) as usize];
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_matches_the_bytewise_definition() {
        let page: Vec<u8> = (0..1000u32).map(|i| ((i * 7919) >> 3) as u8).collect();
        for len in [26, 27, 33, 34, 35, 41, 1000] {
            let page = &page[..len];
            let bytewise = page.iter().enumerate().fold(0u32, |crc, (i, &byte)| {
                let byte = if (22..26).contains(&i) { 0 } else { byte };
                (crc << 8) ^ CRC_TABLES[0][((crc >> 24) as u8 ^ byte) as usize]
            });
            assert_eq!(checksum(page), bytewise, "{len} bytes");
        }
    }
}
