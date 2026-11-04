//! A GIF's first frame as Pillow 13 makes it: on a canvas the logical screen's size, grown to hold a frame that
//! overflows it (so a 0x0 screen takes the frame's size), with uncovered pixels at palette index 0. The `image`
//! crate instead clips to the screen and leaves uncovered pixels transparent black.

use std::io::Cursor;

use image::{DynamicImage, RgbImage, RgbaImage};

pub(super) struct Gif<'a> {
    decoder: gif::Decoder<Cursor<&'a [u8]>>,
    pub width: u32,
    pub height: u32,
    frame: (u32, u32, u32, u32),
    transparent: Option<u8>,
    palette: Option<Vec<u8>>,
}

/// Reads the header and first frame descriptor only, so the canvas size is known before any pixel is decoded.
pub(super) fn open(data: &[u8]) -> Option<Gif<'_>> {
    let mut options = gif::DecodeOptions::new();
    options.set_color_output(gif::ColorOutput::Indexed);
    options.check_frame_consistency(false);
    let mut decoder = options.read_info(Cursor::new(data)).ok()?;
    let (screen_w, screen_h) = (decoder.width() as u32, decoder.height() as u32);
    let global = decoder.global_palette().map(<[u8]>::to_vec);
    let f = decoder.next_frame_info().ok()??;
    let frame = (f.left as u32, f.top as u32, f.width as u32, f.height as u32);
    let (transparent, palette) = (f.transparent, f.palette.clone().or(global));
    let width = screen_w.max(frame.0 + frame.2);
    let height = screen_h.max(frame.1 + frame.3);
    Some(Gif { decoder, width, height, frame, transparent, palette })
}

impl Gif<'_> {
    pub fn decode(mut self) -> Option<DynamicImage> {
        let (left, top, fw, fh) = self.frame;
        let mut indices = vec![0u8; self.decoder.buffer_size()];
        self.decoder.read_into_buffer(&mut indices).ok()?;
        let (w, h) = (self.width as usize, self.height as usize);
        let mut canvas = vec![0u8; w * h];
        for (y, row) in indices.chunks_exact(fw as usize).take(fh as usize).enumerate() {
            let at = (top as usize + y) * w + left as usize;
            canvas[at..at + row.len()].copy_from_slice(row);
        }
        // Without a palette Pillow opens the frame as "L"; its default palette is the same grey ramp, which also
        // stands in for entries a short palette lacks.
        let color = |i: u8| -> [u8; 3] {
            match &self.palette {
                Some(p) if (i as usize) * 3 + 3 <= p.len() => [p[i as usize * 3], p[i as usize * 3 + 1], p[i as usize * 3 + 2]],
                _ => [i; 3],
            }
        };
        Some(match self.transparent {
            Some(t) => DynamicImage::ImageRgba8(RgbaImage::from_fn(self.width, self.height, |x, y| {
                let i = canvas[y as usize * w + x as usize];
                let [r, g, b] = color(i);
                image::Rgba([r, g, b, if i == t { 0 } else { 255 }])
            })),
            None => DynamicImage::ImageRgb8(RgbImage::from_fn(self.width, self.height, |x, y| {
                image::Rgb(color(canvas[y as usize * w + x as usize]))
            })),
        })
    }
}
