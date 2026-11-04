//! JPEG as Pillow 13 reads it: its own marker walk for size and metadata (`JpegImagePlugin._open`), then
//! libjpeg-turbo 3.x with its defaults (ISLOW IDCT, fancy upsampling) for the pixels, bit for bit.
//!
//! Entropy decoding has one right answer for valid data, so only the lossy stages must copy libjpeg-turbo: the ISLOW
//! integer IDCT (`jidctint.c`), triangle-filter chroma upsampling with edge rows duplicated (`jdsample.c`,
//! `jdmainct.c`) and the fixed-point YCbCr tables (`jdcolor.c`); zune-jpeg differs in all three. Damaged data
//! follows libjpeg-turbo too: an early marker zero-fills the rest of its segment, restart markers resync as
//! `jpeg_resync_to_restart` does, and data that ends without a marker is truncated, which Pillow refuses.
//! Arithmetic coding, lossless and non-integral subsampling, and progressive images whose scans stop early (libjpeg
//! smooths those) are refused as unsupported; Python decodes them.

use super::meta::Metadata;

/// What Pillow's `Image.open` learns before decoding: the size the pixel limit is checked on.
pub(super) struct Header {
    pub width: u32,
    pub height: u32,
    pub components: u8,
    pub meta: Metadata,
}

/// Pillow's `JpegImageFile._open` marker walk, up to the first SOS. `None` where Pillow fails to open the file.
pub(super) fn header(data: &[u8]) -> Option<Header> {
    if !data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return None;
    }
    let mut pos = 3;
    let mut cur = Some(0xFFu8);
    let mut frame = None;
    let mut meta = Metadata::default();
    // `n = i16(read(2)) - 2; _safe_read(n)`: a short read fails, a non-positive length reads nothing.
    let segment = |pos: &mut usize| -> Option<&[u8]> {
        let len = u16::from_be_bytes(data.get(*pos..*pos + 2)?.try_into().unwrap()) as isize - 2;
        *pos += 2;
        if len <= 0 {
            return Some(&[]);
        }
        let s = data.get(*pos..*pos + len as usize)?;
        *pos += len as usize;
        Some(s)
    };
    while let Some(b) = cur {
        if b != 0xFF {
            cur = data.get(pos).copied();
            pos += 1;
            continue;
        }
        let Some(&marker) = data.get(pos) else { break };
        pos += 1;
        match marker {
            0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF | 0xDE => {
                let s = segment(&mut pos)?;
                if s.len() < 6 || s[0] != 8 || !matches!(s[5], 1 | 3 | 4) || (s.len() - 6) % 3 != 0 {
                    return None;
                }
                let be = |i: usize| u16::from_be_bytes([s[i], s[i + 1]]) as u32;
                frame = Some((be(3), be(1), s[5]));
            }
            0xC4 | 0xCC | 0xDA | 0xDC | 0xDD | 0xDF | 0xFE => {
                segment(&mut pos)?;
            }
            0xDB => {
                let mut s = segment(&mut pos)?;
                while let Some(&v) = s.first() {
                    let len = 1 + if v >> 4 == 0 { 64 } else { 128 };
                    s = s.get(len..)?;
                }
            }
            0xE0..=0xEF => {
                let s = segment(&mut pos)?;
                if marker == 0xE1 && s.starts_with(b"Exif\0\0") {
                    match &mut meta.exif {
                        Some(exif) => exif.extend_from_slice(&s[6..]),
                        None => meta.exif = Some(s.to_vec()),
                    }
                } else if marker == 0xE1 && s.starts_with(b"http://ns.adobe.com/xap/1.0/\0") {
                    meta.xmp = Some(s[29..].to_vec());
                } else if (marker == 0xE0 && s.starts_with(b"JFIF") || marker == 0xEE && s.starts_with(b"Adobe"))
                    && s.len() < 7
                {
                    return None; // Pillow reads a version field past the end
                }
            }
            0xC8 | 0xD0..=0xD9 | 0xF0..=0xFD => {}
            0xFF => {
                cur = Some(0xFF);
                continue;
            }
            0x00 => {}
            _ => return None, // "no marker found"
        }
        if marker == 0xDA {
            break;
        }
        cur = data.get(pos).copied();
        pos += 1;
    }
    let (width, height, components) = frame?;
    (width > 0 && height > 0).then_some(Header { width, height, components, meta })
}

