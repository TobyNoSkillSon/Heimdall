//! Offline Discord-like Opus simulation, using the statically linked reference codec.
use anyhow::{ensure, Result};
use std::{
    ffi::{c_char, c_void, CStr},
    ptr::NonNull,
};
unsafe extern "C" {
    fn opus_encoder_create(
        rate: i32,
        channels: i32,
        application: i32,
        error: *mut i32,
    ) -> *mut c_void;
    fn opus_encoder_destroy(encoder: *mut c_void);
    fn opus_encoder_ctl(encoder: *mut c_void, request: i32, ...) -> i32;
    fn opus_encode(
        encoder: *mut c_void,
        pcm: *const i16,
        frame_size: i32,
        data: *mut u8,
        max_bytes: i32,
    ) -> i32;
    fn opus_decoder_create(rate: i32, channels: i32, error: *mut i32) -> *mut c_void;
    fn opus_decoder_destroy(decoder: *mut c_void);
    fn opus_decode(
        decoder: *mut c_void,
        bytes: *const u8,
        len: i32,
        pcm: *mut i16,
        frame_size: i32,
        fec: i32,
    ) -> i32;
    fn opus_strerror(error: i32) -> *const c_char;
    fn opus_get_version_string() -> *const c_char;
}
fn check(status: i32) -> Result<()> {
    if status < 0 {
        anyhow::bail!(
            "Opus: {}",
            unsafe { CStr::from_ptr(opus_strerror(status)) }.to_string_lossy()
        );
    }
    Ok(())
}
struct Encoder(NonNull<c_void>);
impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe { opus_encoder_destroy(self.0.as_ptr()) }
    }
}
struct Decoder(NonNull<c_void>);
impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe { opus_decoder_destroy(self.0.as_ptr()) }
    }
}
pub fn version() -> String {
    unsafe { CStr::from_ptr(opus_get_version_string()) }
        .to_string_lossy()
        .into_owned()
}
pub struct CodecReplay {
    pub decoded: Vec<i16>,
    pub packets: Vec<Vec<u8>>,
    pub lookahead_samples: usize,
}
/// Mono 16 kHz, 20 ms packets, OPUS_APPLICATION_VOIP, no packet loss. Remove codec
/// lookahead and flush its tail so decoded/reference timestamps remain identical.
pub fn roundtrip(pcm: &[i16], bitrate: i32) -> Result<CodecReplay> {
    ensure!(
        !pcm.is_empty() && (6000..=128000).contains(&bitrate),
        "invalid Opus replay input"
    );
    let mut error = 0;
    let encoder = Encoder(
        NonNull::new(unsafe { opus_encoder_create(16000, 1, 2048, &mut error) })
            .ok_or_else(|| anyhow::anyhow!("Opus encoder allocation"))?,
    );
    check(error)?;
    let decoder = Decoder(
        NonNull::new(unsafe { opus_decoder_create(16000, 1, &mut error) })
            .ok_or_else(|| anyhow::anyhow!("Opus decoder allocation"))?,
    );
    check(error)?;
    check(unsafe { opus_encoder_ctl(encoder.0.as_ptr(), 4002, bitrate) })?;
    let mut lookahead = 0i32;
    check(unsafe { opus_encoder_ctl(encoder.0.as_ptr(), 4027, &mut lookahead as *mut i32) })?;
    let lookahead = lookahead as usize;
    let frames = (pcm.len() + lookahead).div_ceil(320);
    let mut packets = Vec::with_capacity(frames);
    let mut decoded = Vec::with_capacity(frames * 320);
    for frame in 0..frames {
        let mut input = [0i16; 320];
        let start = frame * 320;
        if start < pcm.len() {
            let n = (pcm.len() - start).min(320);
            input[..n].copy_from_slice(&pcm[start..start + n]);
        }
        let mut packet = vec![0u8; 4000];
        let n = unsafe {
            opus_encode(
                encoder.0.as_ptr(),
                input.as_ptr(),
                320,
                packet.as_mut_ptr(),
                packet.len() as i32,
            )
        };
        check(n)?;
        packet.truncate(n as usize);
        let mut output = [0i16; 320];
        let n = unsafe {
            opus_decode(
                decoder.0.as_ptr(),
                packet.as_ptr(),
                packet.len() as i32,
                output.as_mut_ptr(),
                320,
                0,
            )
        };
        check(n)?;
        ensure!(n == 320, "unexpected Opus frame length");
        decoded.extend_from_slice(&output);
        packets.push(packet);
    }
    let decoded = decoded[lookahead..lookahead + pcm.len()].to_vec();
    Ok(CodecReplay {
        decoded,
        packets,
        lookahead_samples: lookahead,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn opus_replay_preserves_length_and_alignment() {
        let input: Vec<i16> = (0..16000)
            .map(|i| ((i as f32 * 0.11).sin() * 10000.) as i16)
            .collect();
        let replay = roundtrip(&input, 24000).unwrap();
        assert_eq!(replay.decoded.len(), input.len());
        let dot: f64 = input
            .iter()
            .zip(&replay.decoded)
            .map(|(a, b)| *a as f64 * *b as f64)
            .sum();
        let norm = |x: &[i16]| x.iter().map(|v| (*v as f64).powi(2)).sum::<f64>().sqrt();
        assert!(dot / (norm(&input) * norm(&replay.decoded)) > 0.9);
        assert!(replay.packets.iter().map(Vec::len).sum::<usize>() < 12000);
    }
}
