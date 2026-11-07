//! tmi/chunking.py: split large blobs into shareable pieces.
//!
//! `storage_segments` cuts a payload into volatile spans (kept together in
//! one per-version "glue" blob) and shareable pieces: CLIP layer records at
//! tile-record boundaries, everything else by content-defined chunking on
//! the frozen gear table.

pub const PIECE_MIN: usize = 64 * 1024;
pub const PIECE_MAX: usize = 2 * 1024 * 1024;
pub const CDC_BITS: u32 = 19;
pub const WINDOW: usize = 64;
pub const MIN_SHARED: usize = 4096;
const CLIP_TILES_PER_CUT: u32 = 8;

/// chunking.py `GEAR_TABLE` (tests check the two stay equal).
pub const GEAR_TABLE: [u64; 256] = [
    0x31B33C423D7F58AD, 0x384CDD53B6C43D24, 0x7200027C2D6B1EFA, 0x2F924F085329A6CA,
    0x13253ED888F61C19, 0x2C17EBD77F0BC1BB, 0x30219F71A13FE7B5, 0x113AFB3EE7EB4FA2,
    0x4F44E8DFBC632646, 0x3DDA8C39B7F4476F, 0x2338845EC751305A, 0x7F707336C14FE31F,
    0x49D82A929E4BF10F, 0x4C1928D2E0103040, 0x41EE39E226595F4B, 0x6D926FB4E7B7B44D,
    0x108289034595D7AA, 0x03DCFB0AE4618810, 0x2E5072CE9348DE11, 0x7EC282A990139B32,
    0x64D3FB58DE2C4A23, 0x0EAC99F30308D841, 0x1B158F733B280688, 0x48FF02B4DB37DECE,
    0x218C775C976F5046, 0x3FAF71E7E1E12967, 0x2B8301B7AED20E75, 0x00556F7AAF200A7F,
    0x1767AAED5336B31C, 0x5A65BBA7F7FE4341, 0x5D625F538324E85E, 0x5D35424C6D8EAF98,
    0x6B8C12DAF010B48B, 0x36F5CB99E1D184D4, 0x62DDDEC309ABC241, 0x60C7BE93B6B92913,
    0x425ED2DFC5BCF841, 0x70C6BA9107E701E2, 0x755187D8F28607F4, 0x4263F4C6FA78F07C,
    0x45B3C5F6A32716DD, 0x67917E7977F62C62, 0x25322A7094965606, 0x48124B858740877D,
    0x0D8A4E610390D337, 0x227F18C2C7EFB066, 0x345D71FE259E228A, 0x316E5301B464E0D1,
    0x1FDD8A7484C97CCE, 0x354BBDB2F89E7F87, 0x2FE117721ECEC88A, 0x34B2D25DEE06A6EE,
    0x41997307D6A026D5, 0x7BCCBEA755C67520, 0x16E9E32E3DEC4B89, 0x5FA889A78E9612BB,
    0x7A76DC523E390F52, 0x69D154535E004B77, 0x17A71AF7E91200B6, 0x48F765AC2EF13A80,
    0x5255C0F4EEBA84AD, 0x5045BEF8027A951F, 0x23A112F18A1B1CFE, 0x38D86BC370ACB141,
    0x6A2CEBF4FE53FC63, 0x6EC4A00584134301, 0x353795E9E06D0FD6, 0x04B93D1EEC732899,
    0x501E81CA522BC61F, 0x77D1D0679E6AAFE9, 0x1012244FC5B256A5, 0x3A7F413F06B91418,
    0x4A8E240BEF0434A2, 0x73D491C4CBE1C410, 0x22966DB533164E9B, 0x5857BC3A206DFF6A,
    0x2A2FB75AE33AC50B, 0x42841F779A90A7C3, 0x2E36CC7814DA5248, 0x76B594307FA0A812,
    0x031D0695B19F5407, 0x2B4E88A6AEDDEA55, 0x48F30EF2DCBDC6E7, 0x675FFDD708A5D69B,
    0x08527597A525F681, 0x61999D80DE2719DD, 0x788494808DD5A972, 0x43AEF7535E113804,
    0x17E1116C65F350FE, 0x21764697EB1845D5, 0x7059661EE5A070AA, 0x338F11D4F5960D0E,
    0x7F0073CD449A0E39, 0x252466CFF5EEEAAA, 0x7F4A05E8A8071ED2, 0x48F887B12CA7A066,
    0x0794204575B96E9E, 0x6BB924993996C9D6, 0x3450F62901AE99C9, 0x299477851055314C,
    0x6261B69297CA8E9C, 0x633D914CF429116B, 0x1321B8FA4B781479, 0x7E54BAA4E9764B29,
    0x047F1CD5DE0DF593, 0x57D819BA6819792E, 0x423EAA7BD039DB25, 0x53BA31B6803C81C9,
    0x778BAC69AE74E3CE, 0x26A669BA1FE288D8, 0x5AC50CFCCEE3D1F0, 0x601E3F30DCEBF5B5,
    0x398EE2E054185B4B, 0x0F0098437804384F, 0x7AA0E754C1A82300, 0x3F9EA9695CB8BC0E,
    0x41A4481C74CDA682, 0x6B322A8CD83035BE, 0x31AE1067B21F905B, 0x2267D68AF877D736,
    0x63FEC503546F896F, 0x0CCE4D759B2CAC3B, 0x6B21D56D8F7B5E2C, 0x40C0DF312FC25987,
    0x7E5DF913A7FECDD2, 0x664D804AB0F99A31, 0x7D5B8F371EDDFAFE, 0x63ABB13CBD46644A,
    0x760E936114243A27, 0x5677E955692BB143, 0x3153952F1B5BA4A3, 0x1B4DAE97362328FB,
    0x1E346FE4E719F71E, 0x23F4FFE34B4669AD, 0x0CF0EA38C8FA8726, 0x4B3D88D468BC0412,
    0x7E92982782E4F564, 0x4EBA42DB9023847D, 0x31C6F54B4E991ADC, 0x0C0566C7D8D41AAE,
    0x332FE2CEBB3CE197, 0x7EE616921B6F09DC, 0x6CA9464709AFB827, 0x76A62BA7F5C807B3,
    0x50D71598EFF30014, 0x341CD424A6A56B36, 0x589E83DF83A426A0, 0x227995BEA91C53C3,
    0x3A947A41F3E73CA0, 0x2C53CF119666AEB8, 0x243BF31F911D48EE, 0x3D08DBA468651051,
    0x0B14F6A47F04DC31, 0x160909A1D93353E1, 0x6083030E95937649, 0x066EEE6AB847AA80,
    0x6F8DD8EDFD7B9609, 0x66570B73E1C48942, 0x088765C0FA9209AE, 0x33A6012381C6BD36,
    0x722B7E9731B2E372, 0x166C54B2AECBDFE4, 0x712E4C3C6EEE0E51, 0x41CE769AC13B8813,
    0x37B45EB068E2C27D, 0x43DBD876A3A7B1DA, 0x43A5425327E7361B, 0x7EDCA572850BBD9F,
    0x3F0B289B1FAD9039, 0x670008CBA0C296B5, 0x4871B474448A60BE, 0x50B9AF5D6EACF71C,
    0x0DDD1F098129BE02, 0x214B285ED3241C0F, 0x3969B5A5464B5F88, 0x0B1EE794A74F000B,
    0x134F280FBC8C6056, 0x0636A6AF9AF731E8, 0x786C97A79C82E184, 0x5E4CB3EF2B329E9C,
    0x19291E5942D76817, 0x272F1A4D9282ADA3, 0x7BA0DE75C41E23A2, 0x464647F0D81FBB2B,
    0x0D6D967A1D618326, 0x0682AC0ACB938486, 0x342D21D28DE47E0E, 0x00BD0963D8D689B1,
    0x17D9E34686585309, 0x56D3D0F3530D94DC, 0x7E7D748420CCFD43, 0x73049C6955B5F446,
    0x6221250C058ED11D, 0x6FEBFA643F9703B6, 0x40DE98A173EAF1B7, 0x51CF713BE8142441,
    0x2FE04C9C6BED2AB8, 0x444195A4570C195D, 0x66BE7177F5700CFE, 0x232B87D710391D72,
    0x2566903B58DD6D97, 0x388D6B9B66BD77EE, 0x31616187915CFBE1, 0x2DC9A59FD96BC6CD,
    0x2F673678BECBAE1E, 0x56DF4DC5BD2A1C8A, 0x7E9A588BE00B73A3, 0x339B63C3347F993F,
    0x657618E1381E526D, 0x55FF76C87A43FECC, 0x6711B2EF9426C02C, 0x2C0661F8B5087CDA,
    0x796BFB8F42C7E1D0, 0x408937B9B0EE2631, 0x65E2B5A2EB0739EC, 0x58B042C3424E6A3C,
    0x683503FBBCA3A63A, 0x576E3CCD209C2B83, 0x0AA06DEBF7D1B29F, 0x336171FA6AEC720D,
    0x0BA753C147B060EC, 0x571F462C60B6776C, 0x0D6EF25DA1236599, 0x1A0DDB93BFB295D9,
    0x734075F662E0B80A, 0x36D0D1A395381EE0, 0x7FEA8B2BC5B2B4FD, 0x314185EE2B78324E,
    0x540480F7BA241719, 0x4B6BD466159A70B2, 0x683B01436583A06F, 0x4618E23AEEC797A0,
    0x03EA74E190D85052, 0x1CEAEBB19E98220D, 0x580682E87676BD54, 0x790ACC98983BBF56,
    0x2A2B62EE61942339, 0x6CEC6DB31DB42A69, 0x0228F22F56D99078, 0x2C27AAFDD76831A2,
    0x402BCCE02F2AA8CB, 0x61354EC53BE5A91F, 0x719AA4EDF8A7B6C4, 0x183F8D6C3AC80CF1,
    0x4039A89F12D6A7D9, 0x6CD5BA262999EBE2, 0x5A1E81244568C48B, 0x2F20B71E3EA2386C,
    0x767E4E8273CD625C, 0x5EC6FD0C0F642D49, 0x58BB420E91B34DF5, 0x6878CAC166FA2752,
    0x28A68388F8B453CD, 0x207E705362B3FA6E, 0x4815FDE9DA6AAD7A, 0x2A456A7B57AC9DA0,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    pub start: usize,
    pub end: usize,
    pub shareable: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Volatile,
    Cdc,
    ClipRecords,
}

