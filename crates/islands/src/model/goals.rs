//! The Goals list (`islands/goals.json`, preset "todo", list "goals"):, "only the major jobs… something a
//! week maybe a month ahead, and we slowly work towards it, and whenever we accomplish some task it just gets hidden
//! under the collapse… and each task accumulates below it"; 16:03, "goals are self-motivating". Its `items` are the
//! asks (what needs the user now, the To do item shape plus `goal`); its `goals` are the goals, each with its tasks
//! (`done` | `now` | `next`). Progress is drawn from the tasks; there is never a due date or an overdue colour.

use serde_json::Value;

use super::due::Due;
use super::plan::{item_id, PlanItem, PLAN_SYMBOLS};
use super::{j, Json};

/// A goal's state: active (file order, on top), paused (dimmed, under the active ones), done (sunk to the bottom).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GoalState {
    Active,
    Paused,
    Done,
}

impl GoalState {
    pub fn parse(s: Option<&str>) -> GoalState {
        match s.map(str::to_lowercase).as_deref() {
            Some("done" | "finished" | "reached") => GoalState::Done,
            Some("paused" | "parked" | "later") => GoalState::Paused,
            _ => GoalState::Active,
        }
    }
}

/// A task's state: done (under the collapse), now (the lit line), next (counted in progress, not drawn).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskState {
    Done,
    Now,
    Next,
}