/// Decoded samples as libjpeg outputs them: 1 channel (grey), 3 (RGB) or 4 (CMYK, Adobe-inverted as stored),
/// `width * height * channels` bytes.
pub(super) struct Decoded {
    pub width: u32,
    pub height: u32,
    pub channels: usize,
    pub pixels: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum JpegError {
    /// Pillow fails too: corrupt, truncated, or a frame other than the one its header walk found.
    Corrupt,
    /// libjpeg-turbo decodes it; this port does not.
    Unsupported(&'static str),
}

use JpegError::{Corrupt, Unsupported};

type R<T> = Result<T, JpegError>;

#[derive(Clone)]
struct Huffman {
    /// libjpeg's `maxcode` per code length 1..=17 (17 is the sentinel) and `valoffset` per length.
    maxcode: [i32; 18],
    valoffset: [i32; 17],
    /// Symbols padded with zeros to 256, as libjpeg's `huffval`.
    values: [u8; 256],
    /// `(length << 8) | symbol` for codes of at most 8 bits, else 0 (libjpeg's `lookup`).
    lookup: [u16; 256],
    /// `jpeg_make_d_derived_tbl` would accept it as an AC table, and as a DC table.
    valid: bool,
    valid_dc: bool,
}

impl Huffman {
    fn new(counts: &[u8; 16], symbols: &[u8]) -> Self {
        let mut h = Huffman {
            maxcode: [-1; 18],
            valoffset: [0; 17],
            values: [0; 256],
            lookup: [0; 256],
            valid: true,
            valid_dc: true,
        };
        h.values[..symbols.len()].copy_from_slice(symbols);
        let (mut code, mut k) = (0i32, 0usize);
        for len in 1..=16 {
            let n = counts[len - 1] as usize;
            if n > 0 {
                h.valoffset[len] = k as i32 - code;
                for _ in 0..n {
                    if len <= 8 {
                        let shift = 8 - len;
                        for fill in 0..1 << shift {
                            let at = ((code << shift) | fill) as usize;
                            if at < 256 {
                                h.lookup[at] = ((len as u16) << 8) | h.values[k] as u16;
                            }
                        }
                    }
                    code += 1;
                    k += 1;
                }
                h.maxcode[len] = code - 1;
                // Codes must fit their length.
                h.valid &= code < 1 << len;
            }
            code <<= 1;
        }
        h.maxcode[17] = 0xFFFFF;
        h.valid_dc = h.valid && symbols.iter().all(|&s| s <= 15);
        h
    }
}

/// libjpeg-turbo's bit reader (`jdhuff.c` with 64-bit buffers): refills to 57 bits when a read needs more than it
/// holds; a marker ends the data, after which reads see zeros and set `insufficient`; end of input is truncation.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    acc: u64,
    n: u32,
    /// libjpeg's `unread_marker`.
    marker: Option<u8>,
    insufficient: bool,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8], pos: usize) -> Self {
        Bits { data, pos, acc: 0, n: 0, marker: None, insufficient: false }
    }

    fn byte(&mut self) -> R<u8> {
        let b = *self.data.get(self.pos).ok_or(Corrupt)?;
        self.pos += 1;
        Ok(b)
    }

    /// `jpeg_fill_bit_buffer` for a read of `need` bits.
    fn fill(&mut self, need: u32) -> R<()> {
        while self.n < 57 {
            if self.marker.is_some() {
                break;
            }
            let mut c = self.byte()?;
            if c == 0xFF {
                // Fill bytes before a marker are skipped; FF00 is a stuffed data byte.
                loop {
                    c = self.byte()?;
                    if c != 0xFF {
                        break;
                    }
                }
                if c == 0 {
                    c = 0xFF;
                } else {
                    self.marker = Some(c);
                    break;
                }
            }
            self.acc |= (c as u64) << (56 - self.n);
            self.n += 8;
        }
        if need > self.n {
            self.insufficient = true;
            self.n = 57; // zeros
        }
        Ok(())
    }

    fn bits(&mut self, k: u32) -> R<u32> {
        if k == 0 {
            return Ok(0);
        }
        if self.n < k {
            self.fill(k)?;
        }
        let v = (self.acc >> (64 - k)) as u32;
        self.acc <<= k;
        self.n -= k;
        Ok(v)
    }

    fn bit(&mut self) -> R<bool> {
        Ok(self.bits(1)? == 1)
    }

    /// `HUFF_DECODE`: an 8-bit lookahead, else `jpeg_huff_decode` bit by bit; a code longer than 16 bits decodes
    /// as symbol 0, libjpeg's "safest result".
    fn decode(&mut self, h: &Huffman) -> R<u8> {
        let mut len = 1;
        if self.n < 8 {
            self.fill(0)?;
        }
        if self.n >= 8 {
            let entry = h.lookup[(self.acc >> 56) as usize];
            if entry != 0 {
                let l = (entry >> 8) as u32;
                self.acc <<= l;
                self.n -= l;
                return Ok(entry as u8);
            }
            len = 9;
        }
        let mut code = self.bits(len as u32)? as i32;
        while code > h.maxcode[len] {
            code = (code << 1) | self.bits(1)? as i32;
            len += 1;
        }
        if len > 16 {
            return Ok(0);
        }
        Ok(h.values[((code + h.valoffset[len]) & 0xFF) as usize])
    }

    /// `HUFF_EXTEND` of `s` raw bits.
    fn receive_extend(&mut self, s: u8) -> R<i32> {
        if s == 0 {
            return Ok(0);
        }
        let s = s.min(16);
        let v = self.bits(s as u32)? as i32;
        Ok(if v < 1 << (s - 1) { v - (1 << s) + 1 } else { v })
    }

    /// libjpeg's `next_marker`: skip to an FF, past fill FFs; FF00 is data and skipped too.
    fn next_marker(&mut self) -> R<u8> {
        loop {
            let mut c = self.byte()?;
            while c != 0xFF {
                c = self.byte()?;
            }
            loop {
                c = self.byte()?;
                if c != 0xFF {
                    break;
                }
            }
            if c != 0 {
                return Ok(c);
            }
        }
    }

    /// `process_restart`: drop buffered bits, then `read_restart_marker` and `jpeg_resync_to_restart`.
    fn restart(&mut self, expected: &mut u8) -> R<()> {
        self.acc = 0;
        self.n = 0;
        let mut marker = match self.marker {
            Some(m) => m,
            None => self.next_marker()?,
        };
        self.marker = Some(marker);
        if marker == 0xD0 + *expected {
            self.marker = None;
        } else {
            let rst = |k: u8| 0xD0 + (k & 7);
            loop {
                let action = if marker < 0xC0 {
                    2
                } else if !(0xD0..=0xD7).contains(&marker)
                    || marker == rst(*expected + 1)
                    || marker == rst(*expected + 2)
                {
                    3
                } else if marker == rst(expected.wrapping_sub(1)) || marker == rst(expected.wrapping_sub(2)) {
                    2
                } else {
                    1
                };
                match action {
                    1 => {
                        self.marker = None;
                        break;
                    }
                    2 => {
                        marker = self.next_marker()?;
                        self.marker = Some(marker);
                    }
                    _ => break, // leave the marker unread: this segment is empty
                }
            }
        }
        *expected = (*expected + 1) & 7;
        if self.marker.is_none() {
            self.insufficient = false;
        }
        Ok(())
    }
}

