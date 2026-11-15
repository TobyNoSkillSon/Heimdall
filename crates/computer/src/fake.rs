//! A fake Accessibility tree for tests: apps, a tree per app, action names per element, and scripted effects (a
//! right-click adds a menu; a pick ticks an item and closes the menu). Every call is logged.
use crate::{AppInfo, Backend, Node, Snapshot};
use anyhow::{bail, Result};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;

type Effect = Box<dyn Fn(&mut Node)>;

pub struct Fake {
    pub trusted: bool,
    pub apps: Vec<AppInfo>,
    pub trees: RefCell<HashMap<i32, Node>>,
    pub actions: HashMap<usize, Vec<String>>,
    effects: HashMap<(usize, String), Effect>,
    pub idle: f64,
    pub log: RefCell<Vec<String>>,
    /// Elements whose `AXSelectedText` cannot be set (the fallback appends to `AXValue`).
    pub no_selected_text: Vec<usize>,
    pub truncated: bool,
    /// Elements whose action is delivered but not confirmed in time (`AX_CANNOT_COMPLETE`).
    pub unconfirmed: Vec<usize>,
    next: Cell<usize>,
}

impl Fake {
    pub fn empty() -> Self {
        Fake {
            trusted: false,
            apps: vec![],
            trees: RefCell::new(HashMap::new()),
            actions: HashMap::new(),
            effects: HashMap::new(),
            idle: 60.0,
            log: RefCell::new(vec![]),
            no_selected_text: vec![],
            truncated: false,
            unconfirmed: vec![],
            next: Cell::new(1),
        }
    }
    pub fn new() -> Self {
        Fake { trusted: true, ..Self::empty() }
    }
    /// Adds a running app with its tree; handles are numbered in the order nodes are built.
    pub fn app(&mut self, name: &str, bundle_id: &str, pid: i32, root: Node) -> &mut Self {
        self.apps.push(AppInfo { name: name.into(), bundle_id: bundle_id.into(), pid, active: self.apps.is_empty() });
        self.trees.borrow_mut().insert(pid, root);
        self
    }
    pub fn on(&mut self, handle: usize, action: &str, effect: impl Fn(&mut Node) + 'static) -> &mut Self {
        self.effects.insert((handle, action.into()), Box::new(effect));
        self
    }
    pub fn give(&mut self, handle: usize, actions: &[&str]) -> &mut Self {
        self.actions.insert(handle, actions.iter().map(|s| s.to_string()).collect());
        self
    }
    /// A fresh handle for a node built in a test.
    pub fn handle(&self) -> usize {
        let h = self.next.get();
        self.next.set(h + 1);
        h
    }
    pub fn calls(&self) -> Vec<String> {
        self.log.borrow().clone()
    }
    fn with_node<T>(&self, handle: usize, f: impl FnOnce(&mut Node) -> T) -> Option<T> {
        fn find(n: &mut Node, h: usize) -> Option<&mut Node> {
            if n.handle == h {
                return Some(n);
            }
            n.children.iter_mut().find_map(|c| find(c, h))
        }
        let mut trees = self.trees.borrow_mut();
        let found = trees.values_mut().find_map(|t| find(t, handle)).map(f);
        found
    }
    /// Runs the scripted effect on the tree that holds the element.
    fn apply(&self, handle: usize, action: &str) {
        fn holds(n: &Node, h: usize) -> bool {
            n.handle == h || n.children.iter().any(|c| holds(c, h))
        }
        if let Some(effect) = self.effects.get(&(handle, action.to_owned())) {
            if let Some(tree) = self.trees.borrow_mut().values_mut().find(|t| holds(t, handle)) {
                effect(tree);
            }
        }
    }
}
impl Default for Fake {
    fn default() -> Self {
        Self::new()
    }
}

