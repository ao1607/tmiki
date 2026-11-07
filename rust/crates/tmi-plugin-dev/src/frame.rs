//! Frames: a 4-byte big-endian length, then one CBOR item of that length.

use std::fmt;
use std::io::{self, Read, Write};

use crate::cbor::{Value, ValueError};

/// The largest frame either side sends or takes (1 GiB: a frame may carry
/// a layer's pixels or a large `inline` byte string).
pub const MAX_FRAME: u32 = 1 << 30;

#[derive(Debug)]
pub enum FrameError {
    /// Reading or writing the pipe failed.
    Io(io::Error),
    /// The stream ended inside a frame (after `got` of its bytes).
    Truncated { got: usize },
    /// A length of 0 or over [`MAX_FRAME`] (often text printed on stdout).
    Length(u32),
    /// The frame's bytes are not one CBOR item.
    Cbor(ValueError),
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::Io(e) => write!(f, "{e}"),
            FrameError::Truncated { got } => write!(f, "the stream ended inside a frame (after {got} bytes)"),
            FrameError::Length(n) => {
                let text: String = n.to_be_bytes().iter().map(|&b| if b.is_ascii_graphic() || b == b' ' { b as char } else { '.' }).collect();
                write!(f, "a frame of {n} bytes (at most {MAX_FRAME}; the 4 bytes read as text: {text:?}, did the plugin print to stdout?)")
            }
            FrameError::Cbor(e) => write!(f, "a frame that is not one CBOR item: {e}"),
        }
    }
}

impl std::error::Error for FrameError {}

/// Writes `v` as one frame and flushes.
pub fn write_frame(w: &mut impl Write, v: &Value) -> Result<(), FrameError> {
    let body = v.encode();
    if body.is_empty() || body.len() > MAX_FRAME as usize {
        return Err(FrameError::Length(body.len().min(u32::MAX as usize) as u32));
    }
    w.write_all(&(body.len() as u32).to_be_bytes()).map_err(FrameError::Io)?;
    w.write_all(&body).map_err(FrameError::Io)?;
    w.flush().map_err(FrameError::Io)
}

fn read_full(r: &mut impl Read, buf: &mut [u8]) -> Result<usize, io::Error> {
    let mut got = 0;
    while got < buf.len() {
        match r.read(&mut buf[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(got)
}

/// The next frame; `None` when the stream ends between frames.
pub fn read_frame(r: &mut impl Read) -> Result<Option<Value>, FrameError> {
    let mut len = [0u8; 4];
    match read_full(r, &mut len).map_err(FrameError::Io)? {
        0 => return Ok(None),
        4 => {}
        got => return Err(FrameError::Truncated { got }),
    }
    let n = u32::from_be_bytes(len);
    if n == 0 || n > MAX_FRAME {
        return Err(FrameError::Length(n));
    }
    // grown as the bytes arrive: a lying length costs no more than what is sent
    let mut body = Vec::new();
    let got = r.by_ref().take(n as u64).read_to_end(&mut body).map_err(FrameError::Io)?;
    if got < n as usize {
        return Err(FrameError::Truncated { got: 4 + got });
    }
    Value::decode(&body).map(Some).map_err(FrameError::Cbor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_go_round_and_bad_ones_are_errors() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &Value::text("hi")).unwrap();
        write_frame(&mut buf, &Value::Uint(7)).unwrap();
        let mut r = &buf[..];
        assert_eq!(read_frame(&mut r).unwrap(), Some(Value::text("hi")));
        assert_eq!(read_frame(&mut r).unwrap(), Some(Value::Uint(7)));
        assert!(read_frame(&mut r).unwrap().is_none());

        let mut r = &b"hello world\n"[..];
        let e = read_frame(&mut r).unwrap_err();
        assert!(e.to_string().contains("\"hell\""), "{e}");
        let mut r = &[0u8, 0, 0, 5, 0x00][..];
        assert!(matches!(read_frame(&mut r), Err(FrameError::Truncated { got: 5 })));
        let mut r = &[0u8, 0][..];
        assert!(matches!(read_frame(&mut r), Err(FrameError::Truncated { got: 2 })));
        let mut r = &[0u8, 0, 0, 2, 0x00, 0x00][..];
        assert!(matches!(read_frame(&mut r), Err(FrameError::Cbor(_))));
    }
}
