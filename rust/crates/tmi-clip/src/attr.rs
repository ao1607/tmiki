//! `Offscreen.Attribute` (`parse_offscreen_attribute`, clip.py:449-531),
//! every check kept: the parse fails exactly where Python raises.

#[derive(Clone, Debug)]
pub struct OffscreenAttribute {
    pub pixel_width: usize,
    pub pixel_height: usize,
    pub tile_cols: usize,
    pub tile_rows: usize,
    /// "color-alpha-bgra", "gray-alpha", "single-channel" or "unknown-N".
    pub channel_profile: String,
    pub init_color_words: Vec<u32>,
}

/// Python's `buf[a:b]` (clamped, empty when b < a).
fn sl(buf: &[u8], a: usize, b: usize) -> &[u8] {
    let a = a.min(buf.len());
    let b = b.min(buf.len()).max(a);
    &buf[a..b]
}

fn u32_exact(buf: &[u8]) -> Result<u32, String> {
    let b: [u8; 4] = buf.try_into().map_err(|_| "struct.error: unpack requires a buffer of 4 bytes".to_string())?;
    Ok(u32::from_be_bytes(b))
}

fn u32s(buf: &[u8]) -> Result<Vec<u32>, String> {
    if buf.len() % 4 != 0 {
        return Err(format!("u32 buffer length is not aligned: {}", buf.len()));
    }
    Ok(buf.chunks_exact(4).map(|c| u32::from_be_bytes(c.try_into().unwrap())).collect())
}

fn utf16be(buf: &[u8]) -> Result<String, String> {
    if buf.len() % 2 != 0 {
        return Err("truncated utf-16".into());
    }
    let units: Vec<u16> = buf.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
    String::from_utf16(&units).map_err(|e| e.to_string())
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn name_bytes(name: &str) -> Vec<u8> {
    name.encode_utf16().flat_map(|u| u.to_be_bytes()).collect()
}

pub fn parse_offscreen_attribute(payload: &[u8]) -> Result<OffscreenAttribute, String> {
    if payload.len() < 194 {
        return Err(format!("Offscreen.Attribute too short: {}", payload.len()));
    }
    let block_table_span = u32_exact(&payload[12..16])? as u64;
    let name_len = u32_exact(&payload[16..20])? as usize;
    let parameter_name_end = 20 + name_len * 2;
    if parameter_name_end > payload.len() {
        return Err("Offscreen.Attribute Parameter name overruns payload".into());
    }
    if utf16be(&payload[20..parameter_name_end])? != "Parameter" {
        return Err("unexpected Offscreen.Attribute parameter name".into());
    }
    let p = parameter_name_end;
    let head = sl(payload, p, p + 16);
    if head.len() != 16 {
        return Err("struct.error: unpack requires a buffer of 16 bytes".into());
    }
    let pixel_width = u32_exact(&head[0..4])? as usize;
    let pixel_height = u32_exact(&head[4..8])? as usize;
    let tile_cols = u32_exact(&head[8..12])? as usize;
    let tile_rows = u32_exact(&head[12..16])? as usize;
    let channel_words = u32s(sl(payload, p + 16, p + 80))?;
    let first = *channel_words.first().ok_or("IndexError: tuple index out of range")?;
    let channel_profile = match first {
        33 => "color-alpha-bgra".to_string(),
        17 => "gray-alpha".to_string(),
        1 => "single-channel".to_string(),
        n => format!("unknown-{n}"),
    };

    let init_name_off = find(payload, &name_bytes("InitColor"));
    let block_name_off = find(payload, &name_bytes("BlockSize"));
    let (init_name_off, block_name_off) = match (init_name_off, block_name_off) {
        (Some(a), Some(b)) if a >= 4 && b >= 4 => (a, b),
        _ => return Err("Offscreen.Attribute missing InitColor or BlockSize section".into()),
    };
    let init_off = init_name_off - 4;
    let init_name_len = u32_exact(sl(payload, init_off, init_off + 4))? as usize;
    if init_name_len != 9 {
        return Err(format!("bad InitColor name length: {init_name_len}"));
    }
    let init_size_off = init_off + 4 + init_name_len * 2;
    let init_record_size = u32_exact(sl(payload, init_size_off, init_size_off + 4))? as i64;
    let primary_start = init_size_off + 4;
    // init_primary_end = start + size - 4 (may lie before start: an empty slice)
    let primary_end = primary_start as i64 + init_record_size - 4;
    let block_off = block_name_off - 4;
    let init_color_words = if primary_end <= primary_start as i64 {
        Vec::new()
    } else {
        u32s(sl(payload, primary_start, primary_end as usize))?
    };
    // init_color_extra_words: parsed (and checked) though unused.
    let extra_start = primary_end.max(0) as usize;
    if extra_start < block_off {
        u32s(sl(payload, extra_start, block_off))?;
    } else if primary_end < 0 {
        // payload[negative:block_off] counts from the end in Python
        return Err("negative InitColor record end".into());
    }

    let block_name_len = u32_exact(sl(payload, block_off, block_off + 4))? as usize;
    if block_name_len != 9 {
        return Err(format!("bad BlockSize name length: {block_name_len}"));
    }
    let block_size_off = block_off + 4 + block_name_len * 2;
    let three = sl(payload, block_size_off, block_size_off + 12);
    if three.len() != 12 {
        return Err("struct.error: unpack requires a buffer of 12 bytes".into());
    }
    let block_count = u32_exact(&three[4..8])? as u64;
    let block_entry_bytes = u32_exact(&three[8..12])? as u64;
    let table_off = (block_size_off + 12) as u64;
    let table_end = table_off + block_count * block_entry_bytes;
    if block_entry_bytes != 4 {
        return Err(format!("unsupported BlockSize entry width: {block_entry_bytes}"));
    }
    if table_end != payload.len() as u64 {
        return Err(format!("BlockSize table ends at {table_end}, attribute length is {}", payload.len()));
    }
    if block_count != (tile_cols as u64) * (tile_rows as u64) {
        return Err(format!("BlockSize count {block_count} != grid {tile_cols}*{tile_rows}"));
    }
    if block_table_span != 34 + block_count * block_entry_bytes {
        return Err(format!("unexpected table span {block_table_span}"));
    }
    Ok(OffscreenAttribute { pixel_width, pixel_height, tile_cols, tile_rows, channel_profile, init_color_words })
}

/// `_mask_init_alpha`: a mask's value outside its painted tiles.
pub fn mask_init_alpha(attr: &OffscreenAttribute) -> u8 {
    let w = &attr.init_color_words;
    if w.len() < 2 || w[0] == 0 {
        return 0;
    }
    ((w[1] as u64 * 255 + 0x7FFF_FFFF) / 0xFFFF_FFFF).min(255) as u8
}
