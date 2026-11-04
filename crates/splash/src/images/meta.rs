//! What Pillow reads besides pixels: the orientation `ImageOps.exif_transpose` acts on (EXIF, else XMP), and the
//! PNG transparency key it applies its own way.
//!
//! Pillow takes EXIF from JPEG APP1 (the first block: later ones are appended after it), PNG `eXIf` chunks before or
//! after the image data (or ImageMagick's hex "Raw profile type exif" text), and the WebP `EXIF` chunk; when EXIF
//! has no Orientation tag at all, `tiff:Orientation` in XMP. These follow Pillow 13's `Image.getexif` and
//! `TiffImagePlugin.ImageFileDirectory_v2.load`.

/// EXIF and XMP as Pillow's `info` holds them for one image.
#[derive(Default)]
pub(super) struct Metadata {
    pub exif: Option<Vec<u8>>,
    pub xmp: Option<Vec<u8>>,
}

/// The EXIF orientation (1 to 8) Pillow transposes by; 1 when there is none or it is unusable.
pub(super) fn orientation(meta: &Metadata) -> u8 {
    match meta.exif.as_deref().map(exif_orientation) {
        // Pillow's EXIF parser raised; Python's `except Exception: pass` keeps the stored orientation.
        Some(Err(())) => 1,
        Some(Ok(Tag::Number(v))) => transform(v),
        Some(Ok(Tag::Other)) => 1,
        Some(Ok(Tag::Absent)) | None => meta.xmp.as_deref().map_or(1, xmp_orientation),
    }
}

fn transform(v: f64) -> u8 {
    if (2.0..=8.0).contains(&v) && v.fract() == 0.0 { v as u8 } else { 1 }
}