impl TaskState {
    pub fn parse(s: Option<&str>) -> TaskState {
        match s.map(str::to_lowercase).as_deref() {
            Some("done" | "finished" | "ok") => TaskState::Done,
            Some("now" | "current" | "running" | "active") => TaskState::Now,
            _ => TaskState::Next,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct GoalTask {
    /// Stable key for diffing rows: the task's id, else its title (made unique with "#2").
    pub key: String,
    pub title: String,
    pub state: TaskState,
    /// When it was finished (epoch s), if known.
    pub at: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Goal {
    /// Place in the file's `goals` array.
    pub index: usize,
    /// Stable key: the goal's id, else its title.
    pub key: String,
    pub id: Option<String>,
    pub title: String,
    pub icon: Option<String>,
    pub state: GoalState,
    /// The latest move, one line, present tense.
    pub now: Option<String>,
    /// Epoch s of the latest move.
    pub moved: Option<f64>,
    pub tasks: Vec<GoalTask>,
    /// The finished tasks are shown (the app writes it on a click on "N done").
    pub expanded: bool,
}

/// How recent a move must be for the quiet accent (the bar's leading edge lit): within the last day.
pub const RECENT: f64 = 86400.0;

impl Goal {
    /// Done tasks and all known tasks; `None` when the goal has no tasks yet (no bar, never an empty one).
    pub fn progress(&self) -> Option<(usize, usize)> {
        if self.tasks.is_empty() {
            return None;
        }
        Some((self.tasks.iter().filter(|t| t.state == TaskState::Done).count(), self.tasks.len()))
    }

    /// The share done, 0 to 1 (a done goal is whole whatever its tasks say).
    pub fn share(&self) -> Option<f64> {
        if self.state == GoalState::Done {
            return Some(1.0);
        }
        self.progress().map(|(d, n)| d as f64 / n as f64)
    }

    /// Moved within the last day: the bar's leading edge is lit. Motivation only; nothing ever reads as late.
    pub fn moved_recently(&self, now: f64) -> bool {
        self.state != GoalState::Done && self.moved.is_some_and(|m| m <= now + 60.0 && now - m < RECENT)
    }

    /// The finished tasks in the order they were finished (oldest first, so each one accumulates below the last);
    /// tasks without `at` keep their file order after the dated ones.
    pub fn done_tasks(&self) -> Vec<&GoalTask> {
        let mut d: Vec<(usize, &GoalTask)> = self.tasks.iter().filter(|t| t.state == TaskState::Done).enumerate().collect();
        d.sort_by(|(ia, a), (ib, b)| match (a.at, b.at) {
            (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal).then(ia.cmp(ib)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => ia.cmp(ib),
        });
        d.into_iter().map(|p| p.1).collect()
    }

    /// The task(s) in hand: the lit line under the goal.
    pub fn current(&self) -> Vec<&GoalTask> {
        self.tasks.iter().filter(|t| t.state == TaskState::Now).collect()
    }

    /// The goal's symbol: its own `icon`, else a project word in its title (`PLAN_SYMBOLS`), else a flag.
    pub fn symbol(&self) -> String {
        if let Some(i) = &self.icon {
            return i.clone();
        }
        let words: Vec<String> = self.title.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_owned).collect();
        PLAN_SYMBOLS
            .iter()
            .find(|(k, _)| words.iter().any(|w| w == k || w.strip_suffix('s') == Some(*k)))
            .map(|p| p.1.to_owned())
            .unwrap_or_else(|| "flag".into())
    }
}

pub struct Goals;
impl Goals {
    /// The goals in display order: active ones in file order, then paused, then done (they sink to the bottom).
    pub fn parse(raw: &Json) -> Vec<Goal> {
        let mut out = vec![];
        let mut seen = std::collections::HashSet::new();
        let Some(a) = j::arr(raw.get("goals")) else { return out };
        for (k, v) in a.iter().enumerate() {
            let Value::Object(o) = v else { continue };
            let Some(title) = j::nonempty(o.get("title")).map(str::trim).filter(|t| !t.is_empty()) else { continue };
            let id = item_id(o);
            let mut key = id.clone().unwrap_or_else(|| title.to_owned());
            let mut n = 2;
            while !seen.insert(key.clone()) {
                key = format!("{}#{n}", id.as_deref().unwrap_or(title));
                n += 1;
            }
            let mut tseen = std::collections::HashSet::new();
            let tasks = j::arr(o.get("tasks"))
                .map(|ts| {
                    ts.iter()
                        .filter_map(|t| {
                            let (title, state, at, tid) = match t {
                                Value::String(s) => (s.trim().to_owned(), TaskState::Next, None, None),
                                Value::Object(t) => (
                                    j::nonempty(t.get("title"))?.trim().to_owned(),
                                    TaskState::parse(j::str(t.get("state"))),
                                    Due::seconds(t.get("at")),
                                    item_id(t),
                                ),
                                _ => return None,
                            };
                            if title.is_empty() {
                                return None;
                            }
                            let base = tid.unwrap_or_else(|| title.clone());
                            let mut key = base.clone();
                            let mut n = 2;
                            while !tseen.insert(key.clone()) {
                                key = format!("{base}#{n}");
                                n += 1;
                            }
                            Some(GoalTask { key, title, state, at })
                        })
                        .collect()
                })
                .unwrap_or_default();
            out.push(Goal {
                index: k,
                key,
                id,
                title: title.to_owned(),
                icon: j::nonempty(o.get("icon")).map(str::to_owned),
                state: GoalState::parse(j::str(o.get("state"))),
                now: j::nonempty(o.get("now")).map(|s| s.split_whitespace().collect::<Vec<_>>().join(" ")).filter(|s| !s.is_empty()),
                moved: Due::seconds(o.get("moved")),
                tasks,
                expanded: j::boolean(o.get("expanded")).unwrap_or(false),
            });
        }
        let rank = |s: GoalState| match s {
            GoalState::Active => 0,
            GoalState::Paused => 1,
            GoalState::Done => 2,
        };
        out.sort_by_key(|g| (rank(g.state), g.index));
        out
    }

    /// Show or hide one goal's finished tasks: flips its `expanded` in the file object. Returns the new value, `None`
    /// when no goal has that key.
    pub fn toggle_expanded(raw: &mut Json, key: &str) -> Option<bool> {
        let g = Self::parse(raw).into_iter().find(|g| g.key == key)?;
        let Some(Value::Array(goals)) = raw.get_mut("goals") else { return None };
        let o = goals.get_mut(g.index)?.as_object_mut()?;
        let e = !g.expanded;
        o.insert("expanded".into(), Value::Bool(e));
        Some(e)
    }

    /// The asks that wait on the user now: not done, not yet answered, and nobody else acts next. Only these ask for
    /// attention (the bell); goals never do.
    pub fn waiting(items: &[PlanItem]) -> Vec<&PlanItem> {
        items.iter().filter(|i| !i.done && i.chosen.is_none() && i.live.next.as_deref().is_none_or(|n| n == "owner")).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobj;
    use crate::model::Plan;

    fn sample() -> Json {
        jobj! {"id": "goals", "preset": "todo", "list": "goals", "items": [], "goals": [
            {"id": "g-done", "title": "Get a job", "state": "done", "tasks": [{"title": "CV", "state": "done"}]},
            {"id": "g-ship", "title": "Ship Heimdall publicly", "icon": "eye", "about": "why it matters", "now": "Opening film  nearly\nfinal", "moved": 1000,
             "tasks": [{"id": "t1", "title": "Opening film", "state": "done", "at": 900},
                       {"id": "t0", "title": "Privacy scan", "state": "done", "at": 500},
                       {"id": "t2", "title": "Signing", "state": "now"},
                       {"id": "t3", "title": "Updater", "state": "next"}]},
            {"id": "g-pause", "title": "Make Jarvis efficient", "state": "paused"},
            {"id": "g-arsenal", "title": "Ship Arsenal"},
            {"title": ""}]}
    }

    #[test]
    fn goals_sink_done_last_and_paused_under_the_active_ones() {
        let g = Goals::parse(&sample());
        assert_eq!(g.iter().map(|g| g.key.as_str()).collect::<Vec<_>>(), ["g-ship", "g-arsenal", "g-pause", "g-done"]);
        assert_eq!(g[0].now.as_deref(), Some("Opening film nearly final"));
        assert_eq!(g[2].state, GoalState::Paused);
    }

    #[test]
    fn progress_counts_done_of_all_known_tasks_and_none_without_tasks() {
        let g = Goals::parse(&sample());
        assert_eq!(g[0].progress(), Some((2, 4)));
        assert_eq!(g[0].share(), Some(0.5));
        assert_eq!(g[1].progress(), None, "a goal with no tasks shows no bar");
        assert_eq!(g[3].share(), Some(1.0));
        assert_eq!(g[0].current().iter().map(|t| t.title.as_str()).collect::<Vec<_>>(), ["Signing"]);
        // Finished tasks accumulate in the order they were finished.
        assert_eq!(g[0].done_tasks().iter().map(|t| t.key.as_str()).collect::<Vec<_>>(), ["t0", "t1"]);
    }

    #[test]
    fn a_move_within_the_day_is_recent_and_a_done_goal_never_is() {
        let g = Goals::parse(&sample());
        assert!(g[0].moved_recently(1000.0 + 3600.0));
        assert!(!g[0].moved_recently(1000.0 + RECENT + 1.0));
        assert!(!g[1].moved_recently(1000.0));
        assert!(!g[3].moved_recently(0.0));
    }

    #[test]
    fn the_symbol_is_the_goals_own_then_its_project_then_a_flag() {
        let g = Goals::parse(&sample());
        assert_eq!(g[0].symbol(), "eye");
        assert_eq!(g[1].symbol(), "shippingbox", "Arsenal");
        assert_eq!(g[2].symbol(), "person.wave.2", "Jarvis");
        assert_eq!(g[3].symbol(), "flag");
    }

    #[test]
    fn a_click_flips_one_goals_expanded_and_keeps_every_other_key() {
        let mut o = sample();
        assert_eq!(Goals::toggle_expanded(&mut o, "g-ship"), Some(true));
        assert_eq!(o["goals"][1]["expanded"], true);
        assert_eq!(o["goals"][1]["now"], "Opening film  nearly\nfinal");
        assert_eq!(o["goals"][1]["about"], "why it matters", "unknown keys on a goal are kept");
        assert_eq!(Goals::toggle_expanded(&mut o, "g-ship"), Some(false));
        assert_eq!(o["goals"][1]["expanded"], false);
        assert_eq!(Goals::toggle_expanded(&mut o, "nope"), None);
        assert!(o["goals"][0].get("expanded").is_none());
    }

    #[test]
    fn only_open_unanswered_asks_wait_on_owner() {
        let items = Plan::items(&jobj! {"items": [
            {"id": "a", "title": "Approve caches"},
            {"id": "b", "title": "Answered", "chosen": "yes"},
            {"id": "c", "title": "Ticked", "done": true},
            {"id": "d", "title": "Ania's turn", "next": "ania"},
            {"id": "e", "title": "Owner's turn", "next": "Owner"}]});
        assert_eq!(Goals::waiting(&items).iter().map(|i| i.key.as_str()).collect::<Vec<_>>(), ["a", "e"]);
    }
}
