use crate::{AudioClip, Decision, DecisionInput, DecisionMedia, D1};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
/// Append-only training archive. F32 WAVs retain the exact input waveform.
pub type ArchiveSink =
    Box<dyn FnMut(&DecisionInput, Option<&Decision>, Option<&str>, Option<&Value>) -> Result<()> + Send>;
pub struct TrainingArchive {
    sink: Option<ArchiveSink>,
    root: PathBuf,
    log: File,
}
impl TrainingArchive {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(root.join("audio"))?;
        std::fs::create_dir_all(root.join("images"))?;
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(root.join("decisions.jsonl"))?;
        Ok(Self {
            root,
            log,
            sink: None,
        })
    }
    pub fn set_sink(&mut self, sink: ArchiveSink) {
        self.sink = Some(sink);
    }
    fn audio(&self, a: &AudioClip) -> Result<Value> {
        if a.samples.is_empty() {return Ok(Value::Null);}
        a.validate()?;
        let mut h = Sha256::new();
        h.update(a.sample_rate.to_le_bytes());
        for x in &a.samples {
            h.update(x.to_le_bytes());
        }
        let hash = format!("{:x}", h.finalize());
        let relative = format!("audio/{hash}.wav");
        let path = self.root.join(&relative);
        if !path.exists() {
            let tmp = self
                .root
                .join(format!("audio/{hash}.{}.part", std::process::id()));
            let mut w = hound::WavWriter::create(
                &tmp,
                hound::WavSpec {
                    channels: 1,
                    sample_rate: a.sample_rate,
                    bits_per_sample: 32,
                    sample_format: hound::SampleFormat::Float,
                },
            )?;
            for x in &a.samples {
                w.write_sample(*x)?;
            }
            w.finalize()?;
            File::open(&tmp)?.sync_all()?;
            std::fs::rename(tmp, path)?;
        }
        Ok(
            json!({"path":relative,"sample_rate":a.sample_rate,"samples":a.samples.len(),"pcm_sha256":hash,"encoding":"f32le"}),
        )
    }
    pub fn append(&mut self,input:&DecisionInput,decision:Option<&Decision>,error:Option<&str>)->Result<()> {self.append_with_outputs(input,decision,error,None)}
    pub fn append_with_outputs(
        &mut self,
        input: &DecisionInput,
        decision: Option<&Decision>,
        error: Option<&str>,
        model_outputs: Option<&Value>,
    ) -> Result<()> {
        if let Some(sink) = &mut self.sink {
            return sink(input, decision, error, model_outputs);
        }
        let mut record = serde_json::to_value(input)?;
        record["current"]["audio"] = self.audio(&input.current.audio)?;
        for (i, t) in input.context.iter().enumerate() {
            record["context"][i]["audio"] = self.audio(&t.audio)?;
        }
        if let DecisionMedia::Image(image) = &input.media {
            let hash = format!("{:x}", Sha256::digest(&image.bytes));
            let ext = if image.mime == "image/png" {
                "png"
            } else {
                "jpg"
            };
            let relative = format!("images/{hash}.{ext}");
            let path = self.root.join(&relative);
            if !path.exists() {
                let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
                file.write_all(&image.bytes)?;
                file.sync_all()?;
            }
            record["media"] = json!({"kind":"image","value":{"mime":image.mime,"path":relative,"sha256":hash,"bytes":image.bytes.len()}});
        }
        let mut archived_decision = serde_json::to_value(decision)?;
        if let Some(latency) = archived_decision
            .get_mut("latency")
            .and_then(Value::as_object_mut)
        {
            // These are only known after this append completes. Omit unknown values, don't record zero.
            latency.remove("archive_ms");
            latency.remove("total_ms");
        }
        let line = json!({"schema":1,"timestamp_unix_ms":SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),"input":record,"model_outputs":model_outputs,"decision":archived_decision,"latency_scope":"archive stores measured inference time; returned API result includes archive and total time","error":error,"d1_questions":D1::questions(input),"d1_state":decision.map(|d|input.state_text(d.addressed_probability)).transpose()?});
        let mut buf = serde_json::to_vec(&line)?;
        buf.push(b'\n');
        self.log.write_all(&buf).context("append training record")?;
        self.log.sync_data()?;
        Ok(())
    }
}
