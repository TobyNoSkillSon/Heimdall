use anyhow::Result;
fn main() -> Result<()> {
    let a: Vec<_> = std::env::args().collect();
    anyhow::ensure!(a.len() == 5, "model-dir language text output.wav");
    let load = std::time::Instant::now();
    let mut tts = speak::Supertonic::load(&a[1])?;
    let load_ms = load.elapsed().as_secs_f64() * 1000.;
    let audio = tts.render(&a[3], &a[2])?;
    audio.write_wav(&a[4])?;
    println!(
        "{}",
        serde_json::json!({"language":a[2],"text":a[3],"load_ms":load_ms,"first_audio_ms":audio.first_audio_ms,"synthesis_ms":audio.synthesis_ms,"audio_seconds":audio.duration_s(),"rtf":audio.real_time_factor(),"wav":a[4]})
    );
    Ok(())
}