/// `re.search(rb'tiff:Orientation(="|>)([0-9])', xmp)`.
fn xmp_orientation(xmp: &[u8]) -> u8 {
    const KEY: &[u8] = b"tiff:Orientation";
    let mut from = 0;
    while let Some(i) = find(&xmp[from..], KEY) {
        let at = from + i + KEY.len();
        let rest = &xmp[at..];
        let digit = match rest {
            [b'=', b'"', d, ..] | [b'>', d, ..] => Some(*d),
            _ => None,
        };
        if let Some(d @ b'0'..=b'9') = digit {
            return transform((d - b'0') as f64);
        }
        from = at;
    }
    1
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

enum Tag {
    Absent,
    /// A single numeric value (ints, floats, rationals compare equal to ints in Python's dict lookup).
    Number(f64),
    /// Present but a string or bytes: no transposition.
    Other,
}

/// IFD0's Orientation as Pillow reads it. `Err` where Pillow raises (not a TIFF header).
fn exif_orientation(data: &[u8]) -> Result<Tag, ()> {
    let mut data = data;
    for _ in 0..2 {
        data = data.strip_prefix(b"Exif\0\0").unwrap_or(data);
    }
    if data.is_empty() {
        return Ok(Tag::Absent);
    }
    let head = data.get(..8).ok_or(())?;
    let big = match &head[..4] {
        // Pillow also accepts the two byte-swapped magics some writers emit.
        b"MM\0*" | b"II*\0" | b"MM*\0" | b"II\0*" => false,
        b"MM\0+" | b"II+\0" => true,
        _ => return Err(()),
    };
    if big {
        // Pillow unpacks a BigTIFF IFD offset from bytes 8..16 of an 8-byte header and raises.
        return Err(());
    }
    let be = head[0] == b'M';
    let u16_at = |at: usize| data.get(at..at + 2).map(|b| if be { u16::from_be_bytes([b[0], b[1]]) } else { u16::from_le_bytes([b[0], b[1]]) });
    let u32_at = |at: usize| {
        data.get(at..at + 4).map(|b| {
            let b = [b[0], b[1], b[2], b[3]];
            if be { u32::from_be_bytes(b) } else { u32::from_le_bytes(b) }
        })
    };
    let ifd = u32_at(4).unwrap() as usize;
    // A short read is a warning in Pillow: the tags read so far stand.
    let Some(count) = u16_at(ifd) else { return Ok(Tag::Absent) };
    let mut total_size = 0usize;
    for i in 0..count as usize {
        let at = ifd + 2 + i * 12;
        // Pillow reads each 12-byte entry whole; a short one ends the directory.
        if data.len() < at + 12 {
            return Ok(Tag::Absent);
        }
        let (tag, typ, n) = (u16_at(at).unwrap(), u16_at(at + 2).unwrap(), u32_at(at + 4).unwrap());
        // Pillow's loaders: unit size per TIFF type; others are skipped.
        let unit = match typ {
            1 | 2 | 6 | 7 => 1,
            3 | 8 => 2,
            4 | 9 | 11 | 13 => 4,
            5 | 10 | 12 | 16 | 17 | 18 => 8,
            _ => continue,
        };
        let size = n as usize * unit;
        let value = if size > 4 {
            let offset = u32_at(at + 8).unwrap() as usize;
            total_size += size;
            if total_size > data.len() {
                continue;
            }
            match data.get(offset..offset + size) {
                Some(v) => v,
                None => continue,
            }
        } else {
            &data[at + 8..at + 8 + size]
        };
        if value.is_empty() || tag != 0x0112 {
            continue;
        }
        // Pillow warns about extra values of a one-value tag and keeps the first.
        let value = &value[..unit];
        let int = |b: &[u8]| -> f64 {
            let mut v = 0u64;
            for k in 0..b.len() {
                let byte = if be { b[k] } else { b[b.len() - 1 - k] };
                v = (v << 8) | byte as u64;
            }
            v as f64
        };
        let signed = |b: &[u8]| -> f64 {
            let bits = b.len() * 8;
            let v = int(b) as u64;
            ((v << (64 - bits)) as i64 >> (64 - bits)) as f64
        };
        let number = match typ {
            3 | 4 | 13 | 16 | 18 => int(value),
            8 | 9 | 17 => signed(value),
            11 => f32::from_bits(int(value) as u32) as f64,
            12 => f64::from_bits(int(value) as u64),
            5 | 10 => {
                let (num, den) = if typ == 5 {
                    (int(&value[..4]), int(&value[4..]))
                } else {
                    (signed(&value[..4]), signed(&value[4..]))
                };
                if den == 0.0 { f64::NAN } else { num / den }
            }
            // BYTE comes back as bytes, ASCII as str, UNDEFINED as bytes: none is a key of Pillow's method table.
            _ => return Ok(Tag::Other),
        };
        return Ok(Tag::Number(number));
    }
    Ok(Tag::Absent)
}

/// A PNG's metadata plus what Pillow's 16-bit transparency handling needs.
#[derive(Default)]
pub(super) struct Png {
    pub meta: Metadata,
    pub bit_depth: u8,
    pub color_type: u8,
    pub trns: Option<Vec<u8>>,
    /// Pillow's `_open` checks the CRC of every chunk before the first IDAT and refuses the file on a mismatch
    /// (it never checks image data or later chunks).
    pub header_crc_ok: bool,
}

/// CRC-32 (IEEE), as PNG chunks carry it.
fn crc32(parts: &[&[u8]]) -> u32 {
    let mut crc = !0u32;
    for part in parts {
        for &byte in *part {
            crc ^= byte as u32;
            for _ in 0..8 {
                crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
            }
        }
    }
    !crc
}

/// Walks every chunk, including those after the image data, which `exif_transpose`'s `load()` reads too.
pub(super) fn png(data: &[u8]) -> Png {
    let mut out = Png { header_crc_ok: true, ..Png::default() };
    let mut raw_exif = None;
    let mut pos = 8;
    let mut before_idat = true;
    while let Some(head) = data.get(pos..pos + 8) {
        let len = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
        let kind = &head[4..8];
        let Some(body) = data.get(pos + 8..pos + 8 + len) else { break };
        before_idat &= kind != b"IDAT";
        if before_idat {
            let stored = data.get(pos + 8 + len..pos + 12 + len).map(|c| u32::from_be_bytes(c.try_into().unwrap()));
            out.header_crc_ok &= stored == Some(crc32(&[kind, body]));
        }
        match kind {
            b"IHDR" if body.len() >= 10 => (out.bit_depth, out.color_type) = (body[8], body[9]),
            b"tRNS" => out.trns = Some(body.to_vec()),
            b"eXIf" => out.meta.exif = Some([b"Exif\0\0".as_slice(), body].concat()),
            b"tEXt" | b"iTXt" => {
                if let Some((key, text)) = text_chunk(kind, body) {
                    match key {
                        b"XML:com.adobe.xmp" => out.meta.xmp = Some(text.to_vec()),
                        b"Raw profile type exif" => raw_exif = raw_profile(text),
                        _ => {}
                    }
                }
            }
            b"IEND" => break,
            _ => {}
        }
        pos += 12 + len;
    }
    if out.meta.exif.is_none() {
        out.meta.exif = raw_exif;
    }
    out
}

/// Keyword and text of a `tEXt` or uncompressed `iTXt` chunk. Compressed text is not inflated: no XMP is read
/// from it (Pillow would).
fn text_chunk<'a>(kind: &[u8], body: &'a [u8]) -> Option<(&'a [u8], &'a [u8])> {
    let nul = body.iter().position(|&b| b == 0)?;
    let (key, rest) = (&body[..nul], &body[nul + 1..]);
    if kind == b"tEXt" {
        return Some((key, rest));
    }
    let (&flag, rest) = rest.split_first()?;
    if flag != 0 {
        return None;
    }
    let rest = rest.get(1..)?; // compression method
    let lang_end = rest.iter().position(|&b| b == 0)?;
    let rest = &rest[lang_end + 1..];
    let tkey_end = rest.iter().position(|&b| b == 0)?;
    Some((key, &rest[tkey_end + 1..]))
}

