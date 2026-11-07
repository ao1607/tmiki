//! GIF frame 0 as Pillow 12.3.0 opens and loads it (GifImagePlugin `_open`,
//! `_seek(0)`, `load_prepare`, ImageFile.load's read loop, libImaging's
//! GifDecode.c), so `convert("RGBA")` gives Pillow's bytes.
//!
//! - The logical screen size grows to the first frame's extent.
//! - Mode "P" when the frame has a palette Pillow keeps (a local one, else
//!   the global one; a palette that is the identity grey ramp is dropped),
//!   else "L" (pixel = index).
//! - Outside the frame the canvas is the transparent index (when the
//!   graphic control extension sets one), else 0. Frame 0 pixels are all
//!   written (Pillow passes transparency -1 to the decoder for frame 0) and
//!   the index becomes `info["transparency"]`.
//! - The LZW decoder is GifDecode.c's: a code past the table is an error,
//!   the end code only returns to the caller (which reads on), and the image
//!   is done when its last row is written. The caller is ImageFile.load:
//!   reads of 64 KiB from the image data, "truncated" when a read comes back
//!   empty before the decoder finishes.
//!
//! Declined: a frame whose local palette is the grey ramp while the global
//! one is not (Pillow then loads mode "L" but realizes the global palette
//! on the core image).
//!
//! GIF has no dpi or ICC profile in Pillow's `info`.

use tmi_pillow::image::{Image, Mode, Palette, Transparency};

use crate::{Cursor, DecodeError, Format, Info, Limit, Opened};

/// `_accept`.
pub fn accept(data: &[u8]) -> bool {
    data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a")
}

fn corrupt(msg: &str) -> DecodeError {
    DecodeError::Corrupt(Format::Gif, msg.to_string())
}

fn i16le(b: &[u8], at: usize) -> Option<usize> {
    Some(u16::from_le_bytes([*b.get(at)?, *b.get(at + 1)?]) as usize)
}

/// `GifImageFile.data()`: one sub-block, or None for a terminator / EOF.
fn sub_block<'a>(r: &mut Cursor<'a>) -> Option<&'a [u8]> {
    let s = r.read(1);
    if !s.is_empty() && s[0] != 0 {
        return Some(r.read(s[0] as usize));
    }
    None
}

/// `_is_palette_needed(p)`: Err for the IndexError a short last entry can
/// raise (the chained comparison stops at the first mismatch).
fn palette_needed(p: &[u8]) -> Result<bool, DecodeError> {
    let short = || corrupt("palette index out of range");
    for i in (0..p.len()).step_by(3) {
        let v = (i / 3) as i64;
        if v != p[i] as i64 {
            return Ok(true);
        }
        let g = *p.get(i + 1).ok_or_else(short)?;
        if p[i] != g {
            return Ok(true);
        }
        let b = *p.get(i + 2).ok_or_else(short)?;
        if g != b {
            return Ok(true);
        }
    }
    Ok(false)
}

/// What `_open` and `_seek(0)` leave for `load()`.
struct Frame0 {
    size: (usize, usize),
    extent: (usize, usize, usize, usize),
    /// The palette `self.palette` holds (raw RGB bytes).
    palette: Option<Vec<u8>>,
    /// `_frame_palette` is truthy: mode "P".
    p_mode: bool,
    transparency: Option<u8>,
    interlace: bool,
    bits: u8,
    offset: usize,
}