/// chunking.py `cdc_cuts(data, start, end)`.
pub fn cdc_cuts(data: &[u8], start: usize, end: usize) -> Vec<usize> {
    if end - start <= PIECE_MIN {
        return vec![end];
    }
    tmi_codec::cdc::cdc_cuts(data, start, end, &GEAR_TABLE, PIECE_MIN, PIECE_MAX, CDC_BITS, WINDOW)
        .expect("cdc parameters are fixed")
}

fn be_u64(data: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(data.get(at..at + 8)?.try_into().ok()?))
}

fn be_u32(data: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(data[at..at + 4].try_into().unwrap())
}

/// `_clip_spans`; None where Python raises (struct.error / ValueError /
/// IndexError) and falls back to one CDC span.
fn clip_spans(data: &[u8]) -> Option<Vec<(usize, usize, Kind)>> {
    let mut off = be_u64(data, 16)? as u128;
    let len = data.len() as u128;
    let mut spans = Vec::new();
    let mut volatile_from = 0usize;
    while off < len {
        let o = off as usize;
        if data.get(o..o + 4) != Some(b"CHNK".as_slice()) {
            return None;
        }
        let size = be_u64(data, o + 8)? as u128;
        let end = off + 16 + size;
        if end > len {
            return None;
        }
        let end = end as usize;
        let payload = o + 16;
        if data.get(o + 4..o + 8) == Some(b"Exta".as_slice())
            && size >= 56
            && data.get(payload + 8..payload + 16) == Some(b"extrnlid".as_slice())
        {
            let inner = payload + 56;
            let inner_size = be_u64(data, payload + 48)? as u128;
            let inner_end = (end as u128).min(inner as u128 + inner_size) as usize;
            spans.push((volatile_from, inner, Kind::Volatile));
            spans.push((inner, inner_end, Kind::ClipRecords));
            volatile_from = inner_end;
        }
        off = end as u128;
    }
    spans.push((volatile_from, data.len(), Kind::Volatile));
    Some(spans)
}

