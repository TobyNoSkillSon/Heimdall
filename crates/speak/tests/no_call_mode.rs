//! Replies never put AirPods into call mode (the user confirmed this on 23 Oct, and it must stay so). macOS switches a
//! Bluetooth headset to hands-free (24 kHz, call quality, other audio silenced) when a process records from it or runs
//! Apple voice processing on it. Heimdall's voice is a plain cpal output stream on the default output device: this
//! guard fails if any speak source starts reading an input or creates a voice-processing unit. (Capture owns the
//! microphones, and its echo unit refuses Bluetooth routes: capture/src/macos/vpio.rs.)
#[test]
fn playback_opens_no_input_and_no_voice_processing() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut checked = 0;
    let mut stack = vec![dir];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            let source = std::fs::read_to_string(&path).unwrap();
            for forbidden in ["build_input_stream", "default_input_device", "input_devices(", "VoiceProcessing", "vpio", "EnableIO",
                "kAudioUnitSubType", "AudioComponentInstanceNew", "AudioDeviceCreateIOProcID", "inputNode"] {
                assert!(!source.contains(forbidden), "{} uses {forbidden}: playback must stay output-only", path.display());
            }
            checked += 1;
        }
    }
    assert!(checked >= 10, "speak sources not found ({checked})");
}
