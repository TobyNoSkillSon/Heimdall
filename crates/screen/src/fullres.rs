use crate::{attach_held, Action, Frame, PhraseMatch, ScreenBuffer, ScreenHold};
use anyhow::Result;
use std::{
    path::Path,
    sync::{Arc, Mutex},
};

/// Preserve the causal ring attachment, then supplement it with one fresh native
/// frame per display. No native resources exist until this command is called.
/// Spoken commands pin earlier, at detection, through `ScreenHolds`.
pub fn attach_command_screenshot(
    buffer: &Arc<Mutex<ScreenBuffer>>,
    matched: &PhraseMatch,
    now_ns: u64,
    dir: &Path,
) -> Result<Action> {
    let grab = crate::native_grabber();
    let mut call = grab.as_ref().map(|g| move |id| g(id));
    let hold = ScreenHold::pin(
        buffer,
        &matched.source,
        matched.trigger_ns,
        Some(matched.start_ns),
        now_ns,
        call.as_mut().map(|g| g as &mut dyn FnMut(u32) -> Result<Frame>),
    );
    attach_held(hold, matched, now_ns, dir)
}

// Injection keeps failure/stale-generation tests independent of TCC and real screens.
#[doc(hidden)]
pub fn attach_command_with_grabber(
    buffer: &Arc<Mutex<ScreenBuffer>>,
    matched: &PhraseMatch,
    now_ns: u64,
    dir: &Path,
    mut grab: impl FnMut(u32) -> Result<Frame>,
) -> Result<Action> {
    let hold = ScreenHold::pin(
        buffer,
        &matched.source,
        matched.trigger_ns,
        Some(matched.start_ns),
        now_ns,
        Some(&mut grab),
    );
    attach_held(hold, matched, now_ns, dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DisplayInfo, ScreenConfig, ShotKind};
    fn frame(id: u32, width: u32, height: u32, value: u8) -> Frame {
        Frame::new(
            DisplayInfo {
                id,
                x: 0.,
                y: 0.,
                width: 3840,
                height: 2160,
            },
            capture::host_now_ns(),
            width,
            height,
            vec![value; width as usize * height as usize * 3],
        )
        .unwrap()
    }
    fn setup() -> (Arc<Mutex<ScreenBuffer>>, PhraseMatch, u64) {
        let mut b = ScreenBuffer::new(ScreenConfig {fps:1,..Default::default()}).unwrap();
        let anchor=capture::host_now_ns();
        for id in [1, 2] {
            let mut f=frame(id,2,1,id as u8);f.host_time_ns=anchor; b.push(f).unwrap();
        }
        let now = anchor;
        (
            Arc::new(Mutex::new(b)),
            PhraseMatch {
                source: "mic".into(),
                phrase: "look at this".into(),
                start_ns: now,
                trigger_ns: now,
            },
            now,
        )
    }
    fn decode(path: &Path) -> (u32, u32, Vec<u8>) {
        let mut reader =
            png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).unwrap()))
                .read_info()
                .unwrap();
        let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut pixels).unwrap();
        (info.width, info.height, pixels)
    }
    #[test]
    fn two_displays_get_causal_and_readable_frames_with_one_note_and_no_ring_growth() {
        use std::os::unix::fs::PermissionsExt;
        let (b, m, now) = setup();
        let t = tempfile::tempdir().unwrap();
        let before = b.lock().unwrap().bytes();
        let mut calls = vec![];
        let mut native_pixels = vec![];
        let action = attach_command_with_grabber(&b, &m, now, t.path(), |id| {
            // A real producer can continue publishing while the screenshot runs.
            assert!(b.try_lock().is_ok());
            calls.push(id);
            let f = frame(id, 4, 2, 10 + id as u8);
            native_pixels.push(Arc::downgrade(&f.rgb));
            Ok(f)
        })
        .unwrap();
        assert_eq!(calls, [1, 2]);
        assert!(native_pixels.iter().all(|p| p.strong_count() == 0));
        assert_eq!(action.shots.len(), 4);
        assert_eq!(action.note.matches("screenshot").count(), 1);
        assert!(action.full_resolution_errors.is_empty());
        assert_eq!(b.lock().unwrap().bytes(), before);
        assert_eq!(action.image_path, action.shots[0].path);
        for (i, shot) in action.shots.iter().enumerate() {
            let native = i >= 2;
            let expected = if native {
                (4, 2, vec![10 + shot.display_id as u8; 24])
            } else {
                (2, 1, vec![shot.display_id as u8; 6])
            };
            assert_eq!(decode(&shot.path), expected);
            assert_eq!(
                shot.kind,
                if native {
                    ShotKind::FullResolution
                } else {
                    ShotKind::Ring
                }
            );
            assert_eq!(shot.grab_latency_ns.is_some(), native);
            assert_eq!(
                std::fs::metadata(&shot.path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(
            std::fs::metadata(t.path()).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    /// Bug note 2026-10-25T21-31-53: a grab seconds after "look at the screen" (the final's decision) is kept beside
    /// the phrase-time frame, which comes first, and the note names both moments. A grab at the phrase replaces it.
    #[test]
    fn a_late_full_resolution_grab_joins_the_phrase_time_frame_instead_of_replacing_it() {
        let (b, m, anchor) = setup();
        let t = tempfile::tempdir().unwrap();
        let late = anchor + 3 * crate::FULL_RESOLUTION_SAME_MOMENT_NS;
        let action = attach_command_with_grabber(&b, &m, late, t.path(), |id| {
            let mut f = frame(id, 4, 2, 10 + id as u8);
            f.host_time_ns = late;
            Ok(f)
        })
        .unwrap();
        assert_eq!(action.shots.iter().map(|s| s.kind).collect::<Vec<_>>(),
            [ShotKind::Ring, ShotKind::Ring, ShotKind::FullResolution, ShotKind::FullResolution]);
        assert!(action.shots[..2].iter().all(|s| s.frame_ns == anchor), "the phrase-time frames come first");
        assert!(action.shots[2..].iter().all(|s| crate::full_resolution_is_late(s, m.trigger_ns)));
        assert!(action.note.starts_with("[screenshot ") && action.note.contains(" attached, and a sharper one from "), "{}", action.note);
        let (b, m, now) = setup();
        // At the phrase: the grab's own clock is the phrase's, however long the setup's commits took on a loaded Mac.
        let at_once = attach_command_with_grabber(&b, &m, now, t.path(), |id| {
            let mut f = frame(id, 4, 2, 1);
            f.host_time_ns = now;
            Ok(f)
        })
        .unwrap();
        assert!(at_once.shots.iter().all(|s| !crate::full_resolution_is_late(s, m.trigger_ns)));
        assert!(at_once.note.ends_with(" attached]"), "{}", at_once.note);
    }
    #[test]
    fn full_resolution_failure_keeps_both_ring_frames_and_other_display() {
        let (b, m, now) = setup();
        let t = tempfile::tempdir().unwrap();
        let a = attach_command_with_grabber(&b, &m, now, t.path(), |id| {
            if id == 1 {
                anyhow::bail!("Screen Recording denied")
            }
            Ok(frame(id, 4, 2, 9))
        })
        .unwrap();
        assert_eq!(a.shots.len(), 3);
        assert_eq!(a.shots[0].kind, ShotKind::Ring);
        assert_eq!(a.shots[1].kind, ShotKind::Ring);
        assert!(a.full_resolution_errors[0].contains("Screen Recording denied"));
        assert_eq!(std::fs::read_dir(t.path()).unwrap().count(), 3);
    }
    #[test]
    fn generation_change_or_blank_discards_fullres_without_dropping_ring() {
        for blank in [false, true] {
            let (b, m, now) = setup();
            let t = tempfile::tempdir().unwrap();
            let a = attach_command_with_grabber(&b, &m, now, t.path(), |id| {
                if id == 2 {
                    let mut b = b.lock().unwrap();
                    if blank {
                        b.invalidate_display(2);
                    } else {
                        b.set_displays(vec![]);
                    }
                }
                Ok(frame(id, 4, 2, 9))
            })
            .unwrap();
            assert_eq!(a.shots.len(), 2);
            assert!(a.shots.iter().all(|s| s.kind == ShotKind::Ring));
            assert!(a
                .full_resolution_errors
                .iter()
                .any(|s| s.contains("generation changed")));
            assert_eq!(std::fs::read_dir(t.path()).unwrap().count(), 2);
        }
    }
    #[test]
    fn supplemental_png_failure_removes_only_failed_file() {
        let (b, m, now) = setup();
        let t = tempfile::tempdir().unwrap();
        let a = attach_command_with_grabber(&b, &m, now, t.path(), |id| {
            let mut f = frame(id, 4, 2, 9);
            f.rgb = vec![0; 1].into(); // Valid native acquisition, broken encoder input.
            Ok(f)
        })
        .unwrap();
        assert_eq!(a.shots.len(), 2);
        assert_eq!(a.full_resolution_errors.len(), 2);
        assert_eq!(std::fs::read_dir(t.path()).unwrap().count(), 2);
    }
    #[test]
    fn wrong_display_and_stale_native_pixels_are_rejected() {
        let (b, m, now) = setup();
        let t = tempfile::tempdir().unwrap();
        let a = attach_command_with_grabber(&b, &m, now, t.path(), |id| {
            let mut f = frame(if id == 1 { 99 } else { id }, 4, 2, 9);
            if id == 2 {
                f.host_time_ns = 0;
            }
            Ok(f)
        })
        .unwrap();
        assert_eq!(a.shots.len(), 2);
        assert_eq!(a.full_resolution_errors.len(), 2);
    }
    #[test]
    fn unavailable_ring_does_not_start_native_capture() {
        let b = Arc::new(Mutex::new(ScreenBuffer::new(Default::default()).unwrap()));
        let (_, m, now) = setup();
        let t = tempfile::tempdir().unwrap();
        assert!(
            attach_command_with_grabber(&b, &m, now, t.path(), |_| panic!(
                "must not capture without ring"
            ))
            .is_err()
        );
    }
}
