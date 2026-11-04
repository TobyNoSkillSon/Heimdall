//! Bytes to upright 8-bit RGB the way Python's `prepare` gets them from Pillow: `Image.open` limited to JPEG, PNG,
//! WebP and GIF (first frame), `ImageOps.exif_transpose`, then `convert("RGB")`, or for images with any alpha or
//! transparency key, a paste onto white through the alpha.
//!
//! `open` learns the size the source pixel limit is checked on from the same parse the pixels come from, so no
//! decoder allocates for a frame larger than the one checked.

use std::io::Cursor;

use image::metadata::Orientation;
use image::{DynamicImage, GrayImage, ImageBuffer, ImageDecoder, ImageFormat, ImageReader, RgbImage};

use super::gif::{self, Gif};
use super::jpeg::{self, Header, JpegError};
use super::meta::{self, Metadata, Png};
use super::{invalid, ImageError};

fn undecodable<E>(_: E) -> ImageError {
    invalid("image could not be decoded")
}

/// An opened image whose header has been read but whose pixels have not.
pub(super) enum Source<'a> {
    Jpeg(&'a [u8], Header),
    Gif(Box<Gif<'a>>),
    /// Through the `png` crate directly: Pillow never checks image-data CRCs or the zlib checksum, and the
    /// `image` crate cannot be told to skip them.
    Png(Box<png::Reader<Cursor<&'a [u8]>>>, Png),
    /// WebP, through the `image` crate, with the metadata Pillow reads beside it.
    Other { decoder: Box<dyn ImageDecoder + 'a>, meta: Metadata },
}

pub(super) struct Rgb {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// Pillow's format check by magic bytes, then the header only.
pub(super) fn open(payload: &[u8]) -> Result<Source<'_>, ImageError> {
    if payload.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return jpeg::header(payload).map(|h| Source::Jpeg(payload, h)).ok_or_else(|| undecodable(()));
    }
    if payload.starts_with(b"GIF87a") || payload.starts_with(b"GIF89a") {
        return gif::open(payload).map(|g| Source::Gif(Box::new(g))).ok_or_else(|| undecodable(()));
    }
    if payload.starts_with(b"\x89PNG\r\n\x1a\n") {
        let png = meta::png(payload);
        if !png.header_crc_ok {
            return Err(undecodable(()));
        }
        // The pixel limit is checked on the header before any allocation, so no byte cap here.
        let mut decoder = png::Decoder::new_with_limits(Cursor::new(payload), png::Limits { bytes: usize::MAX });
        decoder.ignore_checksums(true);
        decoder.set_transformations(png::Transformations::EXPAND);
        let reader = decoder.read_info().map_err(undecodable)?;
        return Ok(Source::Png(Box::new(reader), png));
    }
    if payload.len() >= 12 && &payload[..4] == b"RIFF" && &payload[8..12] == b"WEBP" {
        let reader = ImageReader::with_format(Cursor::new(payload), ImageFormat::WebP);
        let decoder: Box<dyn ImageDecoder> = Box::new(reader.into_decoder().map_err(undecodable)?);
        return Ok(Source::Other { decoder, meta: meta::webp(payload) });
    }
    Err(undecodable(()))
}

impl Source<'_> {
    pub fn dimensions(&self) -> (u32, u32) {
        match self {
            Source::Jpeg(_, h) => (h.width, h.height),
            Source::Gif(g) => (g.width, g.height),
            Source::Png(reader, _) => (reader.info().width, reader.info().height),
            Source::Other { decoder, .. } => decoder.dimensions(),
        }
    }

    pub fn decode_rgb(self) -> Result<Rgb, ImageError> {
        let (mut image, orientation, png) = match self {
            Source::Jpeg(data, header) => {
                let d = jpeg::decode(data, &header).map_err(|e| match e {
                    JpegError::Corrupt => invalid("image could not be decoded"),
                    JpegError::Unsupported(what) => invalid(what),
                })?;
                let image = match d.channels {
                    1 => DynamicImage::ImageLuma8(GrayImage::from_raw(d.width, d.height, d.pixels).unwrap()),
                    3 => DynamicImage::ImageRgb8(RgbImage::from_raw(d.width, d.height, d.pixels).unwrap()),
                    _ => DynamicImage::ImageRgb8(RgbImage::from_raw(d.width, d.height, cmyk_to_rgb(&d.pixels)).unwrap()),
                };
                (image, meta::orientation(&header.meta), None)
            }
            // Pillow reads XMP from a GIF's application extension too; not ported, so GIFs are not turned.
            Source::Gif(g) => (g.decode().ok_or_else(|| undecodable(()))?, 1, None),
            Source::Png(mut reader, png) => {
                let image = png_image(&mut reader).ok_or_else(|| undecodable(()))?;
                let orientation = meta::orientation(&png.meta);
                (image, orientation, Some(png))
            }
            Source::Other { decoder, meta } => {
                (DynamicImage::from_decoder(decoder).map_err(undecodable)?, meta::orientation(&meta), None)
            }
        };
        if let Some(o) = Orientation::from_exif(orientation) {
            image.apply_orientation(o);
        }
        let (width, height) = (image.width(), image.height());
        Ok(Rgb { width, height, pixels: to_rgb(image, png.as_ref()) })
    }
}

/// The first frame (the IDAT image) with sub-byte samples expanded and 16-bit ones kept, as `image` would give it.
fn png_image(reader: &mut png::Reader<Cursor<&[u8]>>) -> Option<DynamicImage> {
    let mut buf = vec![0u8; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    buf.truncate(info.buffer_size());
    let (w, h) = (info.width, info.height);
    let wide = || buf.chunks_exact(2).map(|b| u16::from_be_bytes([b[0], b[1]])).collect::<Vec<u16>>();
    use png::{BitDepth::*, ColorType::*};
    Some(match (info.color_type, info.bit_depth) {
        (Grayscale, Eight) => DynamicImage::ImageLuma8(ImageBuffer::from_raw(w, h, buf)?),
        (GrayscaleAlpha, Eight) => DynamicImage::ImageLumaA8(ImageBuffer::from_raw(w, h, buf)?),
        (Rgb, Eight) => DynamicImage::ImageRgb8(ImageBuffer::from_raw(w, h, buf)?),
        (Rgba, Eight) => DynamicImage::ImageRgba8(ImageBuffer::from_raw(w, h, buf)?),
        (Grayscale, Sixteen) => DynamicImage::ImageLuma16(ImageBuffer::from_raw(w, h, wide())?),
        (GrayscaleAlpha, Sixteen) => DynamicImage::ImageLumaA16(ImageBuffer::from_raw(w, h, wide())?),
        (Rgb, Sixteen) => DynamicImage::ImageRgb16(ImageBuffer::from_raw(w, h, wide())?),
        (Rgba, Sixteen) => DynamicImage::ImageRgba16(ImageBuffer::from_raw(w, h, wide())?),
        _ => return None,
    })
}

/// Pillow's `paste(rgba, mask=rgba)` onto white: `DIV255(255 * (255 - a) + c * a)` with its rounding.
fn over_white(c: u8, a: u8) -> u8 {
    let t = 255 * (255 - a as u32) + c as u32 * a as u32 + 128;
    (((t >> 8) + t) >> 8) as u8
}

/// Pillow's CMYK JPEG path: it always unpacks as Adobe-inverted (`CMYK;I`), then `cmyk2rgb`.
fn cmyk_to_rgb(cmyk: &[u8]) -> Vec<u8> {
    let muldiv255 = |a: i32, b: i32| {
        let t = a * b + 128;
        ((t >> 8) + t) >> 8
    };
    cmyk.chunks_exact(4)
        .flat_map(|p| {
            let [c, m, y, k] = [0, 1, 2, 3].map(|i| 255 - p[i] as i32);
            let nk = 255 - k;
            [c, m, y].map(|v| (nk - muldiv255(v, nk)).clamp(0, 255) as u8)
        })
        .collect()
}

/// High byte, as Pillow's `;16B` raw modes unpack 16-bit RGB(A) and LA PNGs.
fn hi(v: u16) -> u8 {
    (v >> 8) as u8
}

/// A PNG `tRNS` key as Pillow applies it (`ImagingConvertTransparent`): the image is first converted to 8-bit RGB
/// (16-bit grey clamped, 16-bit colour to its high bytes), then pixels equal to the key's low bytes turn
/// transparent; a 1-bit key means black or white. The `png` crate compares raw samples instead.
fn trns_key(png: Option<&Png>) -> Option<(u8, [u32; 3])> {
    let png = png?;
    let t = png.trns.as_deref()?;
    let low = |i: usize| t.get(i + 1).map(|&b| b as u32);
    match png.color_type {
        0 if png.bit_depth == 1 => Some((0, [if low(0)? != 0 || t[0] != 0 { 255 } else { 0 }, 0, 0])),
        0 => Some((0, [low(0)?, 0, 0])),
        2 => Some((2, [low(0)?, low(2)?, low(4)?])),
        _ => None,
    }
}

fn to_rgb(image: DynamicImage, png: Option<&Png>) -> Vec<u8> {
    let gray = |v: u8| [v, v, v];
    let flat = |rgba: [u8; 4]| [0, 1, 2].map(|i| over_white(rgba[i], rgba[3]));
    let key = trns_key(png);
    match image {
        DynamicImage::ImageRgb8(b) => b.into_raw(),
        DynamicImage::ImageLuma8(b) => b.into_raw().into_iter().flat_map(gray).collect(),
        DynamicImage::ImageRgba8(b) => match key {
            Some((2, t)) => b
                .into_raw()
                .chunks_exact(4)
                .flat_map(|p| {
                    let clear = [p[0], p[1], p[2]].map(u32::from) == t;
                    flat([p[0], p[1], p[2], if clear { 0 } else { 255 }])
                })
                .collect(),
            _ => b.into_raw().chunks_exact(4).flat_map(|p| flat([p[0], p[1], p[2], p[3]])).collect(),
        },
        DynamicImage::ImageLumaA8(b) => match key {
            Some((0, [t, ..])) => {
                b.into_raw().chunks_exact(2).flat_map(|p| gray(if p[0] as u32 == t { 255 } else { p[0] })).collect()
            }
            _ => b.into_raw().chunks_exact(2).flat_map(|p| gray(over_white(p[0], p[1]))).collect(),
        },
        DynamicImage::ImageRgb16(b) => b.into_raw().into_iter().map(hi).collect(),
        DynamicImage::ImageRgba16(b) => match key {
            Some((2, t)) => b
                .into_raw()
                .chunks_exact(4)
                .flat_map(|p| {
                    let rgb = [hi(p[0]), hi(p[1]), hi(p[2])];
                    let clear = rgb.map(u32::from) == t;
                    flat([rgb[0], rgb[1], rgb[2], if clear { 0 } else { 255 }])
                })
                .collect(),
            _ => b.into_raw().chunks_exact(4).flat_map(|p| flat([hi(p[0]), hi(p[1]), hi(p[2]), hi(p[3])])).collect(),
        },
        DynamicImage::ImageLumaA16(b) => match key {
            // 16-bit grey with a key: Pillow's `I;16` clamps to 255, then keys on the result.
            Some((0, [t, ..])) => b
                .into_raw()
                .chunks_exact(2)
                .flat_map(|p| {
                    let v = p[0].min(255) as u8;
                    gray(if v as u32 == t { 255 } else { v })
                })
                .collect(),
            _ => b.into_raw().chunks_exact(2).flat_map(|p| gray(over_white(hi(p[0]), hi(p[1])))).collect(),
        },
        // Pillow opens 16-bit grey as `I;16`, and its conversion to RGB clamps rather than scales.
        DynamicImage::ImageLuma16(b) => b.into_raw().into_iter().flat_map(|v| gray(v.min(255) as u8)).collect(),
        other => other.to_rgb8().into_raw(),
    }
}

#[cfg(test)]
mod tests {
    use super::over_white;

    #[test]
    fn compositing_matches_pillow_endpoints() {
        assert_eq!(over_white(0, 0), 255);
        assert_eq!(over_white(17, 255), 17);
        assert_eq!(over_white(0, 128), 127);
    }
}
