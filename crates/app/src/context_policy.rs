//! Source routing before any harness sees an observation.
use capture::{SourceInfo, SourceKind};

/// A deliberately small exact-bundle allowlist. Owner identity joins app helpers.
pub const CALL_BUNDLES: &[&str] = &[
    "com.hnc.Discord",
    "com.hnc.DiscordPTB",
    "com.hnc.DiscordCanary",
    "com.tinyspeck.slackmacgap",
    "us.zoom.xos",
    "com.microsoft.teams2",
    "com.microsoft.teams",
    "com.apple.FaceTime",
];
pub fn is_call(source: &SourceInfo) -> bool {
    match &source.kind {
        SourceKind::Process {
            bundle_id, owner, ..
        } => {
            let bundle = owner
                .as_ref()
                .and_then(|o| o.bundle_id.as_deref())
                .or(bundle_id.as_deref());
            bundle.is_some_and(|b| CALL_BUNDLES.contains(&b))
        }
        _ => false,
    }
}
pub fn agent_context(source: &SourceInfo) -> bool {
    matches!(source.kind, SourceKind::InputDevice { .. }) || is_call(source)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn heimdalls_captured_output_stays_out_of_agent_observations() {
        let mut source=crate::fake_source("app",1,1);
        if let SourceKind::Process {bundle_id,owner,..}=&mut source.kind {
            *bundle_id=Some("dev.heimdall.app".into());*owner=None;
        }
        assert!(!agent_context(&source));
    }
    #[test]
    fn exact_owner_bundle_routes_helpers_without_name_guessing() {
        let mut s = crate::fake_source("app", 2, 1);
        assert!(!agent_context(&s));
        if let SourceKind::Process {
            name,
            bundle_id,
            owner,
            ..
        } = &mut s.kind
        {
            *name = "Discord YouTube video".into();
            *bundle_id = Some("com.google.Chrome".into());
            assert!(!CALL_BUNDLES.contains(&bundle_id.as_deref().unwrap()));
            *owner = Some(capture::AppOwner {
                name: "Slack".into(),
                bundle_id: Some("com.tinyspeck.slackmacgap".into()),
                path: "/Applications/Slack.app".into(),
            });
        }
        assert!(is_call(&s));
        assert!(agent_context(&s));
        assert!(agent_context(&crate::fake_source("mic", 0, 1)));
        assert!(is_call(&crate::fake_source("discord", 0, 1)));
    }
}
