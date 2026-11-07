//! tmi's object store (tmi/objects.py, tmi/chunking.py): SHA-256 object
//! ids, zlib object files with Python's level heuristic and bytes, chunked
//! large blobs, and JSON exactly as CPython's `json` writes it.

pub mod chunking;
pub mod json;
pub mod limits;
pub mod oid;
pub mod store;
pub mod zlib;

pub use json::Value;
pub use oid::object_id;
pub use store::{ObjectStore, StoreError};