struct Component {
    id: u8,
    h: usize,
    v: usize,
    tq: usize,
    /// Blocks per row and rows of blocks, padded to whole MCUs.
    bw: usize,
    bh: usize,
    coefs: Vec<[i16; 64]>,
    /// Quantization table latched at the component's first scan, as libjpeg does.
    quant: Option<[u16; 64]>,
    dc_pred: i32,
    /// Successive-approximation state per coefficient: -1 never sent, else the last scan's `Al`.
    coef_bits: [i32; 64],
}

/// Zigzag to natural order, with libjpeg's 16 extra entries so corrupt runs past 63 land on coefficient 63.
const ZIGZAG: [usize; 80] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20, 13, 6, 7, 14, 21,
    28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61,
    54, 47, 55, 62, 63, 63, 63, 63, 63, 63, 63, 63, 63, 63, 63, 63, 63, 63, 63, 63, 63,
];

struct Scan {
    comps: Vec<(usize, usize, usize)>, // component index, DC table, AC table
    ss: usize,
    se: usize,
    ah: u32,
    al: u32,
}

fn be16(data: &[u8], at: usize) -> R<usize> {
    Ok(u16::from_be_bytes([*data.get(at).ok_or(Corrupt)?, *data.get(at + 1).ok_or(Corrupt)?]) as usize)
}