fn parse(data: &[u8]) -> Result<Frame0, DecodeError> {
    let mut r = Cursor::new(data);
    let s = r.read(13);
    if !accept(s) {
        return Err(corrupt("not a GIF file"));
    }
    if s.len() < 13 {
        // A short header: struct.error / IndexError, or EOF before a frame.
        return Err(corrupt("truncated header"));
    }
    let mut size = (i16le(s, 6).unwrap(), i16le(s, 8).unwrap());
    let flags = s[10];
    let mut global: Option<Vec<u8>> = None;
    if flags & 128 != 0 {
        let bits = (flags & 7) + 1;
        let p = r.read(3 << bits);
        if palette_needed(p)? {
            global = Some(p.to_vec());
        }
    }

    // _seek(0)
    let mut s = r.read(1);
    if s.is_empty() || s == b";" {
        return Err(corrupt("no more images in GIF file"));
    }
    // None: no local palette; Some(None): a grey-ramp one (`False`).
    let mut local: Option<Option<Vec<u8>>> = None;
    let mut transparency: Option<u8> = None;
    let mut found: Option<(usize, usize, usize, usize, bool, u8, usize)> = None;
    loop {
        if s.is_empty() {
            s = r.read(1);
        }
        if s.is_empty() || s == b";" {
            break;
        } else if s == b"!" {
            let label = r.read(1);
            let block = sub_block(&mut r);
            let Some(&label) = label.first() else {
                return Err(corrupt("extension label missing"));
            };
            if label == 249 && block.is_some() {
                let block = block.unwrap();
                let Some(&flags) = block.first() else {
                    return Err(corrupt("short graphic control extension"));
                };
                if flags & 1 != 0 {
                    transparency = Some(*block.get(3).ok_or_else(|| corrupt("short graphic control extension"))?);
                }
                i16le(block, 1).ok_or_else(|| corrupt("short graphic control extension"))?;
            } else if label == 254 {
                // The comment's own loop consumes its sub-blocks.
                let mut block = block;
                while block.is_some_and(|b| !b.is_empty()) {
                    block = sub_block(&mut r);
                }
                s = b"";
                continue;
            } else if label == 255 && block.is_some() {
                if block.unwrap().starts_with(b"NETSCAPE2.0") {
                    sub_block(&mut r);
                }
            }
            while sub_block(&mut r).is_some_and(|b| !b.is_empty()) {}
        } else if s == b"," {
            let d = r.read(9);
            let (Some(x0), Some(y0), Some(w), Some(h)) = (i16le(d, 0), i16le(d, 2), i16le(d, 4), i16le(d, 6)) else {
                return Err(corrupt("short image descriptor"));
            };
            let Some(&flags) = d.get(8) else {
                return Err(corrupt("short image descriptor"));
            };
            let (x1, y1) = (x0 + w, y0 + h);
            if x1 > size.0 || y1 > size.1 {
                size = (size.0.max(x1), size.1.max(y1));
            }
            let interlace = flags & 64 != 0;
            if flags & 128 != 0 {
                let bits = (flags & 7) + 1;
                let p = r.read(3 << bits);
                local = Some(if palette_needed(p)? { Some(p.to_vec()) } else { None });
            }
            let Some(&bits) = r.read(1).first() else {
                return Err(corrupt("image data missing"));
            };
            found = Some((x0, y0, x1, y1, interlace, bits, r.tell()));
            break;
        }
        s = b"";
    }
    let Some((x0, y0, x1, y1, interlace, bits, offset)) = found else {
        return Err(corrupt("image not found in GIF frame"));
    };
    // _frame_palette = palette if palette is not None else global_palette
    let frame_palette: Option<Vec<u8>> = match &local {
        Some(l) => l.clone(),
        None => global.clone(),
    };
    let p_mode = frame_palette.is_some();
    // self.palette = palette if palette else copy(global_palette) or None
    let palette = match &local {
        Some(Some(l)) => Some(l.clone()),
        _ => global.clone(),
    };
    if !p_mode && palette.is_some() {
        return Err(DecodeError::Declined("GIF: grey-ramp local palette under a global one"));
    }
    if size.0 == 0 || size.1 == 0 {
        return Err(corrupt("not identified by this driver"));
    }
    Ok(Frame0 { size, extent: (x0, y0, x1, y1), palette, p_mode, transparency, interlace, bits, offset })
}

/// The size `Image.open(data).size` reports.
pub fn size(data: &[u8]) -> Result<(usize, usize), DecodeError> {
    Ok(parse(data)?.size)
}

