//! Watches one file while a viewer shows it (Swift `FileWatch`): the file itself (written in place) and its folder
//! (replaced by rename, deleted, created again). Events are coalesced over 150 ms into one `on_change`. No polling.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};

use super::sys::{self, Source};

pub struct FileWatch {
    pub path: PathBuf,
    file: RefCell<Option<Source>>,
    dir: RefCell<Option<Source>>,
    queued: Cell<bool>,
    on_change: Box<dyn Fn()>,
    me: RefCell<Weak<FileWatch>>,
}

impl FileWatch {
    pub fn new(path: &Path, on_change: Box<dyn Fn()>) -> Rc<FileWatch> {
        let w = Rc::new(FileWatch {
            path: path.to_path_buf(),
            file: RefCell::new(None),
            dir: RefCell::new(None),
            queued: Cell::new(false),
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
