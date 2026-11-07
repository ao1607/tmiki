"""Split large blobs into shareable pieces so successive versions dedupe.

A CLIP save rewrites the whole file, but the bytes of untouched layers come
back identical; only each Exta chunk's external id (regenerated on every save),
the SQLite chunk and edited layers change. ``storage_segments`` cuts a payload
into *volatile* spans (small or always-changing bytes, kept together in one
per-version blob) and *shareable* pieces. CLIP layer data is cut at tile-record
boundaries; anything else goes through content-defined chunking (CDC), whose
cut points depend only on nearby bytes — so an edit, or a shift of everything
after it, disturbs only the pieces around the edit.
"""
from __future__ import annotations

import bisect
import struct
from dataclasses import dataclass

# Piece sizes: none shorter than MIN (except at a span's end) or longer than
# MAX. CDC cuts at the first position past MIN where 1 in 2**BITS qualifies,
# so pieces average about MIN + 2**BITS ≈ 576 KiB. MIN must stay well below
# the spacing of candidates: after an edit the two versions' cut chains
# realign only once both take the same candidate, and a MIN that skips
# candidates can keep them out of step for many pieces. Larger pieces share
# less around an edit; smaller ones mean more object files, each costing a
# file open (and, on Windows, an antivirus scan of every new file).
PIECE_MIN = 64 * 1024
PIECE_MAX = 2 * 1024 * 1024
CDC_BITS = 19
_WINDOW = 64
# Shareable pieces below this size are not worth an object of their own.
MIN_SHARED = 4096
# A CLIP tile record ends a piece only at every 8th tile (once past PIECE_MIN).
_CLIP_TILES_PER_CUT = 8
# The CDC gear table: 256 fixed 63-bit values. Cut points (and so which
# pieces dedupe across versions) depend on every one of them, so they are
# written out rather than drawn from numpy's generator, whose output numpy
# does not promise to keep across versions. They are what
# ``np.random.default_rng(0x746D69).integers(0, 2**63, 256, dtype=np.uint64)``
# gave when the table was introduced (tests/test_storage_dedup.py checks).
GEAR_TABLE: tuple[int, ...] = (
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
)
_GEAR_LIST = list(GEAR_TABLE)

_CLIP_BEGIN = "BlockDataBeginChunk".encode("utf-16-be")


@dataclass(frozen=True)
class Segment:
    start: int
    end: int
    shareable: bool


def cdc_cuts(data: bytes | memoryview, start: int = 0, end: int | None = None) -> list[int]:
    """Cut points (exclusive ends, absolute offsets) splitting data[start:end].

    The rolling hash is the wrapping sum of a fixed random table over the last
    64 bytes; a position qualifies when the hash's top CDC_BITS bits are zero.
    tmi_accel scans on all cores (``tmiref.chunking.cdc_cuts_numpy`` is the
    numpy reference it is tested against)."""
    end = len(data) if end is None else end
    if end - start <= PIECE_MIN:
        return [end]
    from . import accel

    return accel.native().cdc_cuts(bytes(data), start, end, _GEAR_LIST, PIECE_MIN, PIECE_MAX, CDC_BITS, _WINDOW)


def _greedy_cuts(candidates, start: int, end: int, boundaries=None) -> list[int]:
    """From ``start``, cut at the first candidate at least PIECE_MIN on; when
    none comes before PIECE_MAX, cut at the last of ``boundaries`` (allowed
    cut points; any byte when None) that keeps the piece within PIECE_MAX.
    Both are ascending sequences of offsets."""
    cuts: list[int] = []
    last = start
    while end - last > PIECE_MIN:
        i = bisect.bisect_left(candidates, last + PIECE_MIN)
        cut = int(candidates[i]) if i < len(candidates) else end
        if cut - last > PIECE_MAX:
            if boundaries is None:
                cut = last + PIECE_MAX
            else:
                j = bisect.bisect_right(boundaries, last + PIECE_MAX) - 1
                # a single record longer than PIECE_MAX stays whole
                cut = int(boundaries[j]) if j >= 0 and boundaries[j] > last else int(
                    boundaries[bisect.bisect_right(boundaries, last)])
        cuts.append(cut)
        last = cut
    if last < end:
        cuts.append(end)
    return cuts


