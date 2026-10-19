//! Streaming gzip, for session files (written) and capture files opened with
//! `--open` (read; plain JSON Lines files are read as they are).

use std::io::{self, Read, Write};

use miniz_oxide::deflate::core::CompressorOxide;
use miniz_oxide::inflate::stream::InflateState;
use miniz_oxide::{DataFormat, MZError, MZFlush, MZStatus};

use crate::crc32;

const CHUNK: usize = 64 << 10;
pub const MAGIC: &[u8] = &[0x1f, 0x8b];
/// A header longer than this (huge name or extra field) isn't believed.
const MAX_HEADER: usize = 64 << 10;

/// Compresses what's written into one gzip member; `finish` writes the trailer.
pub struct Writer<W: Write> {
    out: W,
    deflate: Box<CompressorOxide>,
    buf: Vec<u8>,
    crc: u32,
    len: u32,
}

impl<W: Write> Writer<W> {
    pub fn new(mut out: W) -> io::Result<Writer<W>> {
        // No name or time: the same data always gives the same file.
        out.write_all(&[0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff])?;
        let mut deflate = Box::<CompressorOxide>::default();
        deflate.set_format_and_level(DataFormat::Raw, 6);
        Ok(Writer {
            out,
            deflate,
            buf: vec![0; CHUNK],
            crc: 0,
            len: 0,
        })
    }

    fn pump(&mut self, mut input: &[u8], flush: MZFlush) -> io::Result<()> {
        loop {
            let r = miniz_oxide::deflate::stream::deflate(&mut self.deflate, input, &mut self.buf, flush);
            input = &input[r.bytes_consumed..];
            self.out.write_all(&self.buf[..r.bytes_written])?;
            match r.status {
                Ok(MZStatus::StreamEnd) => return Ok(()),
                Ok(_) if input.is_empty() && flush == MZFlush::None => return Ok(()),
                Ok(_) => {}
                Err(MZError::Buf) if input.is_empty() && flush == MZFlush::None => return Ok(()),
                Err(e) => return Err(io::Error::other(format!("deflate: {e:?}"))),
            }
        }
    }

    /// Ends the member (trailer: CRC-32 and size) and returns the output.
    pub fn finish(mut self) -> io::Result<W> {
        self.pump(&[], MZFlush::Finish)?;
        self.out.write_all(&self.crc.to_le_bytes())?;
        self.out.write_all(&self.len.to_le_bytes())?;
        self.out.flush()?;
        Ok(self.out)
    }
}

impl<W: Write> Write for Writer<W> {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.crc = crc32::update(self.crc, b);
        self.len = self.len.wrapping_add(b.len() as u32);
        self.pump(b, MZFlush::None)?;
        Ok(b.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

/// Inflates a gzip stream of one or more members, checking each trailer.
pub struct Reader<R: Read> {
    inner: R,
    inflate: Box<InflateState>,
    /// Compressed bytes read but not used yet: `input[pos..]`.
    input: Vec<u8>,
    pos: usize,
    eof: bool,
    in_member: bool,
    crc: u32,
    len: u32,
}

impl<R: Read> Reader<R> {
    pub fn new(inner: R) -> Reader<R> {
        Reader {
            inner,
            inflate: InflateState::new_boxed(DataFormat::Raw),
            input: Vec::with_capacity(CHUNK),
            pos: 0,
            eof: false,
            in_member: false,
            crc: 0,
            len: 0,
        }
    }

    /// Reads more compressed input; false at the end of the stream.
    fn fill(&mut self) -> io::Result<bool> {
        if self.eof {
            return Ok(false);
        }
        if self.pos > 0 {
            self.input.drain(..self.pos);
            self.pos = 0;
        }
        let start = self.input.len();
        self.input.resize(start + CHUNK, 0);
        let n = loop {
            match self.inner.read(&mut self.input[start..]) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                r => break r,
            }
        };
        let n = n.inspect_err(|_| self.input.truncate(start))?;
        self.input.truncate(start + n);
        self.eof = n == 0;
        Ok(n > 0)
    }

    fn avail(&self) -> &[u8] {
        &self.input[self.pos..]
    }

    /// Reads up to `n` bytes ahead; false if the stream ends first.
    fn need(&mut self, n: usize) -> io::Result<bool> {
        while self.avail().len() < n {
            if !self.fill()? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Starts the next member; false at a clean end of the stream.
    fn start_member(&mut self) -> io::Result<bool> {
        if !self.need(1)? {
            return Ok(false);
        }
        loop {
            match crate::compressed::gzip_header(self.avail()) {
                Ok(n) => {
                    self.pos += n;
                    break;
                }
                // Maybe the header isn't all here yet.
                Err(_) if self.avail().len() < MAX_HEADER && self.fill()? => {}
                Err(e) => return Err(io::Error::new(io::ErrorKind::InvalidData, e)),
            }
        }
        self.inflate.reset(DataFormat::Raw);
        self.in_member = true;
        self.crc = 0;
        self.len = 0;
        Ok(true)
    }

    fn end_member(&mut self) -> io::Result<()> {
        if !self.need(8)? {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "gzip: the stream ends early",
            ));
        }
        let t = &self.avail()[..8];
        let (crc, len) = (
            u32::from_le_bytes([t[0], t[1], t[2], t[3]]),
            u32::from_le_bytes([t[4], t[5], t[6], t[7]]),
        );
        self.pos += 8;
        self.in_member = false;
        if crc != self.crc || len != self.len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "gzip: the check sum doesn't match",
            ));
        }
        Ok(())
    }
}

