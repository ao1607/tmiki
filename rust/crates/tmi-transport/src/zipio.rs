//! The zip archives tmihub's object endpoints carry: written with one
//! member per object, named `<oid[:2]>/<oid[2:]>` (stored: the object files
//! are zlib streams already; Python deflates them again, which the receiver
//! cannot tell apart), read as `zipfile.ZipFile`
//! reads them (member names in central-directory order, stored or deflated
//! data, CRC-32 checked).

use std::io::{self, Read, Seek, SeekFrom, Write};

const LOCAL: u32 = 0x0403_4b50;
const CENTRAL: u32 = 0x0201_4b50;
const EOCD: u32 = 0x0605_4b50;
const EOCD64: u32 = 0x0606_4b50;
const EOCD64_LOCATOR: u32 = 0x0706_4b50;
// 1980-01-01 00:00 in MS-DOS date and time.
const DOS_DATE: u16 = (0 << 9) | (1 << 5) | 1;
const DOS_TIME: u16 = 0;

struct Member {
    name: Vec<u8>,
    method: u16,
    crc: u32,
    csize: u64,
    usize: u64,
    offset: u64,
}

/// A zip being written to `out`.
pub struct Writer<W: Write> {
    out: W,
    offset: u64,
    members: Vec<Member>,
}

fn le16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn le32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn le64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// The most one member may inflate to: an object file at its cap.
const MAX_MEMBER_SIZE: u64 = tmi_store::store::MAX_OBJECT_FILE;

fn too_large() -> io::Error {
    io::Error::other("a zip member of 4 GiB or more")
}

impl<W: Write> Writer<W> {
    pub fn new(out: W) -> Self {
        Writer { out, offset: 0, members: Vec::new() }
    }

    /// Add `data` as member `name`, deflated or stored (the object files
    /// tmihub takes are zlib streams already: deflating them again gains
    /// nothing).
    pub fn add(&mut self, name: &str, data: &[u8], deflate: bool) -> io::Result<()> {
        let mut crc = flate2::Crc::new();
        crc.update(data);
        let packed: std::borrow::Cow<[u8]> = if deflate {
            let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
            enc.write_all(data)?;
            enc.finish()?.into()
        } else {
            data.into()
        };
        if data.len() as u64 >= u32::MAX as u64 || packed.len() as u64 >= u32::MAX as u64 {
            return Err(too_large());
        }
        let member = Member {
            name: name.as_bytes().to_vec(),
            method: if deflate { 8 } else { 0 },
            crc: crc.sum(),
            csize: packed.len() as u64,
            usize: data.len() as u64,
            offset: self.offset,
        };
        let utf8 = !name.is_ascii();
        let mut head = Vec::with_capacity(30 + member.name.len());
        le32(&mut head, LOCAL);
        le16(&mut head, 20);
        le16(&mut head, if utf8 { 0x800 } else { 0 });
        le16(&mut head, member.method);
        le16(&mut head, DOS_TIME);
        le16(&mut head, DOS_DATE);
        le32(&mut head, member.crc);
        le32(&mut head, member.csize as u32);
        le32(&mut head, member.usize as u32);
        le16(&mut head, member.name.len() as u16);
        le16(&mut head, 0);
        head.extend_from_slice(&member.name);
        self.out.write_all(&head)?;
        self.out.write_all(&packed)?;
        self.offset += (head.len() + packed.len()) as u64;
        self.members.push(member);
        Ok(())
    }

