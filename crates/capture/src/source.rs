use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SourceId(pub String);
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppOwner {
    pub name: String,
    pub bundle_id: Option<String>,
    pub path: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceKind {
    InputDevice {
        name: String,
        uid: String,
    },
    Process {
        name: String,
        bundle_id: Option<String>,
        pid: u32,
        birth_us: u64,
        owner: Option<AppOwner>,
        output_devices: Vec<String>,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeFormat {
    pub sample_rate: u32,
    pub channels: u16,
}
impl NativeFormat {
    pub fn validate(self) -> anyhow::Result<Self> {
        anyhow::ensure!(
            (8000..=384000).contains(&self.sample_rate) && (1..=32).contains(&self.channels),
            "unsupported native format {self:?}"
        );
        Ok(self)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceInfo {
    pub id: SourceId,
    pub label: String,
    pub kind: SourceKind,
    pub format: NativeFormat,
    pub generation: u64,
    pub native_encoding: String,
}
impl SourceInfo {
    pub fn new(kind: SourceKind, format: NativeFormat, generation: u64) -> Self {
        let (id, label) = match &kind {
            SourceKind::InputDevice { name, uid } => {
                (format!("mic:{uid}:{generation}"), format!("{name} [{uid}]"))
            }
            SourceKind::Process {
                name,
                bundle_id,
                pid,
                birth_us,
                owner,
                ..
            } => {
                let app = owner.as_ref().map(|o| o.name.as_str()).unwrap_or(name);
                let bundle = bundle_id.as_deref().unwrap_or("unbundled");
                (
                    format!("process:{pid}:{birth_us}:{generation}"),
                    format!("{app} / {name} [{bundle}, PID {pid}]"),
                )
            }
        };
        Self {
            id: SourceId(id),
            label,
            kind,
            format,
            generation,
            native_encoding: "Float32 little-endian".into(),
        }
    }
}
/// The app a process belongs to, as App Audio lists and records it. `key` survives relaunches:
/// the outer `.app`'s bundle ID (Chromium/Electron helpers resolve to it), else that `.app`'s path,
/// else the process's own HAL bundle ID, else its executable path for unbundled processes.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AppIdentity {
    pub key: String,
    /// Display name: the outer app's display name, else the executable's file name.
    pub name: String,
    /// The outer `.app` (for its icon), else the executable.
    pub path: String,
}
/// Identity for an emitting process. Pure, so the key rules are testable without Core Audio.
pub fn app_identity(executable: &str, bundle_id: Option<&str>, owner: Option<&AppOwner>) -> AppIdentity {
    let file = std::path::Path::new(executable)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| executable.to_owned());
    match owner {
        Some(o) => AppIdentity {
            key: o.bundle_id.clone().filter(|b| !b.is_empty()).unwrap_or_else(|| o.path.clone()),
            name: o.name.clone(),
            path: o.path.clone(),
        },
        None => AppIdentity {
            key: bundle_id.filter(|b| !b.is_empty()).map(str::to_owned).unwrap_or_else(|| executable.to_owned()),
            name: file,
            path: executable.to_owned(),
        },
    }
}
/// The outermost .app owns nested Electron/Chromium helper bundles. Do not
/// infer an owner by stripping a bundle suffix (unrelated apps can share names).
pub fn owning_app_path(executable: &str) -> Option<String> {
    let end = executable.find(".app/")? + 4;
    Some(executable[..end].to_owned())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn helper_owner_is_outer_app() {
        for app in ["Discord", "Google Chrome", "Slack"] {
            let path = format!("/Applications/{app}.app/Contents/Frameworks/{app} Helper.app/Contents/MacOS/{app} Helper");
            assert_eq!(
                owning_app_path(&path),
                Some(format!("/Applications/{app}.app"))
            );
        }
        assert_eq!(owning_app_path("/usr/bin/say"), None);
    }
    #[test]
    fn app_identity_keys_survive_relaunch_and_helpers_join_their_app() {
        let discord = AppOwner { name: "Discord".into(), bundle_id: Some("com.hnc.Discord".into()), path: "/Applications/Discord.app".into() };
        let helper = "/Applications/Discord.app/Contents/Frameworks/Discord Helper (Renderer).app/Contents/MacOS/Discord Helper (Renderer)";
        let main = "/Applications/Discord.app/Contents/MacOS/Discord";
        // Helper and main process, any PID, any launch: one key, the outer app's bundle ID and name.
        let a = app_identity(helper, Some("com.hnc.Discord.helper.Renderer"), Some(&discord));
        let b = app_identity(main, Some("com.hnc.Discord"), Some(&discord));
        assert_eq!(a, b);
        assert_eq!((a.key.as_str(), a.name.as_str(), a.path.as_str()), ("com.hnc.Discord", "Discord", "/Applications/Discord.app"));
        // An outer app without a bundle ID is keyed by its path.
        let bare = AppOwner { name: "Tool".into(), bundle_id: None, path: "/Users/t/Tool.app".into() };
        assert_eq!(app_identity("/Users/t/Tool.app/Contents/MacOS/tool", None, Some(&bare)).key, "/Users/t/Tool.app");
        // Unbundled: the executable path; bundled outside a .app (an XPC service): its bundle ID.
        let say = app_identity("/usr/bin/say", None, None);
        assert_eq!((say.key.as_str(), say.name.as_str()), ("/usr/bin/say", "say"));
        assert_eq!(app_identity("/S/WebKit.framework/XPCServices/com.apple.WebKit.GPU.xpc/Contents/MacOS/com.apple.WebKit.GPU", Some("com.apple.WebKit.GPU"), None).key, "com.apple.WebKit.GPU");
        assert_eq!(app_identity("/usr/bin/afplay", Some(""), None).key, "/usr/bin/afplay");
    }
    #[test]
    fn same_names_and_reused_pids_are_distinct() {
        let f = NativeFormat {
            sample_rate: 48000,
            channels: 2,
        };
        let a = SourceInfo::new(
            SourceKind::InputDevice {
                name: "Mic".into(),
                uid: "one".into(),
            },
            f,
            1,
        );
        let b = SourceInfo::new(
            SourceKind::InputDevice {
                name: "Mic".into(),
                uid: "two".into(),
            },
            f,
            1,
        );
        assert_ne!(a.id, b.id);
        assert!(a.label.contains("one"));
        let p = |birth_us| {
            SourceInfo::new(
                SourceKind::Process {
                    name: "Helper".into(),
                    bundle_id: Some("com.discord.helper".into()),
                    pid: 7,
                    birth_us,
                    owner: Some(AppOwner {
                        name: "Discord".into(),
                        bundle_id: Some("com.hnc.Discord".into()),
                        path: "/Applications/Discord.app".into(),
                    }),
                    output_devices: vec![],
                },
                f,
                1,
            )
        };
        assert_ne!(p(1).id, p(2).id);
        assert!(p(1).label.contains("Discord / Helper"));
    }
}
