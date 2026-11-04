use anyhow::{ensure, Result};
use std::path::Path;
pub fn read_wav(path: &Path) -> Result<Vec<f32>> {
    let mut reader = hound::WavReader::open(path)?;
    let spec = reader.spec();
    ensure!(
        spec.channels == 1 && spec.sample_rate == 16000,
        "Expected mono 16000 Hz WAV; got {} channels / {} Hz",
        spec.channels,
        spec.sample_rate
    );
    let data = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<Vec<_>, _>>()?,
        hound::SampleFormat::Int => {
            let scale = 2_f32.powi(spec.bits_per_sample as i32 - 1);
            reader
                .samples::<i32>()
                .map(|v| v.map(|x| x as f32 / scale))
                .collect::<Result<Vec<_>, _>>()?
        }
    };
    ensure!(
        data.iter().all(|v| v.is_finite()),
        "WAV contains nonfinite samples"
    );
    Ok(data)
}