    /// Write the central directory; the writer back.
    pub fn finish(mut self) -> io::Result<W> {
        let start = self.offset;
        let mut dir = Vec::new();
        for m in &self.members {
            if m.offset >= u32::MAX as u64 {
                return Err(too_large());
            }
            let utf8 = std::str::from_utf8(&m.name).map_or(false, |n| !n.is_ascii());
            le32(&mut dir, CENTRAL);
            le16(&mut dir, 20);
            le16(&mut dir, 20);
            le16(&mut dir, if utf8 { 0x800 } else { 0 });
            le16(&mut dir, m.method);
            le16(&mut dir, DOS_TIME);
            le16(&mut dir, DOS_DATE);
            le32(&mut dir, m.crc);
            le32(&mut dir, m.csize as u32);
            le32(&mut dir, m.usize as u32);
            le16(&mut dir, m.name.len() as u16);
            le16(&mut dir, 0);
            le16(&mut dir, 0);
            le16(&mut dir, 0);
            le16(&mut dir, 0);
            le32(&mut dir, 0o600 << 16);
            le32(&mut dir, m.offset as u32);
            dir.extend_from_slice(&m.name);
        }
        let count = self.members.len() as u64;
        let size = dir.len() as u64;
        let zip64 = count >= 0xFFFF || start >= u32::MAX as u64 || size >= u32::MAX as u64;
        if zip64 {
            let eocd64_at = start + size;
            le32(&mut dir, EOCD64);
            le64(&mut dir, 44);
            le16(&mut dir, 45);
            le16(&mut dir, 45);
            le32(&mut dir, 0);
            le32(&mut dir, 0);
            le64(&mut dir, count);
            le64(&mut dir, count);
            le64(&mut dir, size);
            le64(&mut dir, start);
            le32(&mut dir, EOCD64_LOCATOR);
            le32(&mut dir, 0);
            le64(&mut dir, eocd64_at);
            le32(&mut dir, 1);
        }
        le32(&mut dir, EOCD);
        le16(&mut dir, 0);
        le16(&mut dir, 0);
        le16(&mut dir, count.min(0xFFFF) as u16);
        le16(&mut dir, count.min(0xFFFF) as u16);
        le32(&mut dir, size.min(u32::MAX as u64) as u32);
        le32(&mut dir, start.min(u32::MAX as u64) as u32);
        le16(&mut dir, 0);
        self.out.write_all(&dir)?;
        self.out.flush()?;
        Ok(self.out)
    }
}

/// What reading failed with, as `zipfile` words it.
#[derive(Debug)]
pub enum ReadError {
    /// zipfile.BadZipFile(msg)
    BadZip(String),
    /// Another exception, already formatted.
    Other(String),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadError::BadZip(m) | ReadError::Other(m) => f.write_str(m),
        }
    }
}

fn io_other(e: io::Error) -> ReadError {
    ReadError::Other(e.to_string())
}

fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}

fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(b[i..i + 4].try_into().unwrap())
}

fn u64_at(b: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(b[i..i + 8].try_into().unwrap())
}

/// One member of a zip being read.
pub struct Entry {
    /// The name as `zipfile` decodes it (UTF-8 when flagged, else cp437;
    /// bytes above 0x7f become U+FFFD here: no tmi name has them).
    pub name: String,
    flags: u16,
    method: u16,
    crc: u32,
    csize: u64,
    usize: u64,
    offset: u64,
}

impl Entry {
    /// What reading the member may hold: its packed or declared size.
    pub fn held_size(&self) -> u64 {
        self.csize.max(self.usize)
    }
}

/// A zip being read from a seekable file.
pub struct Reader<R: Read + Seek> {
    src: R,
    pub entries: Vec<Entry>,
}

fn decode_name(raw: &[u8], flags: u16) -> String {
    if flags & 0x800 != 0 {
        return String::from_utf8_lossy(raw).into_owned();
    }
    raw.iter().map(|&b| if b < 0x80 { b as char } else { '\u{fffd}' }).collect()
}