/// libjpeg-turbo's marker reader and decoder over the whole stream. The frame must be the one Pillow's header walk
/// found (`header`), whose size the caller has already held to the pixel limit; nothing is allocated before that.
pub(super) fn decode(data: &[u8], header: &Header) -> R<Decoded> {
    if data.get(..2) != Some(&[0xFF, 0xD8]) {
        return Err(Corrupt);
    }
    let mut reader = Bits::new(data, 2);
    let mut quant: [Option<[u16; 64]>; 4] = [None; 4];
    let mut dc: [Option<Huffman>; 4] = Default::default();
    let mut ac: [Option<Huffman>; 4] = Default::default();
    let mut comps: Vec<Component> = Vec::new();
    let (mut width, mut height, mut progressive) = (0usize, 0usize, false);
    let (mut hmax, mut vmax, mut mcus_x, mut mcus_y) = (1, 1, 0, 0);
    let (mut jfif, mut adobe) = (false, None::<u8>);
    let mut restart_interval = 0usize;
    let mut scans = 0;
    loop {
        let marker = match reader.marker.take() {
            Some(m) => m,
            None => reader.next_marker()?,
        };
        if marker == 0xD9 {
            break;
        }
        if (0xD0..=0xD7).contains(&marker) || marker == 0x01 {
            continue;
        }
        let pos = reader.pos;
        let len = be16(data, pos)?;
        let seg = data.get(pos + 2..pos + len.max(2)).ok_or(Corrupt)?;
        reader.pos = pos + len.max(2);
        match marker {
            0xE0 if len >= 16 && seg.starts_with(b"JFIF\0") => jfif = true,
            0xEE if len >= 14 && seg.starts_with(b"Adobe") => adobe = Some(seg[11]),
            0xE0..=0xEF | 0xFE | 0xDC | 0xCC => {}
            0xDB => {
                let mut i = 0;
                while i < seg.len() {
                    let (pq, tq) = (seg[i] >> 4, (seg[i] & 15) as usize);
                    i += 1;
                    let mut table = [0u16; 64];
                    for &z in &ZIGZAG[..64] {
                        table[z] = if pq == 0 { *seg.get(i).ok_or(Corrupt)? as u16 } else { be16(seg, i)? as u16 };
                        i += 1 + pq.min(1) as usize;
                    }
                    *quant.get_mut(tq).ok_or(Corrupt)? = Some(table);
                }
            }
            0xC4 => {
                let mut i = 0;
                while i < seg.len() {
                    let (tc, th) = (seg[i] >> 4, (seg[i] & 15) as usize);
                    let counts: [u8; 16] = seg.get(i + 1..i + 17).ok_or(Corrupt)?.try_into().unwrap();
                    let total: usize = counts.iter().map(|&c| c as usize).sum();
                    if total > 256 || tc > 1 || th > 3 {
                        return Err(Corrupt);
                    }
                    let symbols = seg.get(i + 17..i + 17 + total).ok_or(Corrupt)?;
                    (if tc == 0 { &mut dc } else { &mut ac })[th] = Some(Huffman::new(&counts, symbols));
                    i += 17 + total;
                }
            }
            0xDD => {
                if len != 4 {
                    return Err(Corrupt);
                }
                restart_interval = be16(seg, 0)?;
            }
            0xC0..=0xC2 => {
                progressive = marker == 0xC2;
                if !comps.is_empty() || seg.len() < 6 || seg[0] != 8 {
                    return Err(Corrupt);
                }
                height = be16(seg, 1)?;
                width = be16(seg, 3)?;
                let n = seg[5] as usize;
                // The frame Pillow sized and the one decoded must agree: the pixel limit was checked on Pillow's.
                if (width as u32, height as u32, n as u8) != (header.width, header.height, header.components)
                    || seg.len() != 6 + 3 * n
                {
                    return Err(Corrupt);
                }
                for c in 0..n {
                    let s = &seg[6 + c * 3..9 + c * 3];
                    let (h, v) = ((s[1] >> 4) as usize, (s[1] & 15) as usize);
                    if !(1..=4).contains(&h) || !(1..=4).contains(&v) || s[2] > 3 {
                        return Err(Corrupt);
                    }
                    comps.push(Component {
                        id: s[0],
                        h,
                        v,
                        tq: s[2] as usize,
                        bw: 0,
                        bh: 0,
                        coefs: Vec::new(),
                        quant: None,
                        dc_pred: 0,
                        coef_bits: [-1; 64],
                    });
                }
                hmax = comps.iter().map(|c| c.h).max().unwrap();
                vmax = comps.iter().map(|c| c.v).max().unwrap();
                if comps.iter().any(|c| !hmax.is_multiple_of(c.h) || !vmax.is_multiple_of(c.v)) {
                    return Err(Unsupported("JPEG chroma subsampling with a non-integral ratio is not supported"));
                }
                mcus_x = width.div_ceil(8 * hmax);
                mcus_y = height.div_ceil(8 * vmax);
                for c in &mut comps {
                    c.bw = mcus_x * c.h;
                    c.bh = mcus_y * c.v;
                    c.coefs = vec![[0; 64]; c.bw * c.bh];
                }
            }
            0xC3 | 0xC9..=0xCB => {
                return Err(Unsupported("arithmetic-coded and lossless JPEGs are not supported"));
            }
            0xDA => {
                if comps.is_empty() {
                    return Err(Corrupt);
                }
                let n = *seg.first().ok_or(Corrupt)? as usize;
                if !(1..=4).contains(&n) || seg.len() != 4 + 2 * n {
                    return Err(Corrupt);
                }
                let mut scan_comps: Vec<(usize, usize, usize)> = Vec::new();
                for i in 0..n {
                    let s = &seg[1 + i * 2..3 + i * 2];
                    let ci = comps.iter().position(|c| c.id == s[0]).ok_or(Corrupt)?;
                    if scan_comps.iter().any(|&(c, _, _)| c == ci) {
                        return Err(Corrupt);
                    }
                    scan_comps.push((ci, (s[1] >> 4) as usize, (s[1] & 15) as usize));
                }
                let tail = &seg[1 + n * 2..];
                let scan = Scan {
                    comps: scan_comps,
                    ss: tail[0] as usize,
                    se: tail[1] as usize,
                    ah: (tail[2] >> 4) as u32,
                    al: (tail[2] & 15) as u32,
                };
                let blocks_in_mcu: usize = scan.comps.iter().map(|&(ci, _, _)| comps[ci].h * comps[ci].v).sum();
                if n > 1 && blocks_in_mcu > 10 {
                    return Err(Corrupt);
                }
                let table = |tables: &[Option<Huffman>; 4], t: usize, is_dc: bool| -> R<()> {
                    match tables.get(t).and_then(Option::as_ref) {
                        Some(h) if if is_dc { h.valid_dc } else { h.valid } => Ok(()),
                        _ => Err(Corrupt),
                    }
                };
                if progressive {
                    let dc_band = scan.ss == 0;
                    let bad = (dc_band && scan.se != 0)
                        || (!dc_band && (scan.ss > scan.se || scan.se > 63 || n != 1))
                        || (scan.ah != 0 && scan.al + 1 != scan.ah)
                        || scan.al > 13;
                    if bad {
                        return Err(Corrupt);
                    }
                    for &(_, td, ta) in &scan.comps {
                        if dc_band && scan.ah == 0 {
                            table(&dc, td, true)?;
                        } else if !dc_band {
                            table(&ac, ta, false)?;
                        }
                    }
                } else {
                    for &(_, td, ta) in &scan.comps {
                        table(&dc, td, true)?;
                        table(&ac, ta, false)?;
                    }
                }
                let (ss, se) = if progressive { (scan.ss, scan.se) } else { (0, 63) };
                for &(ci, _, _) in &scan.comps {
                    let c = &mut comps[ci];
                    if c.quant.is_none() {
                        c.quant = Some(quant[c.tq].ok_or(Corrupt)?);
                    }
                    c.dc_pred = 0;
                    for k in ss..=se {
                        c.coef_bits[k] = scan.al as i32;
                    }
                }
                let mut bits = Bits::new(data, reader.pos);
                let tables = (&dc, &ac);
                decode_scan(&mut bits, &scan, &mut comps, tables, restart_interval, (mcus_x, mcus_y), (width, height, hmax, vmax), progressive)?;
                scans += 1;
                // A single scan holding every component is decoded as it streams: libjpeg reads nothing after it.
                if scans == 1 && !progressive && n == comps.len() {
                    break;
                }
                reader = Bits { marker: bits.marker, ..Bits::new(data, bits.pos) };
            }
            // Hierarchical frames and markers libjpeg does not know are errors there.
            _ => return Err(Corrupt),
        }
    }
    if scans == 0 {
        return Err(Corrupt);
    }
    if progressive && smoothing_applies(&comps) {
        return Err(Unsupported("progressive JPEGs whose scans stop before full precision are not supported"));
    }
    finish(comps, (width, height, hmax, vmax), jfif, adobe)
}