pub(crate) fn open(data: &[u8], limit: Limit) -> Result<Opened, DecodeError> {
    let f = parse(data)?;
    let (w, h) = f.size;
    limit.check(w, h)?;
    let (x0, y0, x1, y1) = f.extent;
    // load_prepare: the canvas, then ImageFile.load's setimage check.
    let fill = f.transparency.unwrap_or(0);
    let mut px = vec![fill; w * h];
    if x1 <= x0 || y1 <= y0 {
        return Err(corrupt("tile cannot extend outside image"));
    }
    let mut dec = GifDecoder::new(f.bits, f.interlace, (x0, y0, x1 - x0, y1 - y0), w);
    // ImageFile.load: read MAXBLOCK at a time, feed what is not consumed.
    const MAXBLOCK: usize = 65536;
    let mut pos = f.offset;
    let mut b: Vec<u8> = Vec::new();
    loop {
        let end = pos.saturating_add(MAXBLOCK).min(data.len());
        let s = &data[pos.min(end)..end];
        pos = end.max(pos);
        if s.is_empty() {
            return Err(corrupt("image file is truncated"));
        }
        b.extend_from_slice(s);
        let n = dec.decode(&mut px, &b);
        if n < 0 {
            break;
        }
        b.drain(..n as usize);
    }
    if dec.errcode < 0 {
        return Err(corrupt("decoder error"));
    }
    let mode = if f.p_mode { Mode::P } else { Mode::L };
    let mut image = Image::new(mode, w, h, px);
    if f.p_mode {
        image.palette = Some(Palette::from_rgb(f.palette.as_deref().unwrap_or(&[])));
    }
    image.transparency = f.transparency.map(|t| Transparency::Int(t as i64));
    Ok(Opened { image, format: Format::Gif, info: Info::default() })
}

const GIFBITS: i32 = 12;
const GIFTABLE: usize = 1 << GIFBITS;
const GIFBUFFER: usize = 1 << GIFBITS;

const IMAGING_CODEC_OVERRUN: i32 = -1;
const IMAGING_CODEC_BROKEN: i32 = -2;
const IMAGING_CODEC_CONFIG: i32 = -8;

/// GifDecode.c's state (`ImagingCodecState` + `GIFDECODERSTATE`), with the
/// image passed in on each call.
struct GifDecoder {
    // ImagingCodecState
    state: i32,
    x: i32,
    y: i32,
    xoff: usize,
    yoff: usize,
    xsize: i32,
    ysize: i32,
    stride: usize,
    errcode: i32,
    // GIFDECODERSTATE
    bits: i32,
    interlace: i32,
    step: i32,
    repeat: i32,
    bitbuffer: i32,
    bitcount: i32,
    blocksize: i32,
    codesize: i32,
    codemask: i32,
    clear: i32,
    end: i32,
    lastcode: i32,
    lastdata: u8,
    bufferindex: usize,
    buffer: Box<[u8; GIFBUFFER]>,
    link: Box<[u16; GIFTABLE]>,
    data: Box<[u8; GIFTABLE]>,
    next: i32,
}

impl GifDecoder {
    fn new(bits: u8, interlace: bool, (xoff, yoff, xsize, ysize): (usize, usize, usize, usize), stride: usize) -> Self {
        GifDecoder {
            state: 0,
            x: 0,
            y: 0,
            xoff,
            yoff,
            xsize: xsize as i32,
            ysize: ysize as i32,
            stride,
            errcode: 0,
            bits: bits as i32,
            interlace: interlace as i32,
            step: 0,
            repeat: 0,
            bitbuffer: 0,
            bitcount: 0,
            blocksize: 0,
            codesize: 0,
            codemask: 0,
            clear: 0,
            end: 0,
            lastcode: 0,
            lastdata: 0,
            bufferindex: 0,
            buffer: Box::new([0; GIFBUFFER]),
            link: Box::new([0; GIFTABLE]),
            data: Box::new([0; GIFTABLE]),
            next: 0,
        }
    }

    /// The image offset of the current output position.
    fn out_at(&self) -> usize {
        (self.y as usize + self.yoff) * self.stride + self.xoff + self.x as usize
    }

