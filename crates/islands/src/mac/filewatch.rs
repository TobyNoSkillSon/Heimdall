//! Watches one file while a viewer shows it: the file itself (written in place) and its folder
//! (replaced by rename, deleted, created again). Events are coalesced over 150 ms into one `on_change`, which runs only
//! when the file itself changed (`Stamp`): a sibling written in the same folder wakes the watch but reads nothing. No
//! polling.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};

use super::sys::{self, Source};

pub struct FileWatch {
    pub path: PathBuf,
    file: RefCell<Option<Source>>,
    dir: RefCell<Option<Source>>,
    queued: Cell<bool>,
    /// The file as `on_change` last saw it.
    seen: Cell<Option<Stamp>>,
    on_change: Box<dyn Fn()>,
    me: RefCell<Weak<FileWatch>>,
}

/// A file's identity and version as `stat` sees it (inode, size, modification time); `None` while it is missing.
/// Audit ISL-12 (14 Nov): the data root holds visibility.json, display.json and streams.json beside state.json, so every
/// state.json write woke all three watches, and each re-read and re-parsed its own unchanged file.
pub type Stamp = (u64, u64, i64, i64);

pub fn stamp(path: &Path) -> Option<Stamp> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| (m.ino(), m.size(), m.mtime(), m.mtime_nsec()))
}

impl FileWatch {
    pub fn new(path: &Path, on_change: Box<dyn Fn()>) -> Rc<FileWatch> {
        let w = Rc::new(FileWatch {
            path: path.to_path_buf(),
            file: RefCell::new(None),
            dir: RefCell::new(None),
            queued: Cell::new(false),
            seen: Cell::new(stamp(path)),
            on_change,
            me: RefCell::new(Weak::new()),
        });
        *w.me.borrow_mut() = Rc::downgrade(&w);
        if let Some(parent) = path.parent() {
            let me = Rc::downgrade(&w);
            *w.dir.borrow_mut() = Source::vnode(parent, sys::VNODE_WRITE | sys::VNODE_RENAME | sys::VNODE_DELETE, move |_| {
                if let Some(w) = me.upgrade() {
                    w.fired();
                }
            });
        }
        w.arm_file();
        w
    }

    /// The file's own watch follows its current inode (an editor's save replaces it).
    fn arm_file(&self) {
        *self.file.borrow_mut() = None;
        let me = self.me.borrow().clone();
        let mask = sys::VNODE_WRITE | sys::VNODE_EXTEND | sys::VNODE_DELETE | sys::VNODE_RENAME | sys::VNODE_ATTRIB;
        *self.file.borrow_mut() = Source::vnode(&self.path, mask, move |d| {
            super::island::debug(&format!("file event {d:#x}"));
            if let Some(w) = me.upgrade() {
                w.fired();
            }
        });
    }

    fn fired(&self) {
        super::island::debug(&format!("filewatch event {}", self.path.display()));
        if self.queued.get() {
            return;
        }
        self.queued.set(true);
        let me = self.me.borrow().clone();
        sys::after(0.15, move || {
            let Some(w) = me.upgrade() else { return };
            w.queued.set(false);
            w.arm_file();
            let now = stamp(&w.path);
            if w.seen.replace(now) == now {
                return;
            }
            super::island::debug(&format!("filewatch fired {}", w.path.display()));
            (w.on_change)();
        });
    }

    pub fn cancel(&self) {
        *self.file.borrow_mut() = None;
        *self.dir.borrow_mut() = None;
    }
}

impl Drop for FileWatch {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::stamp;

    #[test]
    fn a_sibling_write_leaves_the_stamp_and_a_replace_changes_it() {
        let dir = std::env::temp_dir().join(format!("fi-filewatch-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let (file, sibling) = (dir.join("streams.json"), dir.join("state.json"));
        assert_eq!(stamp(&file), None, "missing");
        std::fs::write(&file, b"{\"shown\": true}").unwrap();
        let first = stamp(&file);
        assert!(first.is_some());
        std::fs::write(&sibling, b"{}").unwrap();
        assert_eq!(stamp(&file), first, "a sibling's write is no change");
        let tmp = dir.join(".streams.json.tmp");
        std::fs::write(&tmp, b"{\"shown\": false}").unwrap();
        std::fs::rename(&tmp, &file).unwrap();
        assert_ne!(stamp(&file), first, "replaced by rename");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
