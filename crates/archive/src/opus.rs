use anyhow::{Result, ensure};
use std::{
    ffi::{CStr, c_char, c_void},
    ptr::NonNull,
};
unsafe extern "C" {
    fn opus_encoder_create(
        rate: i32,
        channels: i32,
        application: i32,
        error: *mut i32,
    ) -> *mut c_void;
    fn opus_encoder_destroy(p: *mut c_void);
    fn opus_encoder_ctl(p: *mut c_void, request: i32, ...) -> i32;
    fn opus_encode_float(
        p: *mut c_void,
        pcm: *const f32,
        frames: i32,
        data: *mut u8,
        max: i32,
    ) -> i32;
    fn opus_decoder_create(rate: i32, channels: i32, error: *mut i32) -> *mut c_void;
    fn opus_decoder_destroy(p: *mut c_void);
    fn opus_decode_float(
        p: *mut c_void,
        data: *const u8,
        len: i32,
        pcm: *mut f32,
        frames: i32,
        fec: i32,
    ) -> i32;
    fn opus_strerror(error: i32) -> *const c_char;
}
fn check(n: i32) -> Result<usize> {
    ensure!(
        n >= 0,
        "Opus: {}",
        unsafe { CStr::from_ptr(opus_strerror(n)) }.to_string_lossy()
    );
    Ok(n as usize)
}
/// Mono encoder; its frames are 20 ms (`rate / 50` samples).
pub struct Encoder(NonNull<c_void>, usize);
impl Encoder {
    pub fn configured(bitrate: i32, speech: bool) -> Result<Self> {
        Self::at(16000, bitrate, speech)
    }
    /// `rate` is one of Opus's rates (8, 12, 16, 24 or 48 kHz).
    pub fn at(rate: i32, bitrate: i32, speech: bool) -> Result<Self> {
        ensure!([8000, 12000, 16000, 24000, 48000].contains(&rate), "Opus rate {rate}");
        let mut e = 0;
        let p = NonNull::new(unsafe {
            opus_encoder_create(rate, 1, if speech { 2048 } else { 2049 }, &mut e)
        })
        .ok_or_else(|| anyhow::anyhow!("Opus allocation"))?;
        let s = Self(p, rate as usize / 50);
        check(e)?;
        check(unsafe { opus_encoder_ctl(p.as_ptr(), 4002, bitrate) })?;
        check(unsafe { opus_encoder_ctl(p.as_ptr(), 4010, 10) })?;
        if speech {
            check(unsafe { opus_encoder_ctl(p.as_ptr(), 4024, 3001i32) })?; // OPUS_SIGNAL_VOICE
            check(unsafe { opus_encoder_ctl(p.as_ptr(), 4004, 1103i32) })?; // wideband ceiling
        }
        Ok(s)
    }
    pub fn get_lookahead(&mut self) -> Result<i32> {
        let mut n = 0;
        check(unsafe { opus_encoder_ctl(self.0.as_ptr(), 4027, &mut n as *mut i32) })?;
        Ok(n)
    }
    pub fn frame(&self) -> usize {
        self.1
    }
    /// One 20 ms frame (`frame()` samples).
    pub fn encode_float(&mut self, pcm: &[f32], out: &mut [u8]) -> Result<usize> {
        ensure!(pcm.len() == self.1, "Opus frame length");
        check(unsafe {
            opus_encode_float(
                self.0.as_ptr(),
                pcm.as_ptr(),
                self.1 as i32,
                out.as_mut_ptr(),
                out.len() as i32,
            )
        })
    }
}
impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            opus_encoder_destroy(self.0.as_ptr());
        }
    }
}
pub struct Decoder(NonNull<c_void>, usize);
impl Decoder {
    pub fn new() -> Result<Self> {
        Self::at(16000)
    }
    pub fn at(rate: i32) -> Result<Self> {
        ensure!([8000, 12000, 16000, 24000, 48000].contains(&rate), "Opus rate {rate}");
        let mut e = 0;
        let p = NonNull::new(unsafe { opus_decoder_create(rate, 1, &mut e) })
            .ok_or_else(|| anyhow::anyhow!("Opus decoder allocation"))?;
        let s = Self(p, rate as usize / 50);
        check(e)?;
        Ok(s)
    }
    /// Decodes one packet into `out` (`frame` samples at this rate).
    pub fn decode_float(&mut self, bytes: &[u8], out: &mut [f32]) -> Result<usize> {
        ensure!(out.len() == self.1, "Opus frame length");
        check(unsafe {
            opus_decode_float(
                self.0.as_ptr(),
                bytes.as_ptr(),
                bytes.len() as i32,
                out.as_mut_ptr(),
                self.1 as i32,
                0,
            )
        })
    }
}
impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe {
            opus_decoder_destroy(self.0.as_ptr());
        }
    }
}