/// libjpeg-turbo's `smoothing_ok`: block smoothing runs when every component has a quantization table with nonzero
/// low-frequency steps and a DC scan, and some component's first nine AC coefficients lack full precision.
fn smoothing_applies(comps: &[Component]) -> bool {
    let mut useful = false;
    for c in comps {
        let Some(q) = c.quant else { return false };
        if [0, 1, 8, 16, 9, 2, 3, 10, 17, 24].iter().any(|&i| q[i] == 0) || c.coef_bits[0] < 0 {
            return false;
        }
        useful |= c.coef_bits[1..10].iter().any(|&b| b != 0);
    }
    useful
}

fn finish(
    mut comps: Vec<Component>,
    (width, height, hmax, vmax): (usize, usize, usize, usize),
    jfif: bool,
    adobe: Option<u8>,
) -> R<Decoded> {
    let n = comps.len();
    let mut full = Vec::with_capacity(n);
    for c in &mut comps {
        let plane = idct_component(c);
        c.coefs = Vec::new(); // a 32 MP photo holds ~200 MB of coefficients; free each as soon as it is spent
        full.push(upsample(c, &plane, width, height, hmax, vmax)?);
    }
    let (w, h) = (width as u32, height as u32);
    if n == 1 {
        return Ok(Decoded { width: w, height: h, channels: 1, pixels: full.remove(0) });
    }
    if n == 4 {
        // libjpeg reads four components as YCCK under an Adobe marker with transform 2 (or any but 0), else CMYK.
        let ycck = adobe.is_some_and(|t| t != 0);
        let mut pixels = vec![0u8; width * height * 4];
        if ycck {
            let mut rgb = vec![0u8; width * height * 3];
            ycc_to_rgb(&full[0], &full[1], &full[2], &mut rgb);
            for (i, px) in pixels.chunks_exact_mut(4).enumerate() {
                px.copy_from_slice(&[255 - rgb[i * 3], 255 - rgb[i * 3 + 1], 255 - rgb[i * 3 + 2], full[3][i]]);
            }
        } else {
            for (i, px) in pixels.chunks_exact_mut(4).enumerate() {
                px.copy_from_slice(&[full[0][i], full[1][i], full[2][i], full[3][i]]);
            }
        }
        return Ok(Decoded { width: w, height: h, channels: 4, pixels });
    }
    let rgb_ids = comps[0].id == b'R' && comps[1].id == b'G' && comps[2].id == b'B';
    let is_rgb = !jfif
        && match adobe {
            Some(transform) => transform == 0,
            None => rgb_ids,
        };
    let mut pixels = vec![0u8; width * height * 3];
    if is_rgb {
        for (i, px) in pixels.chunks_exact_mut(3).enumerate() {
            px.copy_from_slice(&[full[0][i], full[1][i], full[2][i]]);
        }
    } else {
        ycc_to_rgb(&full[0], &full[1], &full[2], &mut pixels);
    }
    Ok(Decoded { width: w, height: h, channels: 3, pixels })
}

type Tables<'t> = (&'t [Option<Huffman>; 4], &'t [Option<Huffman>; 4]);

