//! numpy 2.5's and CPython's float semantics where tmi's renderer depends on
//! them bit for bit (rounding, interp, linspace, float `%`, `math.hypot`,
//! pairwise sums).
//!
//! dtype promotion (NEP 50) is followed at the call sites: a Python float
//! meeting a float32 array is cast to float32 first (`x as f32`), and every
//! operation runs in the array's precision, left to right, without fused
//! multiply-adds.

pub mod interp;
pub mod linalg;
pub mod pyfloat;
pub mod pyhypot;
pub mod pyint;
pub mod round;
pub mod sum;

pub use interp::{interp, linspace};
pub use linalg::{inv2, matmul3};
pub use pyfloat::{py_mod, py_round};
pub use pyhypot::py_hypot;
pub use round::{rint_to_u8, to_byte};