impl Backend for Fake {
    fn trusted(&self) -> bool {
        self.trusted
    }
    fn apps(&self) -> Vec<AppInfo> {
        self.apps.clone()
    }
    fn snapshot(&self, pid: i32) -> Result<Snapshot> {
        self.log.borrow_mut().push(format!("snapshot {pid}"));
        match self.trees.borrow().get(&pid) {
            Some(root) => Ok(Snapshot { root: root.clone(), truncated: self.truncated }),
            None => bail!("AX_FAILED: no tree for {pid}"),
        }
    }
    fn actions(&self, handle: usize) -> Vec<String> {
        self.actions.get(&handle).cloned().unwrap_or_default()
    }
    fn perform(&self, handle: usize, action: &str) -> Result<()> {
        self.log.borrow_mut().push(format!("{action} {handle}"));
        if !self.actions(handle).iter().any(|a| a == action) {
            bail!("AX_FAILED: {action} is not supported by element {handle}");
        }
        self.apply(handle, action);
        if self.unconfirmed.contains(&handle) {
            bail!("AX_CANNOT_COMPLETE: the app did not confirm {action} in time");
        }
        Ok(())
    }
    fn set_text(&self, handle: usize, attribute: &str, text: &str) -> Result<()> {
        self.log.borrow_mut().push(format!("set {attribute} {handle} {text}"));
        if attribute == "AXSelectedText" && self.no_selected_text.contains(&handle) {
            bail!("AX_FAILED: attribute unsupported");
        }
        self.with_node(handle, |n| {
            if attribute == "AXSelectedText" {
                n.value.push_str(text)
            } else {
                n.value = text.to_owned()
            }
        })
        .ok_or_else(|| anyhow::anyhow!("AX_FAILED: element {handle} is gone"))
    }
    fn focus(&self, handle: usize) -> Result<()> {
        self.log.borrow_mut().push(format!("focus {handle}"));
        for tree in self.trees.borrow_mut().values_mut() {
            fn clear(n: &mut Node) {
                n.focused = false;
                n.children.iter_mut().for_each(clear);
            }
            clear(tree);
        }
        self.with_node(handle, |n| n.focused = true).ok_or_else(|| anyhow::anyhow!("AX_FAILED: element {handle} is gone"))
    }
    fn reread(&self, handle: usize) -> Option<Node> {
        self.with_node(handle, |n| Node { children: vec![], ..n.clone() })
    }
    fn focused(&self, pid: i32) -> Option<Node> {
        fn find(n: &Node) -> Option<Node> {
            if n.focused {
                return Some(Node { children: vec![], ..n.clone() });
            }
            n.children.iter().find_map(find)
        }
        self.trees.borrow().get(&pid).and_then(find)
    }
    fn idle_seconds(&self) -> f64 {
        self.idle
    }
    fn click(&self, pid: i32, x: f64, y: f64, right: bool) -> Result<()> {
        self.log.borrow_mut().push(format!("click {pid} {x:.0},{y:.0}{}", if right { " right" } else { "" }));
        Ok(())
    }
    fn settle(&self, _: u64) {}
}

/// Builds a node: `el(h, "AXButton", "Mute")`.
pub fn el(handle: usize, role: &str, title: &str) -> Node {
    Node { handle, role: role.into(), title: title.into(), enabled: true, ..Node::default() }
}
impl Node {
    pub fn with(mut self, children: Vec<Node>) -> Self {
        self.children = children;
        self
    }
    pub fn at(mut self, x: f64, y: f64, w: f64, h: f64) -> Self {
        self.frame = Some((x, y, w, h));
        self
    }
    pub fn value(mut self, v: &str) -> Self {
        self.value = v.into();
        self
    }
    pub fn described(mut self, d: &str) -> Self {
        self.description = d.into();
        self
    }
    pub fn sub(mut self, s: &str) -> Self {
        self.subrole = s.into();
        self
    }
    pub fn ticked(mut self) -> Self {
        self.mark = "✓".into();
        self
    }
    pub fn disabled(mut self) -> Self {
        self.enabled = false;
        self
    }
    pub fn focus(mut self) -> Self {
        self.focused = true;
        self
    }
}
