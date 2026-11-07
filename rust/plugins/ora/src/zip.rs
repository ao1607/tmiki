//! The zip directory, read through `Host::read` (the members' bytes are
//! only pointed at, never copied into the plugin), and what the writer needs
//! to rebuild it ([`Directory`]: every central directory record and where
//! each member's bytes run).

use std::collections::HashMap;

use crate::model::{Error, Host, Member, Span};

pub struct Archive {
    /// Each name's last member (as zipfile's `getinfo`).
    entries: HashMap<String, Member>,
    /// The local header offset of each name's last member.
    locals: HashMap<String, u64>,
}

fn u16le(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u32le(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn corrupt(m: impl Into<String>) -> Error {
    Error::Corrupt(m.into())
}

/// One central directory record.
#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    /// Where the record sits in the file, and its length.
    pub record: Span,
    pub method: u16,
    /// The stored (compressed) size.
    pub csize: u64,
    pub local: u64,
}

/// The end of central directory record and the directory before it.
#[derive(Clone, Debug)]
pub struct Directory {
    pub entries: Vec<Entry>,
    pub cd_offset: u64,
    pub cd_size: u64,
    /// The end of central directory record (with its comment), to the end of the file.
    pub eocd: Span,
}

impl Directory {
    pub fn open(host: &dyn Host) -> Result<Directory, Error> {
        let size = host.size();
        if size < 22 {
            return Err(corrupt("it is empty or cut short, too short for a ZIP archive"));
        }
        // the end of central directory record: within the last 64 KiB + 22 bytes
        let tail_len = size.min(22 + 0xFFFF);
        let tail_at = size - tail_len;
        let tail = host.read(tail_at, tail_len as u32);
        let eocd = (0..=tail.len() - 22)
            .rev()
            .find(|&i| tail[i..i + 4] == *b"PK\x05\x06")
            .ok_or_else(|| corrupt("the archive is cut short, or it is not a ZIP file: it has no end of central directory record"))?;
        let e = &tail[eocd..];
        let count = u16le(e, 10) as usize;
        let cd_size = u32le(e, 12) as u64;
        let cd_off = u32le(e, 16) as u64;
        if count == 0xFFFF || cd_size == 0xFFFF_FFFF || cd_off == 0xFFFF_FFFF {
            return Err(Error::Unsupported("zip64 archives are not read".into()));
        }
        if cd_off.checked_add(cd_size).is_none_or(|end| end > size) || cd_size > (64 << 20) {
            return Err(corrupt("the central directory is outside the file"));
        }
        let cd = host.read(cd_off, cd_size as u32);
        let mut entries = Vec::with_capacity(count);
        let mut at = 0usize;
        for _ in 0..count {
            if at + 46 > cd.len() || cd[at..at + 4] != *b"PK\x01\x02" {
                return Err(corrupt("a broken central directory entry"));
            }
            let method = u16le(&cd, at + 10);
            let csize = u32le(&cd, at + 20) as u64;
            let n = u16le(&cd, at + 28) as usize;
            let m = u16le(&cd, at + 30) as usize;
            let k = u16le(&cd, at + 32) as usize;
            let local = u32le(&cd, at + 42) as u64;
            let name_bytes = cd.get(at + 46..at + 46 + n).ok_or_else(|| corrupt("a name runs past the directory"))?;
            let name = String::from_utf8_lossy(name_bytes).into_owned();
            // (a record running past the directory is the writer's to refuse:
            // the reader never needed its tail)
            let len = 46 + n + m + k;
            if csize == 0xFFFF_FFFF || local == 0xFFFF_FFFF {
                return Err(Error::Unsupported("zip64 members are not read".into()));
            }
            entries.push(Entry { name, record: Span { offset: cd_off + at as u64, len: len as u64 }, method, csize, local });
            at += len;
        }
        Ok(Directory { entries, cd_offset: cd_off, cd_size, eocd: Span { offset: tail_at + eocd as u64, len: (tail.len() - eocd) as u64 } })
    }
}

impl Archive {
    pub fn open(host: &dyn Host) -> Result<Archive, Error> {
        let dir = Directory::open(host)?;
        let size = host.size();
        let mut entries = HashMap::with_capacity(dir.entries.len());
        let mut locals = HashMap::with_capacity(dir.entries.len());
        for e in &dir.entries {
            let csize = e.csize;
            let deflate = match e.method {
                0 => false,
                8 => true,
                // kept out of the table: naming it later is an error
                _ => continue,
            };
            let head = host.read(e.local, 30);
            if head.len() < 30 || head[..4] != *b"PK\x03\x04" {
                return Err(corrupt(format!("{}: no local header", e.name)));
            }
            let start = e.local + 30 + u16le(&head, 26) as u64 + u16le(&head, 28) as u64;
            if start.checked_add(csize).is_none_or(|end| end > size) {
                return Err(corrupt(format!("{}: its data runs past the file", e.name)));
            }
            // a later member of the same name replaces an earlier one
            entries.insert(e.name.clone(), Member { span: Span { offset: start, len: csize }, deflate });
            locals.insert(e.name.clone(), e.local);
        }
        Ok(Archive { entries, locals })
    }

    /// The last member named `name` (as zipfile's `getinfo`).
    pub fn member(&self, name: &str) -> Option<Member> {
        self.entries.get(name).copied()
    }

    /// The local header offset of the last member named `name`.
    pub fn local(&self, name: &str) -> Option<u64> {
        self.locals.get(name).copied()
    }
}