impl<R: Read + Seek> Reader<R> {
    pub fn open(mut src: R) -> Result<Self, ReadError> {
        let not_zip = || ReadError::BadZip("File is not a zip file".into());
        let len = src.seek(SeekFrom::End(0)).map_err(io_other)?;
        if len < 22 {
            return Err(not_zip());
        }
        let tail_len = len.min(22 + 65535);
        src.seek(SeekFrom::Start(len - tail_len)).map_err(io_other)?;
        let mut tail = vec![0u8; tail_len as usize];
        src.read_exact(&mut tail).map_err(io_other)?;
        let at = (0..=tail.len() - 22).rev().find(|&i| u32_at(&tail, i) == EOCD).ok_or_else(not_zip)?;
        let eocd_pos = len - tail_len + at as u64;
        let mut count = u16_at(&tail, at + 10) as u64;
        let mut size = u32_at(&tail, at + 12) as u64;
        let mut start = u32_at(&tail, at + 16) as u64;
        // zip64: the locator sits right before the end record
        if eocd_pos >= 20 {
            let mut loc = [0u8; 20];
            src.seek(SeekFrom::Start(eocd_pos - 20)).map_err(io_other)?;
            src.read_exact(&mut loc).map_err(io_other)?;
            if u32_at(&loc, 0) == EOCD64_LOCATOR {
                let rec_at = u64_at(&loc, 8);
                let mut rec = [0u8; 56];
                src.seek(SeekFrom::Start(rec_at)).map_err(io_other)?;
                src.read_exact(&mut rec).map_err(io_other)?;
                if u32_at(&rec, 0) != EOCD64 {
                    return Err(ReadError::BadZip("Corrupt zip64 end of central directory locator".into()));
                }
                count = u64_at(&rec, 32);
                size = u64_at(&rec, 40);
                start = u64_at(&rec, 48);
            }
        }
        // sizes and offsets are the sender's word: held to the file before
        // anything is allocated for them
        if start.checked_add(size).is_none_or(|end| end > len) {
            return Err(ReadError::BadZip("Truncated central directory".into()));
        }
        src.seek(SeekFrom::Start(start)).map_err(|_| not_zip())?;
        let mut dir = vec![0u8; size as usize];
        src.read_exact(&mut dir).map_err(|_| ReadError::BadZip("Truncated central directory".into()))?;
        let mut entries = Vec::new();
        let mut i = 0usize;
        while i + 46 <= dir.len() {
            if u32_at(&dir, i) != CENTRAL {
                return Err(ReadError::BadZip("Bad magic number for central directory".into()));
            }
            let flags = u16_at(&dir, i + 8);
            let method = u16_at(&dir, i + 10);
            let crc = u32_at(&dir, i + 16);
            let mut csize = u32_at(&dir, i + 20) as u64;
            let mut usize = u32_at(&dir, i + 24) as u64;
            let nlen = u16_at(&dir, i + 28) as usize;
            let xlen = u16_at(&dir, i + 30) as usize;
            let clen = u16_at(&dir, i + 32) as usize;
            let mut offset = u32_at(&dir, i + 42) as u64;
            let end = i + 46 + nlen + xlen + clen;
            if end > dir.len() {
                return Err(ReadError::BadZip("Truncated central directory".into()));
            }
            let name = decode_name(&dir[i + 46..i + 46 + nlen], flags);
            // the zip64 extra field: the 0xFFFFFFFF values, in order
            let mut x = i + 46 + nlen;
            let xend = x + xlen;
            while x + 4 <= xend {
                let id = u16_at(&dir, x);
                let n = u16_at(&dir, x + 2) as usize;
                let mut p = x + 4;
                if id == 1 {
                    for field in [&mut usize, &mut csize, &mut offset] {
                        if *field == u32::MAX as u64 && p + 8 <= x + 4 + n {
                            *field = u64_at(&dir, p);
                            p += 8;
                        }
                    }
                }
                x += 4 + n;
            }
            if offset.checked_add(30).and_then(|o| o.checked_add(csize)).is_none_or(|end| end > len)
                || usize > MAX_MEMBER_SIZE
            {
                return Err(ReadError::BadZip(format!("Bad size or offset for file {}", py_repr(&name))));
            }
            entries.push(Entry { name, flags, method, crc, csize, usize, offset });
            i = end;
            if entries.len() as u64 == count && count < 0xFFFF {
                break;
            }
        }
        Ok(Reader { src, entries })
    }