fn mdp2_spans(data: &[u8]) -> Option<Vec<(usize, usize, Kind)>> {
    let xml_size = u32::from_le_bytes(data.get(12..16)?.try_into().ok()?) as usize;
    data.get(16..20)?;
    let pack_start = 20 + xml_size;
    if pack_start > data.len() {
        return None;
    }
    Some(vec![(0, pack_start, Kind::Volatile), (pack_start, data.len(), Kind::Cdc)])
}

fn greedy_cuts(candidates: &[usize], start: usize, end: usize, boundaries: &[usize]) -> Vec<usize> {
    let mut cuts = Vec::new();
    let mut last = start;
    while end - last > PIECE_MIN {
        let i = candidates.partition_point(|&c| c < last + PIECE_MIN);
        let mut cut = if i < candidates.len() { candidates[i] } else { end };
        if cut - last > PIECE_MAX {
            let j = boundaries.partition_point(|&b| b <= last + PIECE_MAX) as isize - 1;
            cut = if j >= 0 && boundaries[j as usize] > last {
                boundaries[j as usize]
            } else {
                // a single record longer than PIECE_MAX stays whole
                boundaries[boundaries.partition_point(|&b| b <= last)]
            };
        }
        cuts.push(cut);
        last = cut;
    }
    if last < end {
        cuts.push(end);
    }
    cuts
}

