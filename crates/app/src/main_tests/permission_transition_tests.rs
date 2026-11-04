    use super::*;
    #[test]
    fn steady_audio_packets_emit_no_permission_ui_updates() {
        let mut p = LivePermissions::default();
        assert!(p.audio_permission_transition("mic:1", &[0.5], 0, true).is_none());
        assert!(p.audio_permission_transition("app:Discord", &[0.5], 0, true).is_some());
        for _ in 0..10_000 {
            for source in ["mic:1", "app:Discord", "app:Chrome"] {
                assert!(p.audio_permission_transition(source, &[0.0], u64::MAX, true).is_none());
            }
        }
    }
    #[test]
    fn silent_taps_deny_only_while_a_recorded_app_plays_and_sound_grants() {
        use ui::{Permission::SystemAudio, PermissionState::*, UiUpdate::Permissions};
        let s = 1_000_000_000;
        let silence = [0.0; 320];
        let mut p = LivePermissions::default();
        // Nothing recorded is playing: silence is just silence, however long.
        assert!(p.audio_permission_transition("process:1", &silence, 0, false).is_none());
        assert!(p.audio_permission_transition("process:1", &silence, 60 * s, false).is_none());
        // A recorded app plays, its tap stays silent: denied after the window, once.
        assert!(p.audio_permission_transition("process:1", &silence, 61 * s, true).is_none());
        assert!(p.audio_permission_transition("process:1", &silence, 70 * s, true).is_none());
        assert!(matches!(p.audio_permission_transition("process:1", &silence, 71 * s, true), Some(Permissions(v)) if v == [(SystemAudio, Denied)]));
        assert!(p.audio_permission_transition("process:1", &silence, 90 * s, true).is_none());
        // The UI echoing Denied keeps the taps running so the grant can still be seen.
        p.observe(&[(SystemAudio, Denied)]);
        assert!(p.system);
        // The app stopped for a moment: the window restarts.
        let mut q = LivePermissions::default();
        q.audio_permission_transition("process:1", &silence, 0, true);
        q.audio_permission_transition("process:1", &silence, 9 * s, false);
        assert!(q.audio_permission_transition("process:1", &silence, 12 * s, true).is_none());
        // Real sound after a denial grants it and stays granted.
        let mut sound = silence; sound[100] = 0.01;
        assert!(matches!(p.audio_permission_transition("process:1", &sound, 91 * s, true), Some(Permissions(v)) if v == [(SystemAudio, Granted)]));
        assert!(p.system_confirmed);
        assert!(p.audio_permission_transition("process:1", &silence, 200 * s, true).is_none());
    }
    #[test]
    fn in_process_grants_denials_and_regrant_start_independent_capabilities() {
        let mut p = LivePermissions::default();
        let mut running = [false;4];
        let mut starts = [0;4];
        let mut reconcile = |states: &[(ui::Permission,ui::PermissionState)], on| {
            p.observe(states);
            let desired = p.desired(on);
            for i in 0..4 { if desired[i] && !running[i] { starts[i]+=1; } }
            running=desired;
            running
        };
        use ui::{Permission::*,PermissionState::*};
        assert_eq!(reconcile(&[(SystemAudio,Unconfirmed),(Microphone,Denied),(Accessibility,Denied)],true),[false,true,false,false]);
        assert_eq!(reconcile(&[(Microphone,Granted)],true),[true,true,false,false]);
        assert_eq!(reconcile(&[(ScreenRecording,Granted)],true),[true,true,true,false]);
        assert_eq!(reconcile(&[(Accessibility,Granted)],true),[true,true,true,true]);
        assert_eq!(reconcile(&[(Microphone,Denied)],true),[false,true,true,true]);
        reconcile(&[(Microphone,Granted)],true);
        reconcile(&[],true);
        assert_eq!(reconcile(&[],false),[false;4]);
        reconcile(&[],true);
        assert_eq!(starts,[3,2,2,2]);
    }