/// ImageMagick's "\nexif\n<len>\n<hex lines>": `bytes.fromhex("".join(value.split("\n")[3:]))`.
fn raw_profile(text: &[u8]) -> Option<Vec<u8>> {
    let hex: Vec<u8> = text.split(|&b| b == b'\n').skip(3).flatten().copied().filter(|b| !b.is_ascii_whitespace()).collect();
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    hex.chunks(2).map(|p| u8::from_str_radix(std::str::from_utf8(p).ok()?, 16).ok()).collect()
}

/// The first `EXIF` and `XMP ` chunks of a WebP's RIFF container, as libwebp's demuxer gives Pillow.
pub(super) fn webp(data: &[u8]) -> Metadata {
    let mut meta = Metadata::default();
    let mut pos = 12;
    while let Some(head) = data.get(pos..pos + 8) {
        let len = u32::from_le_bytes([head[4], head[5], head[6], head[7]]) as usize;
        let Some(body) = data.get(pos + 8..pos + 8 + len) else { break };
        match &head[..4] {
            b"EXIF" if meta.exif.is_none() => meta.exif = Some(body.to_vec()),
            b"XMP " if meta.xmp.is_none() => meta.xmp = Some(body.to_vec()),
            _ => {}
        }
        pos += 8 + len + (len & 1);
    }
    meta
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exif(entries: &[(u16, u16, u32, [u8; 4])]) -> Vec<u8> {
        let mut d = b"Exif\0\0MM\0*\0\0\0\x08".to_vec();
        d.extend((entries.len() as u16).to_be_bytes());
        for (tag, typ, n, v) in entries {
            d.extend(tag.to_be_bytes());
            d.extend(typ.to_be_bytes());
            d.extend(n.to_be_bytes());
            d.extend(v);
        }
        d.extend([0; 4]);
        d
    }

    #[test]
    fn crc_matches_png_reference() {
        assert_eq!(crc32(&[b"IEND", b""]), 0xAE42_6082);
    }

    #[test]
    fn first_orientation_entry_wins_and_types_follow_pillow() {
        let meta = |e: Vec<u8>| Metadata { exif: Some(e), xmp: Some(b"<x tiff:Orientation=\"3\"/>".to_vec()) };
        assert_eq!(orientation(&meta(exif(&[(0x112, 3, 1, [0, 6, 0, 0]), (0x112, 3, 1, [0, 1, 0, 0])]))), 6);
        assert_eq!(orientation(&meta(exif(&[(0x112, 3, 1, [0, 1, 0, 0]), (0x112, 3, 1, [0, 6, 0, 0])]))), 1);
        assert_eq!(orientation(&meta(exif(&[(0x112, 4, 1, [0, 0, 0, 8])]))), 8);
        assert_eq!(orientation(&meta(exif(&[(0x112, 1, 1, [6, 0, 0, 0])]))), 1, "BYTE is bytes in Pillow");
        assert_eq!(orientation(&meta(exif(&[(0x112, 3, 2, [0, 6, 0, 1])]))), 6, "extra values dropped");
        assert_eq!(orientation(&meta(exif(&[(0x100, 3, 1, [0, 6, 0, 0])]))), 3, "no tag: XMP decides");
        assert_eq!(orientation(&meta(b"Exif\0\0XXXX0000".to_vec())), 1, "Pillow raises: XMP not read");
        let mut short = exif(&[(0x112, 3, 1, [0, 6, 0, 0])]);
        short.truncate(short.len() - 6);
        assert_eq!(orientation(&meta(short)), 3, "a cut entry ends the directory; XMP decides");
    }
}
