//! What one island reads of the others (`docs/one-system.md`): Goals lights a goal's work blue only while a job island
//! of it runs; Scheduled, Settled and the job islands name a goal by its symbol and title from Goals. Read when a body
//! is built; `sig` says when a rebuild is due because something elsewhere changed (`App::apply`). Outside the running
//! app (unit tests) the context is empty, so nothing reads the user's folder.

use std::collections::HashMap;

use crate::model::onesys::{self, GoalRef};
use crate::model::{Island, IslandFile, Json, Paths};

#[derive(Clone, Debug, Default)]
pub struct Ctx {
    pub goals: Vec<GoalRef>,
    /// Running job islands per goal id.
    pub running: HashMap<String, usize>,
}

impl Ctx {
    /// The running app's context: Goals' file (its island when shown, else the file) and every job island on hand.
    pub fn current() -> Ctx {
        let Some(a) = super::app::try_app() else { return Ctx::default() };
        let all: Vec<Island> = a.all().iter().map(|c| c.island()).collect();
        let goals_raw: Json = all.iter().find(|i| i.id == "goals").map(|i| i.raw.clone()).or_else(|| IslandFile::read(&Paths::file("goals"))).unwrap_or_default();
        Ctx { goals: onesys::goal_refs(&goals_raw), running: onesys::running_by_goal(all.iter()) }
    }

    pub fn goal(&self, id: &str) -> Option<&GoalRef> {
        self.goals.iter().find(|g| g.id == id)
    }

    /// A row's symbol: its own `icon`, else its goal's, else the plan symbol rule from its title.
    pub fn symbol(&self, icon: Option<&str>, goal: Option<&str>, title: &str) -> String {
        if let Some(i) = icon.filter(|i| super::look::symbol_exists(i)) {
            return i.to_owned();
        }
        if let Some(g) = goal.and_then(|g| self.goal(g)).filter(|g| super::look::symbol_exists(&g.symbol)) {
            return g.symbol.clone();
        }
        let it = crate::model::PlanItem { index: 0, key: String::new(), id: None, title: title.to_owned(), group: None, done: false, when: None, due: None, detail: None, proposal: None, chosen: None, icon: None, live: Default::default(), at: None, goal: None };
        Some(it.symbol("scheduled")).filter(|s| super::look::symbol_exists(s)).unwrap_or_else(|| "circle".into())
    }

    /// What of the context an island draws: Goals the running counts, the rest the goals' names and symbols.
    pub fn sig(&self, i: &Island) -> String {
        match i.list().as_deref() {
            Some("goals") => {
                let mut r: Vec<_> = self.running.iter().map(|(k, v)| format!("{k}={v}")).collect();
                r.sort();
                r.join(",")
            }
            Some("scheduled") | Some("settled") => self.goals_sig(),
            Some(_) => String::new(),
            None if onesys::job_goal(i).is_some() && onesys::sent(i).is_some() => self.goals_sig(),
            None => String::new(),
        }
    }

    fn goals_sig(&self) -> String {
        self.goals.iter().map(|g| format!("{}|{}|{}", g.id, g.title, g.symbol)).collect::<Vec<_>>().join(",")
    }
}