    /// `zf.open(name).read()` of entry `index`.
    pub fn read(&mut self, index: usize) -> Result<Vec<u8>, ReadError> {
        let e = &self.entries[index];
        let mut head = [0u8; 30];
        self.src.seek(SeekFrom::Start(e.offset)).map_err(io_other)?;
        self.src.read_exact(&mut head).map_err(|_| ReadError::BadZip("Truncated file header".into()))?;
        if u32_at(&head, 0) != LOCAL {
            return Err(ReadError::BadZip("Bad magic number for file header".into()));
        }
        let nlen = u16_at(&head, 26) as u64;
        let xlen = u16_at(&head, 28) as u64;
        if e.flags & 0x1 != 0 {
            return Err(ReadError::Other(format!("File '{}' is encrypted, password required for extraction", e.name)));
        }
        self.src.seek(SeekFrom::Start(e.offset + 30 + nlen + xlen)).map_err(io_other)?;
        let mut packed = Vec::new();
        (&mut self.src).take(e.csize).read_to_end(&mut packed).map_err(io_other)?;
        if (packed.len() as u64) < e.csize {
            return Err(ReadError::Other("Error -3 while decompressing data".into()));
        }
        let data = match e.method {
            0 => packed,
            8 => {
                // inflated no further than the size it declares (a deflate
                // bomb stops there); more than declared is a bad member
                let mut out = Vec::with_capacity(e.usize.min(1 << 20) as usize);
                flate2::read::DeflateDecoder::new(&packed[..])
                    .take(e.usize + 1)
                    .read_to_end(&mut out)
                    .map_err(|err| ReadError::Other(format!("Error -3 while decompressing data: {err}")))?;
                if out.len() as u64 > e.usize {
                    return Err(ReadError::BadZip(format!("Bad size for file {}", py_repr(&e.name))));
                }
                out
            }
            other => {
                return Err(ReadError::Other(format!("compression type {other}")));
            }
        };
        let mut crc = flate2::Crc::new();
        crc.update(&data);
        if crc.sum() != e.crc {
            return Err(ReadError::BadZip(format!("Bad CRC-32 for file {}", py_repr(&e.name))));
        }
        Ok(data)
    }
}

/// `repr(name)` for the names a zip of objects has (Forward-free: an
/// approximation for anything but printable ASCII).
fn py_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::new();
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let mut w = Writer::new(std::io::Cursor::new(Vec::new()));
        w.add("ab/cdef", b"hello hello hello", true).unwrap();
        w.add("01/23", b"", false).unwrap();
        let buf = w.finish().unwrap().into_inner();
        let mut r = Reader::open(std::io::Cursor::new(buf)).unwrap();
        let names: Vec<_> = r.entries.iter().map(|e| e.name.clone()).collect();
        assert_eq!(names, ["ab/cdef", "01/23"]);
        assert_eq!(r.read(0).unwrap(), b"hello hello hello");
        assert_eq!(r.read(1).unwrap(), b"");
        assert!(matches!(Reader::open(std::io::Cursor::new(b"nope".to_vec())), Err(ReadError::BadZip(_))));
    }

    /// A one-member zip of `data` (deflated) and where its central entry is.
    fn one_member(data: &[u8]) -> (Vec<u8>, usize) {
        let mut w = Writer::new(std::io::Cursor::new(Vec::new()));
        w.add("ab/cd", data, true).unwrap();
        let buf = w.finish().unwrap().into_inner();
        let at = (0..buf.len() - 4).find(|&i| u32_at(&buf, i) == CENTRAL).unwrap();
        (buf, at)
    }

    #[test]
    fn a_deflate_bomb_member_stops_at_its_declared_size() {
        let (mut buf, at) = one_member(&vec![0u8; 8 << 20]);
        buf[at + 24..at + 28].copy_from_slice(&1000u32.to_le_bytes()); // declares 1000 bytes
        let mut r = Reader::open(std::io::Cursor::new(buf)).unwrap();
        let err = r.read(0).err().unwrap().to_string();
        assert!(err.starts_with("Bad size for file"), "{err}");
    }

    #[test]
    fn sizes_and_offsets_past_the_file_are_refused_before_allocating() {
        let (buf, at) = one_member(b"hello");
        let mut big = buf.clone();
        big[at + 20..at + 24].copy_from_slice(&0xFFFF_FFF0u32.to_le_bytes()); // csize
        assert!(Reader::open(std::io::Cursor::new(big)).err().unwrap().to_string().starts_with("Bad size or offset"));
        let mut far = buf.clone();
        far[at + 42..at + 46].copy_from_slice(&0xFFFF_FFF0u32.to_le_bytes()); // offset
        assert!(Reader::open(std::io::Cursor::new(far)).is_err());
        // the end record's directory size past the file
        let mut dir = buf;
        let eocd = dir.len() - 22;
        dir[eocd + 12..eocd + 16].copy_from_slice(&0xFFFF_FFF0u32.to_le_bytes());
        assert!(matches!(Reader::open(std::io::Cursor::new(dir)), Err(ReadError::BadZip(_))));
    }
}
