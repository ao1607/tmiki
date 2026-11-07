//! Object ids: `sha256(type + "\0" + payload)` as lowercase hex
//! (objects.py `object_id`).
//!
//! SHA-256 comes from ring: on the desktop (i5-10400F, no SHA extensions)
//! ring hashes 0.46 GB/s, the `sha2` crate 0.26 GB/s and aws-lc-rs 0.45 GB/s
//! (CPython's hashlib, OpenSSL: 0.34 GB/s); ring also takes the SHA-NI path
//! where the CPU has one (RUST_MIGRATION.md 3a).

use ring::digest::{Context, SHA256};

const HEX: &[u8; 16] = b"0123456789abcdef";

pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 15) as usize] as char);
    }
    out
}

/// `hashlib.sha256(data).hexdigest()`.
pub fn sha256_hex(data: &[u8]) -> String {
    hex(ring::digest::digest(&SHA256, data).as_ref())
}

/// A streaming SHA-256 (`hashlib.sha256()` fed in pieces).
pub struct Sha256(Context);

impl Default for Sha256 {
    fn default() -> Self {
        Sha256(Context::new(&SHA256))
    }
}

impl Sha256 {
    pub fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    pub fn hexdigest(self) -> String {
        hex(self.0.finish().as_ref())
    }
}

/// `ObjectStore.object_id(object_type, payload)`.
pub fn object_id(object_type: &str, payload: &[u8]) -> String {
    let mut h = Sha256::default();
    h.update(object_type.as_bytes());
    h.update(b"\0");
    h.update(payload);
    h.hexdigest()
}

#[cfg(test)]
mod tests {
    #[test]
    fn known_digest() {
        // hashlib.sha256(b"blob\0hello").hexdigest()
        assert_eq!(
            super::object_id("blob", b"hello"),
            super::sha256_hex(b"blob\0hello"),
        );
        assert_eq!(
            super::sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