impl<R: Read> Read for Reader<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        loop {
            if !self.in_member && !self.start_member()? {
                return Ok(0);
            }
            if self.avail().is_empty() && !self.fill()? {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "gzip: the stream ends early",
                ));
            }
            let r =
                miniz_oxide::inflate::stream::inflate(&mut self.inflate, &self.input[self.pos..], out, MZFlush::None);
            self.pos += r.bytes_consumed;
            let got = &out[..r.bytes_written];
            self.crc = crc32::update(self.crc, got);
            self.len = self.len.wrapping_add(got.len() as u32);
            match r.status {
                Ok(MZStatus::StreamEnd) => {
                    self.end_member()?;
                    if r.bytes_written > 0 {
                        return Ok(r.bytes_written);
                    }
                }
                Ok(_) | Err(MZError::Buf) => {
                    if r.bytes_written > 0 {
                        return Ok(r.bytes_written);
                    }
                    // No progress: it needs more input.
                    if r.bytes_consumed == 0 && !self.fill()? {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "gzip: the stream ends early",
                        ));
                    }
                }
                Err(_) => return Err(io::Error::new(io::ErrorKind::InvalidData, "gzip: corrupt data")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gzip(b: &[u8]) -> Vec<u8> {
        let mut w = Writer::new(Vec::new()).unwrap();
        // In odd-sized pieces, as a session is written.
        for piece in b.chunks(7919) {
            w.write_all(piece).unwrap();
        }
        w.finish().unwrap()
    }

    fn gunzip(b: &[u8], read_size: usize) -> io::Result<Vec<u8>> {
        let mut r = Reader::new(b);
        let mut out = Vec::new();
        let mut buf = vec![0; read_size];
        loop {
            let n = r.read(&mut buf)?;
            if n == 0 {
                return Ok(out);
            }
            out.extend_from_slice(&buf[..n]);
        }
    }

    #[test]
    fn round_trip() {
        let text: Vec<u8> = (0..300_000u32)
            .flat_map(|i| format!("{{\"n\":{i}}}\n").into_bytes())
            .collect();
        let random: Vec<u8> = (0..200_000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        for data in [&b""[..], b"x", &text, &random] {
            let z = gzip(data);
            assert!(z.starts_with(MAGIC));
            for read_size in [1, 1000, 1 << 20] {
                if read_size == 1 && data.len() > 10_000 {
                    continue;
                }
                assert_eq!(gunzip(&z, read_size).unwrap(), data);
            }
            // The existing whole-buffer gunzip agrees.
            let mut out = Vec::new();
            crate::compressed::gunzip(&z, &mut out, &mut vec![]).unwrap();
            assert_eq!(out, data);
        }
        assert!(gzip(&text).len() < text.len() / 5);
    }

    #[test]
    fn members_and_damage() {
        let mut two = gzip(b"hello ");
        two.extend(gzip(b"world"));
        assert_eq!(gunzip(&two, 3).unwrap(), b"hello world");
        // A header with a file name, written by another tool.
        let mut named = vec![0x1f, 0x8b, 8, 8, 0, 0, 0, 0, 0, 3];
        named.extend(b"name.jsonl\0");
        named.extend(&gzip(b"named")[10..]);
        assert_eq!(gunzip(&named, 64).unwrap(), b"named");

        let z = gzip(b"some data that compresses some data that compresses");
        let mut flipped = z.clone();
        let last = flipped.len() - 6;
        flipped[last] ^= 1; // the CRC
        assert!(gunzip(&flipped, 64).is_err());
        assert!(gunzip(&z[..z.len() - 3], 64).is_err(), "cut trailer");
        assert!(gunzip(&z[..z.len() / 2], 64).is_err(), "cut data");
        assert!(gunzip(b"not gzip at all", 64).is_err());
    }
}
