//! CLIP and MDP2 readers and writers: the CSFCHUNK container, the read-only
//! model of a CLIP's SQLite database, offscreen attributes, tile decoders,
//! and the layer write-back of both formats (stage 2c).

pub mod attr;
pub mod container;
pub mod db;
pub mod mdp2;
pub mod mdp2_write;
pub mod model;
pub mod testing;
pub mod tiles;
pub mod vector;
pub mod window;
pub mod write;
pub mod zlib;

pub use model::ClipDoc;
