//! Passive AX snapshots: AXIsProcessTrusted never asks, AXManualAccessibility is never set.
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AxNode {
    #[serde(default)]
    pub attributes: BTreeMap<String, String>,
    #[serde(default)]
    pub children: Vec<AxNode>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AxSnapshot {
    pub pid: i32,
    pub observed_at: f64,
    pub truncated: bool,
    pub root: AxNode,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Participant {
    pub name: String,
    pub speaking: Option<bool>,
}
/// Accept explicit exported speaking descriptions only. Selection/focus/green pixels are
/// never interpreted as speech. None preserves Discord builds that export names alone.
pub fn participants(root: &AxNode) -> Vec<Participant> {
    fn visit(n: &AxNode, out: &mut BTreeMap<String, Option<bool>>) {
        let a = &n.attributes;
        let description = a.get("AXDescription").map(String::as_str).unwrap_or("");
        let lower = description.to_lowercase();
        let dom = a.get("AXDOMIdentifier").map(String::as_str).unwrap_or("");
        // A speaking state is a complete comma-delimited token, not prose such
        // as "Dave, speaking about Anna".
        let described_state = lower
            .split(',')
            .skip(1)
            .find_map(|token| match token.trim() {
                "speaking" => Some(true),
                "not speaking" => Some(false),
                _ => None,
            });
        let explicit = described_state.is_some();
        let row = dom.starts_with("voice-user-") || dom.starts_with("participant-") || explicit;
        if row {
            let name = a
                .get("AXTitle")
                .filter(|s| !s.trim().is_empty())
                .cloned()
                .or_else(|| {
                    description
                        .split(',')
                        .next()
                        .filter(|s| !s.trim().is_empty())
                        .map(|s| s.trim().to_string())
                });
            if let Some(name) = name {
                let speaking = match a.get("AXSpeaking").map(|s| s.to_lowercase()).as_deref() {
                    Some("true" | "1") => Some(true),
                    Some("false" | "0") => Some(false),
                    _ => described_state,
                };
                out.entry(name)
                    .and_modify(|v| {
                        if speaking.is_some() {
                            *v = speaking;
                        }
                    })
                    .or_insert(speaking);
            }
        }
        for c in &n.children {
            visit(c, out);
        }
    }
    let mut out = BTreeMap::new();
    visit(root, &mut out);
    out.into_iter()
        .map(|(name, speaking)| Participant { name, speaking })
        .collect()
}

#[cfg(target_os = "macos")]
mod native {
    use super::*;
    use std::{
        ffi::{CString, c_char, c_void},
        time::{Duration, Instant},
    };
    type CF = *const c_void;
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXIsProcessTrusted() -> bool;
        fn AXUIElementCreateApplication(pid: i32) -> CF;
        fn AXUIElementCopyAttributeValue(element: CF, key: CF, value: *mut CF) -> i32;
        fn AXUIElementCopyAttributeNames(element: CF, names: *mut CF) -> i32;
        fn AXUIElementSetMessagingTimeout(element: CF, seconds: f32) -> i32;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRelease(value: CF);
        fn CFGetTypeID(value: CF) -> usize;
        fn CFStringGetTypeID() -> usize;
        fn CFArrayGetTypeID() -> usize;
        fn CFBooleanGetTypeID() -> usize;
        fn CFNumberGetTypeID() -> usize;
        fn CFBooleanGetValue(value: CF) -> bool;
        fn CFNumberGetValue(value: CF, kind: i32, out: *mut c_void) -> bool;
        fn CFStringCreateWithCString(allocator: CF, text: *const c_char, encoding: u32) -> CF;
        fn CFStringGetLength(value: CF) -> isize;
        fn CFStringGetMaximumSizeForEncoding(length: isize, encoding: u32) -> isize;
        fn CFStringGetCString(value: CF, buffer: *mut c_char, size: isize, encoding: u32) -> bool;
        fn CFArrayGetCount(value: CF) -> isize;
        fn CFArrayGetValueAtIndex(value: CF, index: isize) -> CF;
    }
    struct Owned(CF);
    impl Drop for Owned {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { CFRelease(self.0) }
            }
        }
    }
    const UTF8: u32 = 0x08000100;
    fn string(value: CF) -> Option<String> {
        if value.is_null() || unsafe { CFGetTypeID(value) != CFStringGetTypeID() } {
            return None;
        }
        let cap = unsafe { CFStringGetMaximumSizeForEncoding(CFStringGetLength(value), UTF8) } + 1;
        if !(1..=65536).contains(&cap) {
            return None;
        }
        let mut bytes = vec![0u8; cap as usize];
        if !unsafe { CFStringGetCString(value, bytes.as_mut_ptr().cast(), cap, UTF8) } {
            return None;
        }
        let n = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
        String::from_utf8(bytes[..n].to_vec()).ok()
    }
    fn scalar(value: CF) -> Option<String> {
        if value.is_null() {
            return None;
        }
        let ty = unsafe { CFGetTypeID(value) };
        if ty == unsafe { CFStringGetTypeID() } {
            return string(value);
        }
        if ty == unsafe { CFBooleanGetTypeID() } {
            return Some(unsafe { CFBooleanGetValue(value) }.to_string());
        }
        if ty == unsafe { CFNumberGetTypeID() } {
            let mut number = 0f64;
            if unsafe { CFNumberGetValue(value, 6, (&mut number as *mut f64).cast()) } {
                return Some(number.to_string());
            }
        }
        None
    }
    fn copy(element: CF, key: &str, incomplete: &mut bool) -> Option<Owned> {
        let k = CString::new(key).ok()?;
        let key = Owned(unsafe { CFStringCreateWithCString(std::ptr::null(), k.as_ptr(), UTF8) });
        let mut out = std::ptr::null();
        let status = unsafe { AXUIElementCopyAttributeValue(element, key.0, &mut out) };
        if status != 0 || out.is_null() {
            // Missing optional values/leaf children are normal. IPC failures,
            // including a timeout, must never masquerade as a complete tree.
            if status != -25205 && status != -25212 {
                *incomplete = true;
            }
            return None;
        }
        Some(Owned(out))
    }
    struct Budget {
        count: usize,
        deadline: Instant,
        truncated: bool,
    }
    fn walk(element: CF, depth: usize, budget: &mut Budget) -> AxNode {
        if depth > 20 || budget.count >= 1500 || Instant::now() >= budget.deadline {
            budget.truncated = true;
            return AxNode::default();
        }
        budget.count += 1;
        let mut node = AxNode::default();
        let mut raw = std::ptr::null();
        if unsafe { AXUIElementCopyAttributeNames(element, &mut raw) } == 0 && !raw.is_null() {
            let names = Owned(raw);
            if unsafe { CFGetTypeID(raw) == CFArrayGetTypeID() } {
                if unsafe { CFArrayGetCount(names.0) } > 64 {
                    budget.truncated = true;
                }
                for i in 0..unsafe { CFArrayGetCount(names.0) }.min(64) {
                    if Instant::now() >= budget.deadline {
                        budget.truncated = true;
                        break;
                    }
                    if let Some(key) = string(unsafe { CFArrayGetValueAtIndex(names.0, i) }) {
                        if let Some(value) =
                            copy(element, &key, &mut budget.truncated).and_then(|v| scalar(v.0))
                        {
                            node.attributes.insert(key, value);
                        }
                    }
                }
            } else {
                budget.truncated = true;
            }
        } else {
            budget.truncated = true;
        }
        if let Some(children) = copy(element, "AXChildren", &mut budget.truncated) {
            if unsafe { CFGetTypeID(children.0) == CFArrayGetTypeID() } {
                for i in 0..unsafe { CFArrayGetCount(children.0) } {
                    if budget.count >= 1500 || Instant::now() >= budget.deadline {
                        budget.truncated = true;
                        break;
                    }
                    node.children.push(walk(
                        unsafe { CFArrayGetValueAtIndex(children.0, i) },
                        depth + 1,
                        budget,
                    ));
                }
            } else {
                budget.truncated = true;
            }
        }
        node
    }
    pub fn is_trusted() -> bool {
        unsafe { AXIsProcessTrusted() }
    }
    pub fn snapshot(pid: i32, observed_at: f64) -> Result<AxSnapshot> {
        if !is_trusted() {
            bail!("Accessibility is not granted; no prompt was requested");
        }
        if pid <= 0 || !observed_at.is_finite() {
            bail!("invalid AX snapshot input");
        }
        let app = Owned(unsafe { AXUIElementCreateApplication(pid) });
        if app.0.is_null() {
            bail!("cannot create AX application");
        }
        unsafe {
            AXUIElementSetMessagingTimeout(app.0, 0.15);
        }
        let mut budget = Budget {
            count: 0,
            deadline: Instant::now() + Duration::from_secs(3),
            truncated: false,
        };
        let root = walk(app.0, 0, &mut budget);
        Ok(AxSnapshot {
            pid,
            observed_at,
            truncated: budget.truncated,
            root,
        })
    }
    pub fn discord_processes() -> Vec<i32> {
        objc2::rc::autoreleasepool(|_| {
            use objc2_app_kit::NSWorkspace;
            NSWorkspace::sharedWorkspace()
                .runningApplications()
                .iter()
                .filter_map(|app| {
                    let id = app.bundleIdentifier()?.to_string();
                    (id == "com.hnc.Discord"
                        || id.starts_with("com.hnc.Discord.")
                        || id.starts_with("com.discord."))
                    .then(|| app.processIdentifier())
                })
                .collect()
        })
    }
}
#[cfg(target_os = "macos")]
pub use native::{discord_processes, is_trusted, snapshot};
#[cfg(not(target_os = "macos"))]
pub fn is_trusted() -> bool {
    false
}
#[cfg(not(target_os = "macos"))]
pub fn discord_processes() -> Vec<i32> {
    vec![]
}
#[cfg(not(target_os = "macos"))]
pub fn snapshot(_: i32, _: f64) -> Result<AxSnapshot> {
    bail!("AX requires macOS")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn speaking_prose_is_not_a_state() {
        let node = AxNode {
            attributes: BTreeMap::from([
                ("AXDOMIdentifier".into(), "voice-user-1".into()),
                ("AXTitle".into(), "Dave".into()),
                ("AXDescription".into(), "Dave, speaking about Anna".into()),
            ]),
            children: vec![],
        };
        assert_eq!(
            participants(&node),
            vec![Participant {
                name: "Dave".into(),
                speaking: None,
            }]
        );
    }
    #[test]
    fn fixture_preserves_unknown_speaking_state() {
        let tree: AxNode =
            serde_json::from_str(include_str!("../tests/fixtures/discord-ax.json")).unwrap();
        assert_eq!(
            participants(&tree),
            vec![
                Participant {
                    name: "Anna".into(),
                    speaking: Some(false)
                },
                Participant {
                    name: "Dave".into(),
                    speaking: Some(true)
                },
                Participant {
                    name: "Zoë".into(),
                    speaking: None
                }
            ]
        );
    }
    #[test]
    fn selected_is_not_speaking() {
        let tree:AxNode=serde_json::from_str(r#"{"attributes":{"AXDOMIdentifier":"voice-user-5","AXTitle":"Dave","AXSelected":"true"}}"#).unwrap();
        assert_eq!(participants(&tree)[0].speaking, None);
    }
}
