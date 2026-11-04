//! One scheduled main-queue drain, with only the latest pending value per state field.
use crate::model::*;
use std::collections::{HashMap, VecDeque};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Key {
    Listen,
    Engine,
    Component(String),
    Sources,
    Agent,
    Permission(Permission),
    Settings,
    RuntimeSettings,
    Microphones,
    AppAudio,
    AgentSession,
    BugNotes,
}
fn key(update: &UiUpdate) -> Option<Key> {
    match update {
        UiUpdate::Listen(_) => Some(Key::Listen),
        UiUpdate::Engine { .. } => Some(Key::Engine),
        UiUpdate::ComponentStatus { component, .. } => Some(Key::Component(component.clone())),
        UiUpdate::Sources(_) => Some(Key::Sources),
        UiUpdate::Agent(_) => Some(Key::Agent),
        UiUpdate::Permissions(states) if states.len() == 1 => Some(Key::Permission(states[0].0)),
        UiUpdate::Settings(_) => Some(Key::Settings),
        UiUpdate::RuntimeSettings(_) => Some(Key::RuntimeSettings),
        UiUpdate::Microphones(_) => Some(Key::Microphones),
        UiUpdate::AppAudio(_) => Some(Key::AppAudio),
        UiUpdate::AgentSession(_) => Some(Key::AgentSession),
        UiUpdate::BugNotes(_) => Some(Key::BugNotes),
        _ => None,
    }
}

#[derive(Default)]
pub(crate) struct UpdateQueue {
    latest: HashMap<Key, UiUpdate>,
    pending: VecDeque<UiUpdate>,
    scheduled: bool,
}
impl UpdateQueue {
    /// True only when a new main-queue drain must be scheduled.
    pub(crate) fn push(&mut self, update: UiUpdate) -> bool {
        if let UiUpdate::Permissions(states) = update {
            for state in states {
                self.push_one(UiUpdate::Permissions(vec![state]));
            }
        } else {
            self.push_one(update);
        }
        if self.scheduled || self.pending.is_empty() {
            return false;
        }
        self.scheduled = true;
        true
    }
    fn push_one(&mut self, update: UiUpdate) {
        if let Some(k) = key(&update) {
            if self.latest.get(&k) == Some(&update) {
                return;
            }
            self.latest.insert(k.clone(), update.clone());
            self.pending.retain(|old| key(old).as_ref() != Some(&k));
        }
        self.pending.push_back(update);
    }
    /// Native TCC observation changes the same state as app-originated updates.
    pub(crate) fn observe_permission(&mut self, p: Permission, s: PermissionState) {
        let k = Key::Permission(p);
        self.pending.retain(|old| key(old).as_ref() != Some(&k));
        self.latest
            .insert(k, UiUpdate::Permissions(vec![(p, s)]));
    }
    /// A Listening tile click shows its clicked state until the app's next `Listen`, even one equal to the last
    /// (the app confirming nothing changed): that one must not be dropped as a repeat, or the tile stays stuck.
    pub(crate) fn expect_listen(&mut self) {
        self.latest.remove(&Key::Listen);
    }
    pub(crate) fn drain(&mut self) -> VecDeque<UiUpdate> {
        self.scheduled = false;
        std::mem::take(&mut self.pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn states() -> Vec<UiUpdate> {
        vec![
            UiUpdate::Listen(ListenState::Listening),
            UiUpdate::ComponentStatus {
                component: "capture".into(),
                message: None,
            },
            UiUpdate::Permissions(vec![(Permission::SystemAudio, PermissionState::Granted)]),
        ]
    }
    #[test]
    fn steady_state_schedules_zero_ui_updates() {
        let mut q = UpdateQueue::default();
        for update in states() {
            q.push(update);
        }
        assert_eq!(q.drain().len(), 3);
        for _ in 0..10_000 {
            for update in states() {
                assert!(!q.push(update));
            }
        }
        assert!(q.drain().is_empty());
    }
    #[test]
    fn blocked_main_queue_coalesces_transitions_and_merges_permission_patches() {
        let mut q = UpdateQueue::default();
        assert!(q.push(UiUpdate::Listen(ListenState::Paused)));
        for _ in 0..10_000 {
            assert!(!q.push(UiUpdate::Listen(ListenState::Listening)));
            assert!(!q.push(UiUpdate::Listen(ListenState::Paused)));
        }
        q.push(UiUpdate::Permissions(vec![(
            Permission::Microphone,
            PermissionState::Granted,
        )]));
        q.push(UiUpdate::Permissions(vec![(
            Permission::SystemAudio,
            PermissionState::Granted,
        )]));
        let pending = q.drain();
        assert_eq!(pending.len(), 3);
        assert_eq!(pending[0], UiUpdate::Listen(ListenState::Paused));
    }
    #[test]
    fn native_revocation_does_not_hide_later_grant_and_events_are_lossless() {
        let mut q = UpdateQueue::default();
        let grant = UiUpdate::Permissions(vec![(Permission::Microphone, PermissionState::Granted)]);
        q.push(grant.clone());
        q.observe_permission(Permission::Microphone, PermissionState::Denied);
        assert!(q.drain().is_empty());
        assert!(q.push(grant));
        let event = UiUpdate::Transcript(TranscriptLine {
            id: 1,
            label: None,
            segments: vec![],
            is_final: false,
        });
        for _ in 0..100 {
            q.push(event.clone());
        }
        assert_eq!(q.drain().len(), 101);
    }
    #[test]
    fn a_listening_click_lets_the_unchanged_state_through_once() {
        let mut q = UpdateQueue::default();
        q.push(UiUpdate::Listen(ListenState::Listening));
        q.drain();
        // Steady state: repeats are dropped.
        assert!(!q.push(UiUpdate::Listen(ListenState::Listening)));
        assert!(q.drain().is_empty());
        // The user clicked Listening off; the app could not stop and confirms Listening: the tile must hear it.
        q.expect_listen();
        assert!(q.push(UiUpdate::Listen(ListenState::Listening)));
        assert_eq!(q.drain().len(), 1);
        assert!(!q.push(UiUpdate::Listen(ListenState::Listening)));
    }
}
