//! Shared mono resampling for archival audio and speech verification.
use anyhow::Result;
pub fn mono_16k(samples: &[f32], sample_rate: u32, channels: u16) -> Result<Vec<f32>> {
    use rubato::{FftFixedInOut, Resampler};
    let mono: Vec<f32> = samples
        .chunks_exact(channels as usize)
        .map(|c| c.iter().sum::<f32>() / channels as f32)
        .collect();
    if sample_rate == 16000 {
        return Ok(mono);
    }
    let mut r = FftFixedInOut::<f32>::new(sample_rate as usize, 16000, 1024, 1)?;
    let step = r.input_frames_next();
    let delay = r.output_delay();
    let wanted = mono.len() * 16000 / sample_rate as usize;
    let mut output = vec![];
    for chunk in mono.chunks(step) {
        let mut block = vec![0.; step];
        block[..chunk.len()].copy_from_slice(chunk);
        output.extend(r.process(&[block], None)?.remove(0));
    }
    while output.len() < wanted + delay {
        output.extend(r.process(&[vec![0.; step]], None)?.remove(0));
    }
    Ok(output[delay..delay + wanted].to_vec())
}
