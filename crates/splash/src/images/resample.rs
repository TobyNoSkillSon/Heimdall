//! Pillow's `Image.resize(..., BICUBIC)` for 8-bit RGB, bit for bit (Pillow 13 `libImaging/Resample.c`).
//!
//! The `image` crate's filters differ in kernel support, normalization and rounding, and the engine's prefix cache
//! keys on a digest of these exact bytes, so this follows Pillow: separable convolution in Pillow's pass order, an
//! 8-bit intermediate, and coefficients in 22-bit fixed point.

/// `32 - 8 - 2`: fixed-point bits Pillow keeps for 8-bit coefficients.
const PRECISION_BITS: u32 = 22;
const SUPPORT: f64 = 2.0;

/// Keys' cubic with a = -0.5, Pillow's `bicubic_filter`.
fn bicubic(x: f64) -> f64 {
    const A: f64 = -0.5;
    let x = x.abs();
    if x < 1.0 {
        ((A + 2.0) * x - (A + 3.0)) * x * x + 1.0
    } else if x < 2.0 {
        (((x - 5.0) * x + 8.0) * x - 4.0) * A
    } else {
        0.0
    }
}

/// One output position's source window and its fixed-point weights.
struct Coeffs {
    ksize: usize,
    /// `(first source index, count)` per output index.
    bounds: Vec<(usize, usize)>,
    weights: Vec<i32>,
}

/// `precompute_coeffs` then `normalize_coeffs_8bpc`, over the full box `[0, in_size)`.
fn coeffs(in_size: usize, out_size: usize) -> Coeffs {
    let scale = in_size as f64 / out_size as f64;
    let filterscale = scale.max(1.0);
    let support = SUPPORT * filterscale;
    let ksize = support.ceil() as usize * 2 + 1;
    let mut bounds = Vec::with_capacity(out_size);
    let mut weights = vec![0i32; out_size * ksize];
    let mut k = vec![0f64; ksize];
    for xx in 0..out_size {
        let center = (xx as f64 + 0.5) * scale;
        let ss = 1.0 / filterscale;
        // C's `(int)` truncates toward zero; a negative start clamps to 0 either way.
        let xmin = ((center - support + 0.5) as i64).max(0) as usize;
        let xmax = ((center + support + 0.5) as i64).min(in_size as i64) as usize - xmin;
        let mut ww = 0.0;
        for (x, slot) in k.iter_mut().enumerate().take(xmax) {
            let w = bicubic((x as f64 + xmin as f64 - center + 0.5) * ss);
            *slot = w;
            ww += w;
        }
        for (x, &w) in k.iter().enumerate().take(xmax) {
            let w = if ww != 0.0 { w / ww } else { w };
            let fixed = w * (1u32 << PRECISION_BITS) as f64;
            weights[xx * ksize + x] = if w < 0.0 { (fixed - 0.5) as i32 } else { (fixed + 0.5) as i32 };
        }
        bounds.push((xmin, xmax));
    }
    Coeffs { ksize, bounds, weights }
}

/// Pillow's `clip8`: drop the fraction (arithmetic shift) and clamp to a byte.
fn clip8(v: i32) -> u8 {
    (v >> PRECISION_BITS).clamp(0, 255) as u8
}

const HALF: i32 = 1 << (PRECISION_BITS - 1);

/// Resizes packed RGB (`width * height * 3` bytes) to `out_width x out_height` as Pillow's BICUBIC does.
pub fn resize_bicubic_rgb(src: &[u8], width: usize, height: usize, out_width: usize, out_height: usize) -> Vec<u8> {
    assert_eq!(src.len(), width * height * 3);
    let (dw, dh) = (width as i64 - out_width as i64, height as i64 - out_height as i64);
    // Pillow 13 runs the vertical pass first when the height shrinks by more than twice what the width does; with
    // 8-bit rounding between passes, the order shows in the bytes.
    if dh > 0 && dh > dw * 2 {
        let tall = vertical(src, width, height, out_height);
        horizontal(&tall, width, out_height, out_width)
    } else {
        let wide = horizontal(src, width, height, out_width);
        vertical(&wide, out_width, height, out_height)
    }
}

fn horizontal(src: &[u8], width: usize, height: usize, out_width: usize) -> Vec<u8> {
    if out_width == width {
        return src.to_vec();
    }
    let c = coeffs(width, out_width);
    let mut out = vec![0u8; out_width * height * 3];
    for (row, dst) in src.chunks_exact(width * 3).zip(out.chunks_exact_mut(out_width * 3)) {
        for (xx, &(xmin, xmax)) in c.bounds.iter().enumerate() {
            let mut ss = [HALF; 3];
            for (x, &w) in c.weights[xx * c.ksize..][..xmax].iter().enumerate() {
                let p = &row[(xmin + x) * 3..][..3];
                for ch in 0..3 {
                    ss[ch] += p[ch] as i32 * w;
                }
            }
            for ch in 0..3 {
                dst[xx * 3 + ch] = clip8(ss[ch]);
            }
        }
    }
    out
}

fn vertical(src: &[u8], width: usize, height: usize, out_height: usize) -> Vec<u8> {
    if out_height == height {
        return src.to_vec();
    }
    let c = coeffs(height, out_height);
    let stride = width * 3;
    let mut out = vec![0u8; stride * out_height];
    // Whole rows at a time: the same sums as Pillow's per-pixel loop, in an order the compiler vectorizes.
    let mut acc = vec![0i32; stride];
    for (yy, &(ymin, ymax)) in c.bounds.iter().enumerate() {
        acc.fill(HALF);
        for (y, &w) in c.weights[yy * c.ksize..][..ymax].iter().enumerate() {
            for (a, &p) in acc.iter_mut().zip(&src[(ymin + y) * stride..][..stride]) {
                *a += p as i32 * w;
            }
        }
        for (d, &a) in out[yy * stride..][..stride].iter_mut().zip(&acc) {
            *d = clip8(a);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weights_sum_to_one_in_fixed_point() {
        for (i, o) in [(100, 448), (700, 448), (5, 3), (3, 5)] {
            let c = coeffs(i, o);
            for (xx, &(_, n)) in c.bounds.iter().enumerate() {
                let s: i32 = c.weights[xx * c.ksize..][..n].iter().sum();
                assert!((s - (1 << PRECISION_BITS)).abs() <= n as i32, "{i}->{o} at {xx}: {s}");
            }
        }
    }

    #[test]
    fn flat_image_stays_flat() {
        let src = vec![77u8; 13 * 9 * 3];
        assert!(resize_bicubic_rgb(&src, 13, 9, 32, 20).iter().all(|&v| v == 77));
    }
}