#[allow(clippy::too_many_arguments)]
fn decode_scan(
    bits: &mut Bits,
    scan: &Scan,
    comps: &mut [Component],
    (dc, ac): Tables,
    restart_interval: usize,
    (mcus_x, mcus_y): (usize, usize),
    (width, height, hmax, vmax): (usize, usize, usize, usize),
    progressive: bool,
) -> R<()> {
    let mut eobrun = 0u32;
    let mut expected_rst = 0u8;
    // `(component index, block x, block y)` of each block of an MCU, in coding order.
    let mut units: Vec<(usize, usize, usize)> = Vec::new();
    let (total, single) = if scan.comps.len() == 1 {
        // A non-interleaved scan codes only the blocks that hold image samples.
        let c = &comps[scan.comps[0].0];
        let bw = (width * c.h).div_ceil(hmax).div_ceil(8);
        let bh = (height * c.v).div_ceil(vmax).div_ceil(8);
        (bw * bh, Some(bw))
    } else {
        (mcus_x * mcus_y, None)
    };
    let ac_first_scan = progressive && scan.ss > 0 && scan.ah == 0;
    let mut m = 0;
    while m < total {
        if restart_interval > 0 && m > 0 && m % restart_interval == 0 {
            bits.restart(&mut expected_rst)?;
            eobrun = 0;
            for &(ci, _, _) in &scan.comps {
                comps[ci].dc_pred = 0;
            }
        }
        // An EOB run leaves whole blocks of a first AC scan untouched: step over it to the next restart at most,
        // rather than block by block (a file can repeat such scans a thousand times over a 32 MP frame).
        if ac_first_scan && eobrun > 0 && !bits.insufficient {
            let to_restart = if restart_interval > 0 { restart_interval - m % restart_interval } else { total - m };
            let skip = (eobrun as usize).min(to_restart).min(total - m);
            eobrun -= skip as u32;
            m += skip;
            continue;
        }
        units.clear();
        match single {
            Some(bw) => units.push((scan.comps[0].0, m % bw, m / bw)),
            None => {
                let (mx, my) = (m % mcus_x, m / mcus_x);
                for &(ci, _, _) in &scan.comps {
                    let (h, v) = (comps[ci].h, comps[ci].v);
                    for y in 0..v {
                        for x in 0..h {
                            units.push((ci, mx * h + x, my * v + y));
                        }
                    }
                }
            }
        }
        // Out of data: libjpeg leaves the rest of the segment's MCUs untouched (zero, or earlier scans' values).
        // DC refinement reads on regardless; zeros change nothing.
        let refine_dc = progressive && scan.ss == 0 && scan.ah != 0;
        if bits.insufficient && !refine_dc {
            m += 1;
            continue;
        }
        for &(ci, bx, by) in &units {
            let &(_, td, ta) = scan.comps.iter().find(|s| s.0 == ci).unwrap();
            let c = &mut comps[ci];
            let block = &mut c.coefs[by * c.bw + bx];
            if !progressive {
                let s = bits.decode(dc[td].as_ref().unwrap())?;
                c.dc_pred = c.dc_pred.wrapping_add(bits.receive_extend(s)?);
                block[0] = c.dc_pred as i16;
                let table = ac[ta].as_ref().unwrap();
                let mut k = 1;
                while k < 64 {
                    let rs = bits.decode(table)?;
                    let (r, s) = ((rs >> 4) as usize, rs & 15);
                    if s != 0 {
                        k += r;
                        block[ZIGZAG[k]] = bits.receive_extend(s)? as i16;
                    } else if r == 15 {
                        k += 15;
                    } else {
                        break;
                    }
                    k += 1;
                }
            } else if scan.ss == 0 {
                if scan.ah == 0 {
                    let s = bits.decode(dc[td].as_ref().unwrap())?;
                    c.dc_pred = c.dc_pred.checked_add(bits.receive_extend(s)?).ok_or(Corrupt)?;
                    block[0] = c.dc_pred.wrapping_shl(scan.al) as i16;
                } else if bits.bit()? {
                    block[0] |= 1i16.wrapping_shl(scan.al);
                }
            } else if scan.ah == 0 {
                ac_first(bits, ac[ta].as_ref().unwrap(), block, scan, &mut eobrun)?;
            } else {
                ac_refine(bits, ac[ta].as_ref().unwrap(), block, scan, &mut eobrun)?;
            }
        }
        m += 1;
    }
    Ok(())
}

/// `decode_mcu_AC_first`.
fn ac_first(bits: &mut Bits, table: &Huffman, block: &mut [i16; 64], scan: &Scan, eobrun: &mut u32) -> R<()> {
    if *eobrun > 0 {
        *eobrun -= 1;
        return Ok(());
    }
    let mut k = scan.ss;
    while k <= scan.se {
        let rs = bits.decode(table)?;
        let (r, s) = ((rs >> 4) as u32, rs & 15);
        if s != 0 {
            k += r as usize;
            block[ZIGZAG[k]] = bits.receive_extend(s)?.wrapping_shl(scan.al) as i16;
        } else if r == 15 {
            k += 15;
        } else {
            *eobrun = (1 << r) + bits.bits(r)? - 1;
            break;
        }
        k += 1;
    }
    Ok(())
}

/// `decode_mcu_AC_refine`: correction bits for nonzero coefficients, new ±1 coefficients in the gaps.
fn ac_refine(bits: &mut Bits, table: &Huffman, block: &mut [i16; 64], scan: &Scan, eobrun: &mut u32) -> R<()> {
    let p1 = 1i16 << scan.al;
    let m1 = -p1;
    let mut k = scan.ss;
    let refine = |bits: &mut Bits, coef: &mut i16| -> R<()> {
        if bits.bit()? && (*coef & p1) == 0 {
            *coef = coef.wrapping_add(if *coef >= 0 { p1 } else { m1 });
        }
        Ok(())
    };
    if *eobrun == 0 {
        while k <= scan.se {
            let rs = bits.decode(table)?;
            let (mut r, s) = ((rs >> 4) as i32, rs & 15);
            let mut value = 0i16;
            if s != 0 {
                value = if bits.bit()? { p1 } else { m1 };
            } else if r != 15 {
                *eobrun = (1 << r) + bits.bits(r as u32)?;
                break;
            }
            loop {
                let coef = &mut block[ZIGZAG[k]];
                if *coef != 0 {
                    refine(bits, coef)?;
                } else {
                    r -= 1;
                    if r < 0 {
                        break;
                    }
                }
                k += 1;
                if k > scan.se {
                    break;
                }
            }
            if value != 0 {
                block[ZIGZAG[k]] = value;
            }
            k += 1;
        }
    }
    if *eobrun > 0 {
        while k <= scan.se {
            let coef = &mut block[ZIGZAG[k]];
            if *coef != 0 {
                refine(bits, coef)?;
            }
            k += 1;
        }
        *eobrun -= 1;
    }
    Ok(())
}

