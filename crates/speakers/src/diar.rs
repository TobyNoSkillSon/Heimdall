use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    ffi::{CStr, CString, c_char, c_void},
    path::Path,
    ptr::NonNull,
    rc::Rc,
};

#[repr(C)]
struct ModelConfig {
    size: usize,
    model_path: *const c_char,
    gpu: i32,
    preset: *const c_char,
    chunk_frames: i32,
    right_context_frames: i32,
    left_context_frames: i32,
    fifo_frames: i32,
    spkcache_frames: i32,
    update_period_frames: i32,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct SpeakerSegment {
    pub start_time: f64,
    pub end_time: f64,
    pub speaker: i32,
}
unsafe extern "C" {
    fn heimdall_diar_drain(stream: *mut c_void, out: *mut SpeakerSegment, capacity: usize, count: *mut usize) -> i32;
    fn nemo_speech_diar_create(config: *const ModelConfig, out: *mut *mut c_void) -> i32;
    fn nemo_speech_diar_destroy(model: *mut c_void);
    fn nemo_speech_diar_num_speakers(model: *const c_void) -> i32;
    fn nemo_speech_diar_seconds_per_frame(model: *const c_void) -> f64;
    fn nemo_speech_diar_stream_open(model: *mut c_void, out: *mut *mut c_void) -> i32;
    fn nemo_speech_diar_stream_push_f32(
        stream: *mut c_void,
        samples: *const f32,
        len: usize,
        sr: i32,
    ) -> i32;
    fn nemo_speech_diar_stream_finish(stream: *mut c_void) -> i32;
    fn nemo_speech_diar_stream_close(stream: *mut c_void);
    fn heimdall_diar_output_revision(stream: *const c_void) -> i64;
    fn heimdall_diar_stable_frames(stream: *const c_void) -> i64;
    fn heimdall_diar_probs_since(stream: *const c_void,since:i64,out:*mut f32,capacity:usize) -> i32;
    fn nemo_speech_diar_frame_count(stream: *const c_void) -> i64;
    fn nemo_speech_diar_frame_probs_start(stream: *const c_void) -> i64;
    fn nemo_speech_diar_segments(
        stream: *const c_void,
        config: *const c_void,
        out: *mut SpeakerSegment,
        capacity: usize,
        count: *mut usize,
    ) -> i32;
    fn nemo_speech_asr_last_error() -> *const c_char;
}
// Metal creates autoreleased command buffers on the Rust-owned worker thread.
// Drain each native invocation, including teardown; a Rust thread has no AppKit pool.
fn native_pool<T>(f: impl FnOnce() -> T) -> T {
    #[cfg(target_os = "macos")]
    {
        objc2::rc::autoreleasepool(|_| {
            let result = f();
            crate::metal::drain_native_scratch();
            result
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        f()
    }
}
fn check(code: i32) -> Result<()> {
    if code != 0 {
        let error = unsafe { CStr::from_ptr(nemo_speech_asr_last_error()) }.to_string_lossy();
        if error.contains("HEIMDALL_GPU_FAULT") {
            return Err(crate::gpu_recovery::GpuError(error.into_owned()).into());
        }
        bail!("NeMo diarization error {code}: {error}")
    }
    Ok(())
}
struct Model {
    raw: NonNull<c_void>,
    #[cfg(target_os = "macos")]
    _metal: Option<crate::metal::NemotronGpu>,
}
impl Drop for Model {
    fn drop(&mut self) {
        unsafe {
            native_pool(|| nemo_speech_diar_destroy(self.raw.as_ptr()));
        }
    }
}
/// Shared native model; deliberately !Send/!Sync, as are its streams. Keep on a dedicated worker.
#[derive(Clone)]
pub struct Diarizer {
    model: Rc<Model>,
    pub speakers: usize,
    pub seconds_per_frame: f64,
}
impl Diarizer {
    /// Native v3 streaming with published ultra-low-latency geometry: 0.32 s latency, 8 tracks, Q8 weights. GPU 0 on macOS; -1 for CPU.
    pub fn load(path: &Path, gpu: i32) -> Result<Self> {
        ensure!(
            path.is_file(),
            "missing diarization model: {}",
            path.display()
        );
        crate::artifacts::verify(
            path,
            107012128,
            "08456d9e22cd9a323c0364d98375f3746d6e68507ebb705cd46438c534c7a3a1",
        )?;
        let name = CString::new(
            path.to_str()
                .ok_or_else(|| anyhow::anyhow!("non-UTF8 model path"))?,
        )?;
        let preset = CString::new("v3-streaming")?;
        let config = ModelConfig {
            size: std::mem::size_of::<ModelConfig>(),
            model_path: name.as_ptr(),
            gpu,
            preset: preset.as_ptr(),
            chunk_frames: 3,
            right_context_frames: 1,
            left_context_frames: 0,
            fifo_frames: 264,
            spkcache_frames: 264,
            update_period_frames: 222,
        };
        let mut raw = std::ptr::null_mut();
        #[cfg(target_os = "macos")]
        let metal = if gpu >= 0 && crate::metal::enabled("nemotron") {
            Some(crate::metal::NemotronGpu::new()?)
        } else {
            None
        };
        check(native_pool(|| unsafe {
            nemo_speech_diar_create(&config, &mut raw)
        }))?;
        let model = Rc::new(Model {
            raw: NonNull::new(raw).ok_or_else(|| anyhow::anyhow!("null NeMo model"))?,
            #[cfg(target_os = "macos")]
            _metal: metal,
        });
        let speakers = unsafe { nemo_speech_diar_num_speakers(raw) } as usize;
        let seconds_per_frame = unsafe { nemo_speech_diar_seconds_per_frame(raw) };
        ensure!(
            speakers == 8 && seconds_per_frame > 0.,
            "expected Nemotron 3 eight-track model"
        );
        Ok(Self {
            model,
            speakers,
            seconds_per_frame,
        })
    }
    pub fn stream(&self, sample_rate: u32) -> Result<DiarStream> {
        ensure!((8000..=96000).contains(&sample_rate), "invalid sample rate");
        let mut raw = std::ptr::null_mut();
        check(native_pool(|| unsafe {
            nemo_speech_diar_stream_open(self.model.raw.as_ptr(), &mut raw)
        }))?;
        Ok(DiarStream {
            raw: NonNull::new(raw).ok_or_else(|| anyhow::anyhow!("null stream"))?,
            _model: self.model.clone(),
            sample_rate,
            finished: false,
            speakers: self.speakers,
            seconds_per_frame: self.seconds_per_frame,
        })
    }
}
#[derive(Debug, Serialize, Deserialize)]
pub struct FrameProbabilities {
    pub first_frame: i64,
    pub frame_count: i64,
    pub speakers: usize,
    pub seconds_per_frame: f64,
    pub values: Vec<f32>,
}
pub struct DiarStream {
    raw: NonNull<c_void>,
    _model: Rc<Model>,
    sample_rate: u32,
    finished: bool,
    pub speakers: usize,
    pub seconds_per_frame: f64,
}
impl Drop for DiarStream {
    fn drop(&mut self) {
        unsafe {
            native_pool(|| nemo_speech_diar_stream_close(self.raw.as_ptr()));
        }
    }
}
impl DiarStream {
    pub fn output_revision(&self) -> i64 { unsafe { heimdall_diar_output_revision(self.raw.as_ptr()) } }
    pub fn stable_frames(&self) -> i64 { unsafe { heimdall_diar_stable_frames(self.raw.as_ptr()) } }
    pub fn frame_count(&self) -> i64 {
        unsafe { nemo_speech_diar_frame_count(self.raw.as_ptr()) }
    }
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    pub fn push_f32(&mut self, audio: &[f32]) -> Result<()> {
        ensure!(!self.finished, "stream already finished");
        ensure!(audio.iter().all(|v| v.is_finite()), "nonfinite PCM");
        check(native_pool(|| unsafe {
            nemo_speech_diar_stream_push_f32(
                self.raw.as_ptr(),
                audio.as_ptr(),
                audio.len(),
                self.sample_rate as i32,
            )
        }))
    }
    pub fn finish(&mut self) -> Result<()> {
        if !self.finished {
            check(native_pool(|| unsafe {
                nemo_speech_diar_stream_finish(self.raw.as_ptr())
            }))?;
            self.finished = true;
        }
        Ok(())
    }
    /// Current snapshot. Tail boundaries can change until more audio arrives; callers replace, not append.
    /// Completed-file callers own their exported history; live callers archive
    /// finalized output incrementally and query only `segments()`.
    pub fn completed_segments(&self) -> Result<Vec<SpeakerSegment>> {
        let mut segments=self.take_finalized_segments()?;
        segments.extend(self.segments()?);
        Ok(segments)
    }
    pub fn take_finalized_segments(&self) -> Result<Vec<SpeakerSegment>> {
        let mut count = 0;
        check(unsafe { heimdall_diar_drain(self.raw.as_ptr(), std::ptr::null_mut(), 0, &mut count) })?;
        if count==0 { return Ok(vec![]); }
        let mut segments = vec![SpeakerSegment::default(); count];
        check(unsafe { heimdall_diar_drain(self.raw.as_ptr(), segments.as_mut_ptr(), segments.len(), &mut count) })?;
        segments.truncate(count);
        Ok(segments)
    }
    pub fn segments(&self) -> Result<Vec<SpeakerSegment>> {
        let mut count = 0;
        check(unsafe {
            nemo_speech_diar_segments(
                self.raw.as_ptr(),
                std::ptr::null(),
                std::ptr::null_mut(),
                0,
                &mut count,
            )
        })?;
        let mut segs = vec![SpeakerSegment::default(); count];
        if count > 0 {
            check(unsafe {
                nemo_speech_diar_segments(
                    self.raw.as_ptr(),
                    std::ptr::null(),
                    segs.as_mut_ptr(),
                    count,
                    &mut count,
                )
            })?;
        }
        segs.truncate(count);
        Ok(segs)
    }
    pub fn frame_probabilities(&self) -> Result<FrameProbabilities> { self.probabilities_since(0) }
    pub fn probabilities_since(&self, since:i64) -> Result<FrameProbabilities> {
        let first_frame = unsafe { nemo_speech_diar_frame_probs_start(self.raw.as_ptr()) }.max(since.min(self.frame_count()));
        let frame_count = unsafe { nemo_speech_diar_frame_count(self.raw.as_ptr()) };
        ensure!(
            frame_count >= first_frame && first_frame >= 0,
            "invalid native frame bounds"
        );
        let mut values = vec![0.; (frame_count - first_frame) as usize * self.speakers];
        if !values.is_empty() {
            check(unsafe {
                heimdall_diar_probs_since(self.raw.as_ptr(),first_frame,values.as_mut_ptr(),values.len())
            })?;
        }
        Ok(FrameProbabilities {
            first_frame,
            frame_count,
            speakers: self.speakers,
            seconds_per_frame: self.seconds_per_frame,
            values,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    unsafe extern "C" {
        fn heimdall_diar_config_size() -> usize;
        fn heimdall_diar_config_preset_offset() -> usize;
        fn heimdall_diar_segment_size() -> usize;
    }
    #[test]
    fn continuous_hour_keeps_native_queries_and_archive_state_bounded() {
        unsafe extern "C" { fn heimdall_diar_continuous_hour(query_us:*mut f64,max_frames:*mut usize,max_pending:*mut usize,archived:*mut f64)->i32; }
        let mut timing=[0.;7]; let mut frames=0;let mut pending=0;let mut archived=0.;
        check(unsafe {heimdall_diar_continuous_hour(timing.as_mut_ptr(),&mut frames,&mut pending,&mut archived)}).unwrap();
        println!("continuous 60min native query median us at 2/10/20/30/40/50/60min: {timing:?}; max_frames={frames}; pending={pending}; archived_speaker_seconds={archived}");
        assert!(frames<=6000 && pending<=8);
        assert!((archived-3600.*8.).abs()<0.001);
        assert!(timing[6] < timing[0]*3., "hour-long query cost grows");
    }
    #[test]
    fn ffi_layout_matches_c_header() {
        unsafe {
            assert_eq!(
                std::mem::size_of::<ModelConfig>(),
                heimdall_diar_config_size()
            );
            assert_eq!(
                std::mem::offset_of!(ModelConfig, preset),
                heimdall_diar_config_preset_offset()
            );
            assert_eq!(
                std::mem::size_of::<SpeakerSegment>(),
                heimdall_diar_segment_size()
            );
        }
    }
}
