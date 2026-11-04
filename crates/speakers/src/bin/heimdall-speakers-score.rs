use anyhow::Result;
use speakers::scoring::{self, Annotation};
use std::path::PathBuf;
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|s| s == "--calibrate") {
        anyhow::ensure!(
            args.len() == 4,
            "--calibrate trials.json output.json dataset"
        );
        let trials: Vec<speakers::identity::Trial> =
            serde_json::from_slice(&std::fs::read(&args[1])?)?;
        let calibration = speakers::identity::Calibration::fit(&trials, &args[3])?;
        std::fs::write(&args[2], serde_json::to_vec_pretty(&calibration)?)?;
        return Ok(());
    }
    if args.first().is_some_and(|s| s == "--der") {
        anyhow::ensure!(
            args.len() == 4,
            "--der reference.rttm segments.json seconds"
        );
        let reference = scoring::parse_rttm(&std::fs::read_to_string(&args[1])?)?;
        let receipt: serde_json::Value = serde_json::from_slice(&std::fs::read(&args[2])?)?;
        let segments: Vec<speakers::diar::SpeakerSegment> =
            serde_json::from_value(receipt["segments"].clone())?;
        let hypothesis: Vec<_> = segments
            .into_iter()
            .map(|s| Annotation {
                start: s.start_time,
                end: s.end_time,
                speaker: s.speaker.to_string(),
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string(&scoring::score_der(
                &reference,
                &hypothesis,
                0.,
                args[3].parse()?
            )?)?
        );
        return Ok(());
    }
    let out = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("models/speakers"));
    let mut receipt: serde_json::Value =
        serde_json::from_slice(&std::fs::read(out.join("evaluation-callhome.json"))?)?;
    let manual = scoring::parse_rttm(&std::fs::read_to_string(out.join("ES2004a.rttm"))?)?;
    let forced = scoring::parse_rttm(&std::fs::read_to_string(out.join("ES2004a-forced.rttm"))?)?;
    for (index, bitrate) in [0, 24000, 64000].iter().enumerate() {
        let final_run: serde_json::Value =
            serde_json::from_slice(&std::fs::read(out.join(format!("final-{bitrate}.json")))?)?;
        let segs: Vec<speakers::diar::SpeakerSegment> =
            serde_json::from_value(final_run["segments"].clone())?;
        let hypothesis: Vec<_> = segs
            .iter()
            .map(|s| Annotation {
                start: s.start_time,
                end: s.end_time,
                speaker: s.speaker.to_string(),
            })
            .collect();
        let primary = scoring::score_der(&forced, &hypothesis, 0., 480.)?;
        let secondary = scoring::score_der(&manual, &hypothesis, 0., 480.)?;
        let condition = &mut receipt["conditions"][index];
        for key in [
            "processing_seconds",
            "rtf",
            "first_result_audio_seconds",
            "peak_memory_bytes",
            "segments",
            "load_seconds",
        ] {
            condition[key] = final_run[key].clone();
        }
        condition["der"] = serde_json::to_value(&primary)?;
        condition["manual_word_der"] = serde_json::to_value(secondary)?;
        println!(
            "{bitrate}: forced-alignment DER {:.3}%, RTF {}",
            primary.der_percent, condition["rtf"]
        );
    }
    receipt["postprocessing"] = serde_json::json!({"onset":0.5,"offset":0.5,"pad_onset":0,"pad_offset":0,"min_duration_on":0,"min_duration_off":0,"source":"NeMo Speech e2e_diarize_speech default bypass_postprocessing=true"});
    receipt["reference"]=serde_json::json!("nttcslab-sp/diar-forced-alignment@9527b7c64846fb38316a610f32e9d3466bd6d8b7/AMI/test/ES2004a.rttm; published Nemotron annotation source; [0,480) crop");
    receipt["identity_reference"]=serde_json::json!("Manual only_words AMI RTTM, two enrollment and two calibration windows/voice/codec; disjoint later windows for identification, unchanged by diarization postprocessing");
    receipt["measurement_method"]=serde_json::json!("Final native default diarization replayed per condition; original disjoint identity measurements retained because only diarization postprocessing changed. Each replay loads identity encoder too; peak RSS includes model load and one diagnostic embedding, excludes CPU/GPU driver memory outside the process.");
    std::fs::write(
        out.join("evaluation.json"),
        serde_json::to_vec_pretty(&receipt)?,
    )?;
    Ok(())
}