/// The UTF-16BE of "BlockDataBeginChunk".
fn clip_begin() -> Vec<u8> {
    "BlockDataBeginChunk".encode_utf16().flat_map(|u| u.to_be_bytes()).collect()
}

fn clip_record_cuts(data: &[u8], start: usize, end: usize) -> Vec<usize> {
    let begin = clip_begin();
    let begin_len = (begin.len() / 2) as u32;
    let mut boundaries = Vec::new();
    let mut candidates = Vec::new();
    let mut p = start;
    while p + 8 <= end {
        let rec_size = be_u32(data, p) as usize;
        if rec_size < 8 || p + rec_size > end {
            break;
        }
        let rec_end = p + rec_size;
        boundaries.push(rec_end);
        if rec_size >= 50
            && be_u32(data, p + 4) == begin_len
            && data[p + 8..].starts_with(&begin)
            && be_u32(data, p + 46) % CLIP_TILES_PER_CUT == CLIP_TILES_PER_CUT - 1
        {
            candidates.push(rec_end);
        }
        p = rec_end;
    }
    if p - start < (end - start) / 2 {
        // not a record stream after all
        return cdc_cuts(data, start, end);
    }
    let mut cuts = if p - start <= PIECE_MIN { vec![p] } else { greedy_cuts(&candidates, start, p, &boundaries) };
    if p < end {
        cuts.push(end);
    }
    cuts
}

/// chunking.py `storage_segments`: volatile and shareable spans covering
/// `data` in order.
pub fn storage_segments(data: &[u8]) -> Vec<Segment> {
    let spans = if data.starts_with(b"CSFCHUNK") {
        clip_spans(data)
    } else if data.starts_with(b"mdipack\x00") {
        mdp2_spans(data)
    } else {
        None
    }
    .unwrap_or_else(|| vec![(0, data.len(), Kind::Cdc)]);
    let mut out = Vec::new();
    for (start, end, kind) in spans {
        if kind == Kind::Volatile {
            out.push(Segment { start, end, shareable: false });
            continue;
        }
        let cuts = if kind == Kind::Cdc { cdc_cuts(data, start, end) } else { clip_record_cuts(data, start, end) };
        let mut last = start;
        for cut in cuts {
            out.push(Segment { start: last, end: cut, shareable: cut - last >= MIN_SHARED });
            last = cut;
        }
    }
    let mut merged: Vec<Segment> = Vec::new();
    for seg in out {
        if seg.start == seg.end {
            continue;
        }
        match merged.last_mut() {
            Some(prev) if !seg.shareable && !prev.shareable => prev.end = seg.end,
            _ => merged.push(seg),
        }
    }
    merged
}