def storage_segments(data: bytes) -> list[Segment]:
    """Volatile and shareable spans covering ``data`` in order."""
    try:
        if data[:8] == b"CSFCHUNK":
            spans = _clip_spans(data)
        elif data[:8] == b"mdipack\x00":
            spans = _mdp2_spans(data)
        else:
            spans = [(0, len(data), "cdc")]
    except (struct.error, ValueError, IndexError):
        spans = [(0, len(data), "cdc")]
    out: list[Segment] = []
    for start, end, kind in spans:
        if kind == "volatile":
            out.append(Segment(start, end, False))
            continue
        cuts = cdc_cuts(data, start, end) if kind == "cdc" else _clip_record_cuts(data, start, end)
        last = start
        for cut in cuts:
            out.append(Segment(last, cut, cut - last >= MIN_SHARED))
            last = cut
    merged: list[Segment] = []
    for seg in out:
        if seg.start == seg.end:
            continue
        if merged and not seg.shareable and not merged[-1].shareable:
            merged[-1] = Segment(merged[-1].start, seg.end, False)
        else:
            merged.append(seg)
    return merged


def _clip_spans(data: bytes) -> list[tuple[int, int, str]]:
    # Each Exta payload: u64 0x28, "extrnlid" + 32 hex digits (new on every
    # save), u64 inner size, the inner tile records. Everything but the inner
    # records — chunk headers, ids, Head, SQLi, Foot — is volatile.
    off = struct.unpack(">Q", data[16:24])[0]
    spans: list[tuple[int, int, str]] = []
    volatile_from = 0
    while off < len(data):
        if data[off:off + 4] != b"CHNK":
            raise ValueError("bad CLIP chunk")
        size = struct.unpack(">Q", data[off + 8:off + 16])[0]
        end = off + 16 + size
        if end > len(data):
            raise ValueError("truncated CLIP chunk")
        payload = off + 16
        if data[off + 4:off + 8] == b"Exta" and size >= 56 and data[payload + 8:payload + 16] == b"extrnlid":
            inner = payload + 56
            inner_end = min(end, inner + struct.unpack(">Q", data[payload + 48:payload + 56])[0])
            spans.append((volatile_from, inner, "volatile"))
            spans.append((inner, inner_end, "clip-records"))
            volatile_from = inner_end
        off = end
    spans.append((volatile_from, len(data), "volatile"))
    return spans


def _clip_record_cuts(data: bytes, start: int, end: int) -> list[int]:
    """Cut an Exta's records into pieces. A tile record may end a piece when
    its block index is the last of a group of eight: block indices are
    positions, so an edit that resizes one record moves no other cut."""
    boundaries: list[int] = []
    candidates: list[int] = []
    unpack = struct.Struct(">I").unpack_from
    begin_len = len(_CLIP_BEGIN) // 2
    p = start
    while p + 8 <= end:
        rec_size = unpack(data, p)[0]
        if rec_size < 8 or p + rec_size > end:
            break
        rec_end = p + rec_size
        boundaries.append(rec_end)
        if (
            rec_size >= 50
            and unpack(data, p + 4)[0] == begin_len
            and data.startswith(_CLIP_BEGIN, p + 8)
            and unpack(data, p + 46)[0] % _CLIP_TILES_PER_CUT == _CLIP_TILES_PER_CUT - 1
        ):
            candidates.append(rec_end)
        p = rec_end
    if p - start < (end - start) // 2:
        # not a record stream after all
        return cdc_cuts(data, start, end)
    # The records are followed by a short BlockStatus / BlockCheckSum table
    # in another framing; it changes with any tile, so it becomes a piece of
    # its own (too small to share, it joins the volatile bytes).
    if p - start <= PIECE_MIN:
        cuts = [p]
    else:
        cuts = _greedy_cuts(candidates, start, p, boundaries)
    return cuts + [end] if p < end else cuts


def _mdp2_spans(data: bytes) -> list[tuple[int, int, str]]:
    # Header + XML (layer properties) change on most saves; the PAC records
    # after them hold the layer tiles.
    _reserved, xml_size, _pack_size = struct.unpack("<III", data[8:20])
    pack_start = 20 + xml_size
    if pack_start > len(data):
        raise ValueError("bad MDP2 header")
    return [(0, pack_start, "volatile"), (pack_start, len(data), "cdc")]
