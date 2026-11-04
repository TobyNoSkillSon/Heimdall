//! Agent-run resident latency/energy harness; WAV only, never speakers.
use anyhow::Result;
use serde::Deserialize;
use std::io::{BufRead, Write};
#[derive(Deserialize)]
struct Request {
    #[serde(default)]
    text: String,
    #[serde(default)]
    lang: String,
    wav: Option<String>,
    #[serde(default)]
    repeat: usize,
    #[serde(default)]
    idle_seconds: f64,
}
fn main() -> Result<()> {
    let a: Vec<_> = std::env::args().collect();
    anyhow::ensure!(a.len() == 2, "model-dir");
    let mut t = speak::Supertonic::load(&a[1])?;
    println!("{{\"ready\":true}}");
    std::io::stdout().flush()?;
    for line in std::io::stdin().lock().lines() {
        let r: Request = serde_json::from_str(&line?)?;
        if r.idle_seconds > 0. {
            std::thread::sleep(std::time::Duration::from_secs_f64(r.idle_seconds));
            println!("{{\"idle\":true}}");
        } else {
            let mut times = vec![];
            let mut seconds = 0.;
            for _ in 0..r.repeat.max(1) {
                let a = t.render(&r.text, &r.lang)?;
                if let Some(p) = &r.wav {
                    a.write_wav(p)?;
                }
                times.push(a.first_audio_ms);
                seconds += a.duration_s();
            }
            println!(
                "{}",
                serde_json::json!({"ms":times,"speech_seconds":seconds})
            );
        }
        std::io::stdout().flush()?;
    }
    Ok(())
}