/// libjpeg-turbo's `jsimd_idct_islow_neon`, the IDCT Pillow runs on Apple silicon, as scalar code. In exact
/// arithmetic it equals `jpeg_idct_islow`; it differs on blocks whose coefficients overflow its lanes (only damaged
/// data gets there): dequantization and three partial sums wrap at 16 bits, products and sums at 32, the first pass
/// keeps 16 bits, and the second saturates to 8.
fn idct_islow(coef: &[i16; 64], q: &[u16; 64], out: &mut [u8], stride: usize) {
    let mut ws = [[0i16; 8]; 8]; // [row][column]
    for col in 0..8 {
        let deq = |r: usize| coef[r * 8 + col].wrapping_mul(q[r * 8 + col] as i16);
        let v = butterfly([deq(0), deq(1), deq(2), deq(3), deq(4), deq(5), deq(6), deq(7)]);
        for (r, x) in v.into_iter().enumerate() {
            // vrshrn_n_s32(x, 11): round, shift, keep the low 16 bits.
            ws[r][col] = ((x as i64 + (1 << 10)) >> 11) as i16;
        }
    }
    for (row, w) in ws.iter().enumerate() {
        let v = butterfly(*w);
        for (c, x) in v.into_iter().enumerate() {
            // vaddhn (high half), then vqrshrn_n_s16(·, 2) saturating to i8, plus 128.
            let high = (x >> 16) as i16 as i32;
            out[row * stride + c] = (((high + 2) >> 2).clamp(-128, 127) + 128) as u8;
        }
    }
}

/// One NEON ISLOW pass over 8 inputs, outputs 0..8 before descaling, with the lane widths of
/// `jidctint-neon.c` (constant pairs folded as it folds them).
fn butterfly(i: [i16; 8]) -> [i32; 8] {
    const F_0_298: i32 = 2446;
    const F_0_390: i32 = 3196;
    const F_0_541: i32 = 4433;
    const F_0_765: i32 = 6270;
    const F_0_899: i32 = 7373;
    const F_1_175: i32 = 9633;
    const F_1_501: i32 = 12299;
    const F_1_847: i32 = 15137;
    const F_1_961: i32 = 16069;
    const F_2_053: i32 = 16819;
    const F_2_562: i32 = 20995;
    const F_3_072: i32 = 25172;
    let m = |a: i16, k: i32| (a as i32).wrapping_mul(k);
    let (z2, z3) = (i[2], i[6]);
    let tmp2 = m(z2, F_0_541).wrapping_add(m(z3, F_0_541 - F_1_847));
    let tmp3 = m(z2, F_0_541 + F_0_765).wrapping_add(m(z3, F_0_541));
    let tmp0 = (i[0].wrapping_add(i[4]) as i32) << 13;
    let tmp1 = (i[0].wrapping_sub(i[4]) as i32) << 13;
    let (tmp10, tmp13) = (tmp0.wrapping_add(tmp3), tmp0.wrapping_sub(tmp3));
    let (tmp11, tmp12) = (tmp1.wrapping_add(tmp2), tmp1.wrapping_sub(tmp2));
    let (t0, t1, t2, t3) = (i[7], i[5], i[3], i[1]);
    let (z3s, z4s) = (t0.wrapping_add(t2), t1.wrapping_add(t3));
    let z3 = m(z3s, F_1_175 - F_1_961).wrapping_add(m(z4s, F_1_175));
    let z4 = m(z3s, F_1_175).wrapping_add(m(z4s, F_1_175 - F_0_390));
    let o0 = m(t0, F_0_298 - F_0_899).wrapping_sub(m(t3, F_0_899)).wrapping_add(z3);
    let o1 = m(t1, F_2_053 - F_2_562).wrapping_sub(m(t2, F_2_562)).wrapping_add(z4);
    let o2 = m(t2, F_3_072 - F_2_562).wrapping_sub(m(t1, F_2_562)).wrapping_add(z3);
    let o3 = m(t3, F_1_501 - F_0_899).wrapping_sub(m(t0, F_0_899)).wrapping_add(z4);
    [
        tmp10.wrapping_add(o3),
        tmp11.wrapping_add(o2),
        tmp12.wrapping_add(o1),
        tmp13.wrapping_add(o0),
        tmp13.wrapping_sub(o0),
        tmp12.wrapping_sub(o1),
        tmp11.wrapping_sub(o2),
        tmp10.wrapping_sub(o3),
    ]
}

