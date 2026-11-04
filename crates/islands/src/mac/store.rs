//! Watches the islands folder with kqueue (no polling): one source on the folder (create, rename, delete) and one per
//! island file (written in place), each triggering one coalesced rescan; only files whose bytes changed are parsed.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::rc::{Rc, Weak};

use serde_json::Value;

use super::sys::{self, Source};
use crate::model::Island;

type OnChange = Box<dyn Fn(Vec<Island>, Vec<String>)>;

pub struct Store {
    dir: PathBuf,
    source: RefCell<Option<Source>>,
    files: RefCell<HashMap<String, (u64, Source)>>,
    retried: RefCell<HashSet<String>>,
    cache: RefCell<HashMap<String, Vec<u8>>>,
    scheduled: Cell<bool>,
    on_change: OnChange,
    me: RefCell<Weak<Store>>,
}

impl Store {
    pub fn new(dir: PathBuf, on_change: OnChange) -> Rc<Store> {
        let s = Rc::new(Store {
            dir,
            source: RefCell::new(None),
            files: RefCell::new(HashMap::new()),
            retried: RefCell::new(HashSet::new()),
            cache: RefCell::new(HashMap::new()),
            scheduled: Cell::new(false),
            on_change,
            me: RefCell::new(Weak::new()),
        });
        *s.me.borrow_mut() = Rc::downgrade(&s);
        s
    }

    pub fn start(&self) {
        let me = self.me.borrow().clone();
        let src = Source::vnode(&self.dir, sys::VNODE_WRITE | sys::VNODE_RENAME | sys::VNODE_DELETE, move |_| {
            if let Some(s) = me.upgrade() {
                s.schedule();
            }
        });
        if src.is_none() {
            eprintln!("FloatingIslands: cannot watch {}", self.dir.display());
            return;
        }
        *self.source.borrow_mut() = src;
        self.scan();
    }

    fn watch_file(&self, id: &str, path: &std::path::Path) {
        let Ok(m) = std::fs::metadata(path) else { return };
        let ino = m.ino();
        if self.files.borrow().get(id).is_some_and(|(i, _)| *i == ino) {
            return;
        }
        self.files.borrow_mut().remove(id);
        let me = self.me.borrow().clone();
        if let Some(src) = Source::vnode(path, sys::VNODE_WRITE | sys::VNODE_EXTEND, move |_| {
            if let Some(s) = me.upgrade() {
                s.schedule();
            }
        }) {
            self.files.borrow_mut().insert(id.to_owned(), (ino, src));
        }
    }

    fn schedule(&self) {
        if self.scheduled.get() {
            return;
        }
        self.scheduled.set(true);
        let me = self.me.borrow().clone();
        sys::after(0.012, move || {
            if let Some(s) = me.upgrade() {
                s.scheduled.set(false);
                s.scan();
            }
        });
    }

    pub fn scan(&self) {
        let mut names: Vec<String> = std::fs::read_dir(&self.dir)
            .map(|d| d.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_default();
        names.sort();
        let mut seen = HashSet::new();
        let mut upserts: Vec<(f64, String, Island)> = vec![];
        for name in names.iter().filter(|n| n.ends_with(".json") && !n.starts_with('.')) {
            let id = name[..name.len() - 5].to_owned();
            seen.insert(id.clone());
            let path = self.dir.join(name);
            self.watch_file(&id, &path);
            let Ok(data) = std::fs::read(&path) else { continue };
            if self.cache.borrow().get(&id) == Some(&data) {
                continue;
            }
            let parsed = serde_json::from_slice::<Value>(&data).ok().and_then(|v| match v {
                Value::Object(o) => Some(o),
                _ => None,
            });
            let Some(o) = parsed else {
                // Possibly caught mid-write by a writer that does not rename: look once more shortly.
                if self.retried.borrow_mut().insert(id.clone()) {
                    let me = self.me.borrow().clone();
                    sys::after(0.06, move || {
                        if let Some(s) = me.upgrade() {
                            s.scan();
                        }
                    });
                }
                continue;
            };
            self.retried.borrow_mut().remove(&id);
            let is_new = !self.cache.borrow().contains_key(&id);
            self.cache.borrow_mut().insert(id.clone(), data);
            // New islands keep the order they were created in (one scan can pick up several at once).
            let born = if is_new {
                std::fs::metadata(&path)
                    .and_then(|m| m.created())
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(f64::NEG_INFINITY)
            } else {
                f64::NEG_INFINITY
            };
            upserts.push((born, name.clone(), Island::new(&id, o)));
        }
        let removed: Vec<String> = self.cache.borrow().keys().filter(|k| !seen.contains(*k)).cloned().collect();
        for r in &removed {
            self.cache.borrow_mut().remove(r);
        }
        self.files.borrow_mut().retain(|k, _| seen.contains(k));
        upserts.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal).then(a.1.cmp(&b.1)));
        if !upserts.is_empty() || !removed.is_empty() {
            (self.on_change)(upserts.into_iter().map(|u| u.2).collect(), removed);
        }
    }
}