    /// NEWLINE: false where the C macro returns -1.
    fn newline(&mut self) -> bool {
        self.x = 0;
        self.y += self.step;
        while self.y >= self.ysize {
            match self.interlace {
                1 => {
                    self.y = 4;
                    self.repeat = 4;
                    self.interlace = 2;
                }
                2 => {
                    self.step = 4;
                    self.y = 2;
                    self.repeat = 2;
                    self.interlace = 3;
                }
                3 => {
                    self.step = 2;
                    self.y = 1;
                    self.repeat = 1;
                    self.interlace = 0;
                }
                _ => return false,
            }
        }
        true
    }

    /// `ImagingGifDecode(im, state, buffer, bytes)`: the bytes consumed, or
    /// -1 (done, or an error in `errcode`).
    fn decode(&mut self, im: &mut [u8], buffer: &[u8]) -> i64 {
        let mut ptr = 0usize;
        let mut bytes = buffer.len() as i64;
        if self.state == 0 {
            if self.bits < 0 || self.bits > 12 {
                self.errcode = IMAGING_CODEC_CONFIG;
                return -1;
            }
            self.clear = 1 << self.bits;
            self.end = self.clear + 1;
            if self.interlace != 0 {
                self.interlace = 1;
                self.step = 8;
                self.repeat = 8;
            } else {
                self.step = 1;
            }
            self.state = 1;
        }
        let mut out = self.out_at();
        loop {
            if self.state == 1 {
                self.next = self.clear + 2;
                self.codesize = self.bits + 1;
                self.codemask = (1 << self.codesize) - 1;
                self.bufferindex = GIFBUFFER;
                self.state = 2;
            }
            // The string to copy: (from the history buffer, start, length)
            // or the one byte lastdata.
            let (from_buffer, pstart, i): (bool, usize, usize);
            if self.bufferindex < GIFBUFFER {
                from_buffer = true;
                i = GIFBUFFER - self.bufferindex;
                pstart = self.bufferindex;
                self.bufferindex = GIFBUFFER;
            } else {
                while self.bitcount < self.codesize {
                    if self.blocksize > 0 {
                        let c = buffer[ptr] as i32;
                        ptr += 1;
                        bytes -= 1;
                        self.blocksize -= 1;
                        self.bitbuffer |= c << self.bitcount;
                        self.bitcount += 8;
                    } else {
                        if bytes < 1 {
                            return ptr as i64;
                        }
                        let c = buffer[ptr] as i64;
                        if bytes < c + 1 {
                            return ptr as i64;
                        }
                        self.blocksize = c as i32;
                        ptr += 1;
                        bytes -= 1;
                    }
                }
                let mut c = self.bitbuffer & self.codemask;
                self.bitbuffer >>= self.codesize;
                self.bitcount -= self.codesize;
                if c == self.clear {
                    if self.state != 2 {
                        self.state = 1;
                    }
                    continue;
                }
                if c == self.end {
                    break;
                }
                from_buffer = false;
                pstart = 0;
                i = 1;
                if self.state == 2 {
                    if c > self.clear {
                        self.errcode = IMAGING_CODEC_BROKEN;
                        return -1;
                    }
                    self.lastdata = c as u8;
                    self.lastcode = c;
                    self.state = 3;
                } else {
                    let thiscode = c;
                    if c > self.next {
                        self.errcode = IMAGING_CODEC_BROKEN;
                        return -1;
                    }
                    if c == self.next {
                        if self.bufferindex == 0 {
                            self.errcode = IMAGING_CODEC_BROKEN;
                            return -1;
                        }
                        self.bufferindex -= 1;
                        self.buffer[self.bufferindex] = self.lastdata;
                        c = self.lastcode;
                    }
                    while c >= self.clear {
                        if self.bufferindex == 0 || c as usize >= GIFTABLE {
                            self.errcode = IMAGING_CODEC_BROKEN;
                            return -1;
                        }
                        self.bufferindex -= 1;
                        self.buffer[self.bufferindex] = self.data[c as usize];
                        c = self.link[c as usize] as i32;
                    }
                    self.lastdata = c as u8;
                    if (self.next as usize) < GIFTABLE {
                        self.data[self.next as usize] = c as u8;
                        self.link[self.next as usize] = self.lastcode as u16;
                        if self.next == self.codemask && self.codesize < GIFBITS {
                            self.codesize += 1;
                            self.codemask = (1 << self.codesize) - 1;
                        }
                        self.next += 1;
                    }
                    self.lastcode = thiscode;
                }
            }

            if self.y >= self.ysize {
                self.errcode = IMAGING_CODEC_OVERRUN;
                return -1;
            }
            // Copy the string into the image (transparency is -1 for frame
            // 0, so every byte is written).
            for k in 0..i {
                let v = if from_buffer { self.buffer[pstart + k] } else { self.lastdata };
                im[out] = v;
                out += 1;
                self.x += 1;
                if self.x >= self.xsize {
                    if !self.newline() {
                        return -1;
                    }
                    if self.y < self.ysize {
                        out = self.out_at();
                    }
                }
            }
        }
        ptr as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal LZW encoder (no table growth past the first code size
    /// change: a clear code before every pixel keeps the table empty).
    fn lzw_clear_each(pixels: &[u8], min_bits: u8) -> Vec<u8> {
        let clear = 1u32 << min_bits;
        let size = min_bits as u32 + 1;
        let mut bits: u64 = 0;
        let mut n = 0u32;
        let mut out = Vec::new();
        let mut put = |code: u32, out: &mut Vec<u8>| {
            bits |= (code as u64) << n;
            n += size;
            while n >= 8 {
                out.push(bits as u8);
                bits >>= 8;
                n -= 8;
            }
        };
        for &p in pixels {
            put(clear, &mut out);
            put(p as u32, &mut out);
        }
        put(clear + 1, &mut out);
        if n > 0 {
            out.push(bits as u8);
        }
        out
    }

    fn gif(w: u16, h: u16, pal: &[u8], gce: Option<u8>, frame: (u16, u16, u16, u16), px: &[u8]) -> Vec<u8> {
        let mut v = b"GIF89a".to_vec();
        v.extend_from_slice(&w.to_le_bytes());
        v.extend_from_slice(&h.to_le_bytes());
        v.extend_from_slice(&[0x80 | 1, 0, 0]);
        let mut p = pal.to_vec();
        p.resize(12, 0);
        v.extend_from_slice(&p);
        if let Some(t) = gce {
            v.extend_from_slice(&[0x21, 0xF9, 4, 1, 0, 0, t, 0]);
        }
        v.push(b',');
        for x in [frame.0, frame.1, frame.2, frame.3] {
            v.extend_from_slice(&x.to_le_bytes());
        }
        v.push(0);
        v.push(2);
        let lzw = lzw_clear_each(px, 2);
        for c in lzw.chunks(255) {
            v.push(c.len() as u8);
            v.extend_from_slice(c);
        }
        v.extend_from_slice(&[0, b';']);
        v
    }

    #[test]
    fn frame_grows_canvas_and_fills_transparency() {
        let pal = [255, 0, 0, 0, 255, 0, 0, 0, 255, 9, 9, 9];
        let data = gif(1, 1, &pal, Some(3), (1, 0, 2, 1), &[0, 1]);
        let o = open(&data, Limit(None)).unwrap();
        assert_eq!((o.image.width, o.image.height, o.image.mode), (3, 1, Mode::P));
        assert_eq!(o.image.data, vec![3, 0, 1]);
        assert_eq!(o.image.transparency, Some(Transparency::Int(3)));
    }

    #[test]
    fn grey_ramp_palette_is_mode_l() {
        let pal = [0, 0, 0, 1, 1, 1, 2, 2, 2, 3, 3, 3];
        let data = gif(2, 1, &pal, None, (0, 0, 2, 1), &[2, 3]);
        let o = open(&data, Limit(None)).unwrap();
        assert_eq!((o.image.mode, o.image.data.clone()), (Mode::L, vec![2, 3]));
    }

    #[test]
    fn truncated_data_is_corrupt() {
        let pal = [255, 0, 0, 0, 255, 0, 0, 0, 255, 9, 9, 9];
        let mut data = gif(2, 2, &pal, None, (0, 0, 2, 2), &[0, 1, 2, 3]);
        data.truncate(data.len() - 6);
        assert!(matches!(open(&data, Limit(None)), Err(DecodeError::Corrupt(..))));
    }
}