/// The component's samples over its padded block area, `bw * 8` wide.
fn idct_component(c: &Component) -> Vec<u8> {
    let stride = c.bw * 8;
    let mut plane = vec![0u8; stride * c.bh * 8];
    let q = c.quant.unwrap_or([0; 64]);
    for by in 0..c.bh {
        for bx in 0..c.bw {
            idct_islow(&c.coefs[by * c.bw + bx], &q, &mut plane[by * 8 * stride + bx * 8..], stride);
        }
    }
    plane
}

/// `jdsample.c` for one component: its `downsampled` samples to the full `width x height`.
fn upsample(c: &Component, plane: &[u8], width: usize, height: usize, hmax: usize, vmax: usize) -> R<Vec<u8>> {
    let stride = c.bw * 8;
    let dw = (width * c.h).div_ceil(hmax);
    let dh = (height * c.v).div_ceil(vmax);
    let at = |x: usize, y: usize| plane[y * stride + x] as i32;
    // Rows beyond either edge repeat the edge row (jdmainct's context pointers).
    let row = |y: isize| y.clamp(0, dh as isize - 1) as usize;
    let mut out = vec![0u8; width * height];
    let (hx, vx) = (hmax / c.h, vmax / c.v);
    if !hmax.is_multiple_of(c.h) || !vmax.is_multiple_of(c.v) {
        return Err(Unsupported("JPEG chroma subsampling with a non-integral ratio is not supported"));
    }
    match (hx, vx) {
        (1, 1) => {
            for y in 0..height {
                out[y * width..][..width].copy_from_slice(&plane[y * stride..][..width]);
            }
        }
        (2, 1) if dw > 2 => {
            for y in 0..height {
                let mut line = Vec::with_capacity(dw * 2);
                h2v1_line(|x| at(x, y), dw, &mut line);
                out[y * width..][..width].copy_from_slice(&line[..width]);
            }
        }
        (1, 2) => {
            for y in 0..height {
                let (near, bias, far) = (y / 2, if y % 2 == 0 { 1 } else { 2 }, if y % 2 == 0 { -1 } else { 1 });
                let other = row(near as isize + far);
                for x in 0..width {
                    out[y * width + x] = ((at(x, near) * 3 + at(x, other) + bias) >> 2) as u8;
                }
            }
        }
        (2, 2) if dw > 2 => {
            for y in 0..height {
                let near = y / 2;
                let other = row(near as isize + if y % 2 == 0 { -1 } else { 1 });
                let sum = |x: usize| at(x, near) * 3 + at(x, other);
                let dst = &mut out[y * width..][..width];
                for (ox, d) in dst.iter_mut().enumerate() {
                    let x = ox / 2;
                    let this = sum(x);
                    *d = (if ox % 2 == 0 {
                        if x == 0 { this * 4 + 8 } else { this * 3 + sum(x - 1) + 8 }
                    } else if x == dw - 1 {
                        this * 4 + 7
                    } else {
                        this * 3 + sum(x + 1) + 7
                    } >> 4) as u8;
                }
            }
        }
        _ => {
            // Plain replication: `int_upsample`, and the 2x cases on images at most 2 samples wide.
            for y in 0..height {
                for x in 0..width {
                    out[y * width + x] = at(x / hx, y / vx) as u8;
                }
            }
        }
    }
    Ok(out)
}

/// `h2v1_fancy_upsample` for one row of `dw > 2` samples.
fn h2v1_line(s: impl Fn(usize) -> i32, dw: usize, out: &mut Vec<u8>) {
    out.push(s(0) as u8);
    out.push(((s(0) * 3 + s(1) + 2) >> 2) as u8);
    for x in 1..dw - 1 {
        let v = s(x) * 3;
        out.push(((v + s(x - 1) + 1) >> 2) as u8);
        out.push(((v + s(x + 1) + 2) >> 2) as u8);
    }
    out.push(((s(dw - 1) * 3 + s(dw - 2) + 1) >> 2) as u8);
    out.push(s(dw - 1) as u8);
}

/// `ycc_rgb_convert` with `build_ycc_rgb_table`'s 16-bit fixed point.
fn ycc_to_rgb(y: &[u8], cb: &[u8], cr: &[u8], out: &mut [u8]) {
    const ONE_HALF: i64 = 1 << 15;
    let fix = |x: f64| (x * 65536.0 + 0.5) as i64;
    let mut cr_r = [0i32; 256];
    let mut cb_b = [0i32; 256];
    let mut cr_g = [0i64; 256];
    let mut cb_g = [0i64; 256];
    for i in 0..256 {
        let x = i as i64 - 128;
        cr_r[i] = ((fix(1.40200) * x + ONE_HALF) >> 16) as i32;
        cb_b[i] = ((fix(1.77200) * x + ONE_HALF) >> 16) as i32;
        cr_g[i] = -fix(0.71414) * x;
        cb_g[i] = -fix(0.34414) * x + ONE_HALF;
    }
    let clamp = |v: i32| v.clamp(0, 255) as u8;
    for (i, px) in out.chunks_exact_mut(3).enumerate() {
        let (yy, b, r) = (y[i] as i32, cb[i] as usize, cr[i] as usize);
        px[0] = clamp(yy + cr_r[r]);
        px[1] = clamp(yy + ((cb_g[b] + cr_g[r]) >> 16) as i32);
        px[2] = clamp(yy + cb_b[b]);
    }
}
