//! Plan islands (Goals, Scheduled, Settled), their items, attention and the responses a click writes.
//! 17:58: "I don't think we should have a to-do list, I think we should have a goal list": an old `todo.json` is not
//! drawn (Heimdall migrates it). The preset name "todo" stays as a file-format key for every list island.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use super::island::{Island, TodoGroup};
use super::{due::Due, j, Json};

/// The plan lists, docked left in this order by default. Each is one island with `preset: "todo"` and `list` naming
/// it; the island id is the list name. Any other `preset: "todo"` island is an agent's own checklist (its own title).
pub struct PlanList;
impl PlanList {
    pub const ALL: [&'static str; 3] = ["goals", "scheduled", "settled"];

    /// The retired To do list (`todo.json`, or a file whose `list` is "todo"): never drawn, its file left alone for
    /// Heimdall to migrate.
    pub fn legacy_todo(id: &str, raw: &Json) -> bool {
        id == "todo" || j::str(raw.get("list")) == Some("todo")
    }

    pub fn title(l: &str) -> &'static str {
        match l {
            "goals" => "Goals",
            "scheduled" => "Scheduled",
            "settled" => "Settled",
            _ => "List",
        }
    }

    pub fn icon(l: &str) -> &'static str {
        match l {
            "goals" => "flag",
            "scheduled" => "calendar",
            "settled" => "checkmark.seal",
            _ => "checklist",
        }
    }

    /// Goals 0, an agent's checklist just after it, Scheduled 1, Settled 2 (Today is 3).
    pub fn order(l: &str) -> f64 {
        match l {
            "goals" => 0.0,
            "todo" => 0.5,
            "scheduled" => 1.0,
            "settled" => 2.0,
            _ => 0.0,
        }
    }

    /// A fresh plan island: what the CLI (and Rust) write when the list does not exist yet. Settled stays a pill.
    /// Goals also gets its empty `goals`.
    pub fn new_island(l: &str) -> Json {
        let mut o = Json::new();
        o.insert("id".into(), Value::from(l));
        o.insert("preset".into(), Value::from("todo"));
        o.insert("list".into(), Value::from(l));
        o.insert("title".into(), Value::from(Self::title(l)));
        o.insert("icon".into(), Value::from(Self::icon(l)));
        o.insert("items".into(), Value::Array(vec![]));
        if l == "goals" {
            o.insert("goals".into(), Value::Array(vec![]));
        }
        o
    }

    /// Where a plan list's last content waits when its file is gone (the names Heimdall's writers use,
    /// `crates/app/src/goals/ops.rs`): the copy kept when the user closed it (`.closed-<list>.json`), and the copy kept at
    /// its last write (`.kept-<list>.json`).
    pub fn closed_copy(list: &str) -> String {
        format!(".closed-{list}.json")
    }
    pub fn kept_copy(list: &str) -> String {
        format!(".kept-{list}.json")
    }

    /// What an islands-side writer (the due move, `islands plan … add|move --to`) starts a plan list from (review
    /// 31 Oct: a missing goals.json was made fresh and the goals and their finished tasks were lost): the file; else the
    /// closed copy, still hidden (it stays closed; Heimdall's reopen shows it with what was written since); else the
    /// copy kept at its last write; else a fresh list. A Goals list never comes out without `goals` while a copy holds
    /// some. `hidden` is left as the source has it: only a writer that shows the list clears it.
    pub fn start_from(list: &str, file: Option<Json>, closed: Option<Json>, kept: Option<Json>) -> Json {
        let copy_goals = closed.iter().chain(kept.iter()).find_map(|c| j::arr(c.get("goals")).filter(|g| !g.is_empty()).cloned());
        let mut o = match (file, closed.clone(), kept) {
            (Some(f), _, _) => f,
            (None, Some(mut c), _) => {
                c.insert("hidden".into(), Value::Bool(true));
                c
            }
            (None, None, Some(k)) => k,
            (None, None, None) => Self::new_island(list),
        };
        if list == "goals" {
            let has = j::arr(o.get("goals")).is_some_and(|g| !g.is_empty());
            match copy_goals {
                Some(g) if !has => {
                    o.insert("goals".into(), Value::Array(g));
                }
                _ if o.get("goals").is_none() => {
                    o.insert("goals".into(), Value::Array(vec![]));
                }
                _ => {}
            }
        }
        o
    }

    /// What closing an island (its X, right-click › Close) does to its file: a plan list, known by id or by its `list`
    /// key, is kept, hidden and minimised (the returned object; bug 2026-10-31T15-57-16: the user closed To do by accident
    /// and the whole list was deleted). `None`: any other island's file is removed, as before.
    pub fn closed(id: &str, raw: &Json) -> Option<Json> {
        if !(Self::ALL.contains(&id) || j::str(raw.get("list")).is_some_and(|l| Self::ALL.contains(&l))) {
            return None;
        }
        let mut o = raw.clone();
        o.insert("hidden".into(), Value::Bool(true));
        o.insert("expanded".into(), Value::Bool(false));
        Some(o)
    }
}

/// An approval proposal on a waiting item: the text and its buttons (Yes / No / Later unless the file says otherwise).
#[derive(Clone, Debug, PartialEq)]
pub struct Proposal {
    pub text: String,
    pub actions: Vec<(String, String)>,
}

impl Proposal {
    pub fn default_actions() -> Vec<(String, String)> {
        vec![("Yes".into(), "yes".into()), ("No".into(), "no".into()), ("Later".into(), "later".into())]
    }

    pub fn parse(v: Option<&Value>) -> Option<Proposal> {
        match v? {
            Value::String(s) if !s.is_empty() => Some(Proposal { text: s.clone(), actions: Self::default_actions() }),
            Value::Object(o) => {
                let text = j::str(o.get("text")).unwrap_or("").to_owned();
                let mut actions = vec![];
                if let Some(a) = j::objs(o.get("actions")) {
                    for a in a {
                        let Some(l) = j::nonempty(a.get("label")) else { continue };
                        actions.push((l.to_owned(), j::str(a.get("action")).map(str::to_owned).unwrap_or_else(|| l.to_lowercase())));
                    }
                }
                if actions.is_empty() {
                    actions = Self::default_actions();
                }
                if text.is_empty() && !o.contains_key("actions") {
                    return None;
                }
                Some(Proposal { text, actions })
            }
            _ => None,
        }
    }
}

/// One plan item as the app shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct PlanItem {
    /// Place in the file's `items` array.
    pub index: usize,
    /// Stable identity for diffing and responses: the file's `id`, else the title (made unique with "#2", "#3").
    pub key: String,
    /// The id the agent wrote (`None` when it left it out).
    pub id: Option<String>,
    pub title: String,
    pub group: Option<String>,
    pub done: bool,
    pub when: Option<String>,
    /// When it is due: epoch seconds (Scheduled items; the app counts down to it and moves the item to Goals' asks).
    pub due: Option<f64>,
    pub detail: Option<String>,
    pub proposal: Option<Proposal>,
    /// The approval the user picked (written by the app when he clicks; Rust then moves the item).
    pub chosen: Option<String>,
    /// The item's own SF Symbol (`icon`), else one is picked from its title (`PlanItem::symbol`).
    pub icon: Option<String>,
    /// What a row can draw beyond its words, all optional (`ItemLive`).
    pub live: ItemLive,
    /// When it happened, epoch seconds (Settled's `at`, written by the Goals job; the local calendar decides the day).
    pub at: Option<f64>,
    /// The goal it advanced (Settled's `goal`, e.g. "g-arsenal"; empty or missing is none).
    pub goal: Option<String>,
}

/// An item's optional live state, the plan list's share of a job island's (the `band` part on job islands: stage,
/// progress, when it started, its stages with their seconds). A waiting item: `kind` of ask (`approve`, `test`,
/// `pick`, `send`, `decide`, `review`, `write`, `fix`, `answer`), `asked` (epoch s) and who acts `next` (`owner`,
/// `heimdall`, or a name). A running item: `stage`, `progress` (0 to 1), `started` (epoch s) and `segments`
/// (`[{"title", "secs"}]`, oldest first, the last one current). Absent keys draw nothing.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ItemLive {
    pub kind: Option<String>,
    pub asked: Option<f64>,
    pub next: Option<String>,
    pub stage: Option<String>,
    pub progress: Option<f64>,
    pub started: Option<f64>,
    pub segments: Vec<(String, f64)>,
}

impl ItemLive {
    pub fn parse(o: &Json) -> ItemLive {
        let s = |k: &str| j::nonempty(o.get(k)).map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
        let segments = j::objs(o.get("segments"))
            .map(|v| {
                v.into_iter()
                    .filter_map(|g| Some((j::nonempty(g.get("title"))?.to_owned(), j::num(g.get("secs")).unwrap_or(0.0).max(0.0))))
                    .take(8)
                    .collect()
            })
            .unwrap_or_default();
        ItemLive {
            kind: s("kind").map(|k| k.to_lowercase()),
            asked: Due::seconds(o.get("asked")),
            next: s("next").map(|k| k.to_lowercase()),
            stage: s("stage"),
            progress: j::num(o.get("progress")).filter(|p| p.is_finite()).map(|p| p.clamp(0.0, 1.0)),
            started: Due::seconds(o.get("started")),
            segments,
        }
    }
}

/// Words that name a project or kind, and the small symbol a plan row shows for them. The title's project prefix
/// ("Echo guard: …") is matched first, then the whole title; the first entry that matches wins. Swift
/// `Plan.symbols` holds the same table.
pub const PLAN_SYMBOLS: [(&str, &str); 40] = [
    ("heimdall", "eye"),
    ("echo", "speaker.wave.2"),
    ("ack", "bubble.left"),
    ("jarvis", "person.wave.2"),
    ("voice", "waveform"),
    ("speaker", "person.2.wave.2"),
    ("vella", "text.quote"),
    ("verdict", "checkmark.shield"),
    ("vireo", "bird"),
    ("arsenal", "shippingbox"),
    ("decider", "arrow.triangle.branch"),
    ("model", "cpu"),
    ("bench", "chart.bar"),
    ("training", "chart.line.uptrend.xyaxis"),
    ("islands", "capsule"),
    ("menu", "menubar.rectangle"),
    ("iphone", "iphone"),
    ("ipad", "ipad"),
    ("mac", "laptopcomputer"),
    ("screen", "display"),
    ("vault", "archivebox"),
    ("trash", "trash"),
    ("cache", "trash"),
    ("night", "moon.stars"),
    ("agent", "sparkles"),
    ("plan", "list.bullet"),
    ("scheduled", "calendar"),
    ("bug", "ladybug"),
    ("failed", "arrow.clockwise"),
    ("stream", "dot.radiowaves.left.and.right"),
    ("brand", "star"),
    ("warsaw", "mappin.and.ellipse"),
    ("event", "calendar"),
    ("install", "square.and.arrow.down"),
    ("download", "arrow.down.circle"),
    ("release", "flag"),
    ("test", "testtube.2"),
    ("image", "photo"),
    ("photo", "photo"),
    ("site", "globe"),
];

impl PlanItem {
    /// The small symbol beside the title: the item's own `icon`, else the first `PLAN_SYMBOLS` word in its project
    /// prefix, then in its title, else one for its kind (waiting on the user, discussed, running, scheduled, settled).
    pub fn symbol(&self, list: &str) -> String {
        if let Some(i) = &self.icon {
            return i.clone();
        }
        let title = self.title.to_lowercase();
        let prefix = title.split_once(':').map(|p| p.0).unwrap_or("");
        let words = |s: &str| -> Vec<String> { s.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_owned).collect() };
        // A word matches its key exactly or as a plural or compound ("voices", "speakers", "acks", "trash-rule").
        let hit = |s: &str| -> Option<&'static str> {
            let ws = words(s);
            PLAN_SYMBOLS.iter().find(|(k, _)| ws.iter().any(|w| w == k || w.strip_suffix('s') == Some(*k))).map(|p| p.1)
        };
        if let Some(s) = hit(prefix).or_else(|| hit(&title)) {
            return s.to_owned();
        }
        match (list, self.group.as_deref()) {
            ("scheduled", _) => "clock",
            ("settled", _) => "checkmark",
            (_, Some("waiting")) => "hand.raised",
            (_, Some("running")) => "gearshape",
            _ => "bubble.left.and.bubble.right",
        }
        .to_owned()
    }

    /// A Settled item that records a decision (`kind: "decided"`); anything else, `kind` missing included, was shipped.
    pub fn decided(&self) -> bool {
        matches!(self.live.kind.as_deref(), Some("decided" | "decision"))
    }

    /// Whether a click on the row opens it: there is more than the title to read or answer.
    pub fn discloses(&self) -> bool {
        self.detail.is_some() || (self.proposal.is_some() && !self.done)
    }

    /// The first line of the detail that has words, spaces collapsed: what a closed row shows under its title.
    pub fn detail_line(&self) -> Option<String> {
        self.detail.as_deref()?.lines().map(|l| l.split_whitespace().collect::<Vec<_>>().join(" ")).find(|l| !l.is_empty())
    }
}

/// An item's id as the file has it: a non-empty string or a number.
pub fn item_id(o: &Json) -> Option<String> {
    match o.get("id")? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        // NSNumber's description: whole values without a fraction.
        Value::Number(n) => Some(match n.as_f64() {
            Some(x) if x.fract() == 0.0 && x.abs() < 9e15 => format!("{}", x as i64),
            _ => n.to_string(),
        }),
        Value::Bool(b) => Some(if *b { "1" } else { "0" }.to_owned()),
        _ => None,
    }
}

pub struct Plan;
impl Plan {
    /// Items in file order. Strings are plain items; objects need a title.
    pub fn items(raw: &Json) -> Vec<PlanItem> {
        let mut out = vec![];
        let mut seen: HashMap<String, usize> = HashMap::new();
        let mut unique = |k: &str| {
            let n = seen.entry(k.to_owned()).or_insert(0);
            *n += 1;
            if *n == 1 {
                k.to_owned()
            } else {
                format!("{k}#{n}")
            }
        };
        let Some(a) = j::arr(raw.get("items")) else { return out };
        for (k, v) in a.iter().enumerate() {
            match v {
                Value::String(s) => {
                    if !s.is_empty() {
                        out.push(PlanItem {
                            index: k,
                            key: unique(s),
                            id: None,
                            title: s.clone(),
                            group: None,
                            done: false,
                            when: None,
                            due: None,
                            detail: None,
                            proposal: None,
                            chosen: None,
                            icon: None,
                            live: ItemLive::default(),
                            at: None,
                            goal: None,
                        });
                    }
                }
                Value::Object(o) => {
                    let Some(t) = j::nonempty(o.get("title")) else { continue };
                    let id = item_id(o);
                    out.push(PlanItem {
                        index: k,
                        key: unique(id.as_deref().unwrap_or(t)),
                        id,
                        title: t.to_owned(),
                        group: TodoGroup::valid(j::str(o.get("group"))),
                        done: j::boolean(o.get("done")).unwrap_or(false),
                        when: j::nonempty(o.get("when")).map(str::to_owned),
                        due: Due::seconds(o.get("due")),
                        detail: j::nonempty(o.get("detail")).map(str::to_owned),
                        proposal: Proposal::parse(o.get("proposal")),
                        chosen: j::str(o.get("chosen")).map(str::to_owned),
                        icon: j::nonempty(o.get("icon")).map(str::to_owned),
                        live: ItemLive::parse(o),
                        at: Due::seconds(o.get("at")).map(|a| if a > 1e11 { a / 1000.0 } else { a }),
                        goal: j::nonempty(o.get("goal")).map(|g| g.trim().to_owned()).filter(|g| !g.is_empty()),
                    });
                }
                _ => {}
            }
        }
        out
    }

    /// The groups a list shows, in order. To do: Running, Waiting on you, Discussed (ungrouped items count as
    /// discussed). Scheduled and Settled: one group without a header (`""`). Ticked items sink to the bottom.
    pub fn grouped(items: &[PlanItem], list: &str) -> Vec<(String, Vec<PlanItem>)> {
        let sink = |rows: Vec<&PlanItem>| -> Vec<PlanItem> {
            rows.iter().filter(|r| !r.done).chain(rows.iter().filter(|r| r.done)).map(|r| (*r).clone()).collect()
        };
        if list == "scheduled" {
            // What is due now comes first (it stays here, drawn as due now), then the rest in file order.
            let now = super::time::now();
            let due = |i: &&PlanItem| !i.done && i.due.is_some_and(|d| d <= now);
            let rows: Vec<&PlanItem> = items.iter().filter(due).chain(items.iter().filter(|i| !due(i))).collect();
            return if items.is_empty() { vec![] } else { vec![(String::new(), sink(rows))] };
        }
        if list != "todo" {
            return if items.is_empty() { vec![] } else { vec![(String::new(), sink(items.iter().collect()))] };
        }
        TodoGroup::ALL
            .iter()
            .filter_map(|g| {
                let rows: Vec<&PlanItem> = items.iter().filter(|i| i.group.as_deref().unwrap_or("discussed") == *g).collect();
                (!rows.is_empty()).then(|| ((*g).to_owned(), sink(rows)))
            })
            .collect()
    }

    /// Items in display order (what the CLI numbers and the app draws top to bottom).
    pub fn display_order(raw: &Json, list: &str) -> Vec<PlanItem> {
        Self::grouped(&Self::items(raw), list).into_iter().flat_map(|(_, i)| i).collect()
    }

    /// A stable id for a new item: its title as a short slug, made unique among `taken`.
    pub fn slug(title: &str, taken: &HashSet<String>) -> String {
        let mut s: String = title.to_lowercase().chars().map(|c| if c.is_alphanumeric() { c } else { '-' }).collect();
        while s.contains("--") {
            s = s.replace("--", "-");
        }
        let t: String = s.trim_matches('-').chars().take(40).collect();
        let mut s = t.trim_matches('-').to_owned();
        if s.is_empty() {
            s = "item".into();
        }
        let mut out = s.clone();
        let mut n = 2;
        while taken.contains(&out) {
            out = format!("{s}-{n}");
            n += 1;
        }
        out
    }
}

/// What changed between two renders of a list, by key: drives the row animations.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ItemDiff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    /// Kept rows whose place in the display order changed (moved up or down, or to another group).
    pub moved: Vec<String>,
    /// Kept rows ticked off or back on.
    pub ticked: Vec<String>,
    pub untouched: Vec<String>,
}

impl ItemDiff {
    pub fn new(old: &[PlanItem], new: &[PlanItem]) -> ItemDiff {
        let old_keys: Vec<&String> = old.iter().map(|i| &i.key).collect();
        let new_keys: Vec<&String> = new.iter().map(|i| &i.key).collect();
        let mut o: HashMap<&String, &PlanItem> = HashMap::new();
        for i in old {
            o.entry(&i.key).or_insert(i);
        }
        let mut n: HashMap<&String, &PlanItem> = HashMap::new();
        for i in new {
            n.entry(&i.key).or_insert(i);
        }
        let old_set: HashSet<&String> = old_keys.iter().copied().collect();
        let new_set: HashSet<&String> = new_keys.iter().copied().collect();
        let mut d = ItemDiff {
            removed: old_keys.iter().filter(|k| !new_set.contains(*k)).map(|k| (*k).clone()).collect(),
            added: new_keys.iter().filter(|k| !old_set.contains(*k)).map(|k| (*k).clone()).collect(),
            ..Default::default()
        };
        let ticked: HashSet<&String> =
            new_keys.iter().copied().filter(|k| o.get(k).is_some_and(|w| w.done != n[k].done)).collect();
        let kept_old: Vec<&String> = old_keys.iter().copied().filter(|k| new_set.contains(k) && !ticked.contains(k)).collect();
        let kept_new: Vec<&String> = new_keys.iter().copied().filter(|k| old_set.contains(k) && !ticked.contains(k)).collect();
        let mut pos_old: HashMap<&String, usize> = HashMap::new();
        for (i, k) in kept_old.iter().enumerate() {
            pos_old.entry(k).or_insert(i);
        }
        let mut pos_new: HashMap<&String, usize> = HashMap::new();
        for (i, k) in kept_new.iter().enumerate() {
            pos_new.entry(k).or_insert(i);
        }
        for key in new_keys.iter().copied().filter(|k| old_set.contains(k)) {
            let (Some(was), Some(now)) = (o.get(key), n.get(key)) else { continue };
            if ticked.contains(key) {
                d.ticked.push(key.clone());
            } else if pos_old.get(key) != pos_new.get(key)
                || was.group.as_deref().unwrap_or("discussed") != now.group.as_deref().unwrap_or("discussed")
            {
                d.moved.push(key.clone());
            } else {
                d.untouched.push(key.clone());
            }
        }
        d
    }
}

pub struct Attention;
impl Attention {
    pub const KINDS: [&'static str; 2] = ["waiting", "question"];
    /// Seconds between two pulses while attention is set and the island is closed.
    pub const INTERVAL: f64 = 6.0;

    /// The island's own `attention` key, or `waiting` for a plan island with an open waiting item (derived): on Goals,
    /// an ask that waits on the user (`Goals::waiting`); never a goal.
    pub fn of(i: &Island) -> Option<String> {
        if let Some(a) = j::str(i.raw.get("attention")).filter(|a| Self::KINDS.contains(a)) {
            return Some(a.to_owned());
        }
        (i.preset() == "todo" && i.waits_on_owner()).then(|| "waiting".to_owned())
    }

    /// What the user has seen once he opens the island: the same kind with the same open waiting items and question
    /// buttons does not pulse again; a new one does.
    pub fn signature(i: &Island) -> Option<String> {
        let kind = Self::of(i)?;
        let items = Plan::items(&i.raw);
        let mut waiting: Vec<String> = if i.list().as_deref() == Some("goals") {
            super::goals::Goals::waiting(&items).into_iter().map(|t| t.key.clone()).collect()
        } else {
            items.into_iter().filter(|t| t.group.as_deref() == Some("waiting") && !t.done).map(|t| t.key).collect()
        };
        waiting.sort();
        let actions: Vec<String> = i.parts().iter().filter_map(|p| j::str(p.get("action")).map(str::to_owned)).collect();
        let mut all = vec![kind];
        all.extend(waiting);
        all.extend(actions);
        Some(all.join("|"))
    }
}

/// The response objects a click writes (`IslandFile::respond_object` adds `at`).
pub struct Responses;
impl Responses {
    pub fn tick(island: &str, item: &str, done: bool) -> Json {
        j::object([
            ("island", Value::from(island)),
            ("kind", Value::from("tick")),
            ("action", Value::from(if done { "done" } else { "undo" })),
            ("value", Value::from(item)),
        ])
    }

    pub fn approval(island: &str, item: &str, action: &str) -> Json {
        j::object([
            ("island", Value::from(island)),
            ("kind", Value::from("approval")),
            ("action", Value::from(action)),
            ("value", Value::from(item)),
        ])
    }

    /// A Scheduled item fell due: it stays in Scheduled, drawn as due now (what has to be done
    /// because it is scheduled lives in Scheduled); the app hears it once.
    pub fn due(item: &str) -> Json {
        j::object([
            ("island", Value::from("scheduled")),
            ("kind", Value::from("due")),
            ("action", Value::from("now")),
            ("value", Value::from(item)),
        ])
    }
}

pub struct Lifetime;
impl Lifetime {
    /// `--max-hours N`: hours until the app quits itself; 0 (or less) means no limit. Default 24 h; an unreadable value
    /// keeps the default.
    pub fn max_hours(args: &[String]) -> Option<f64> {
        let mut h = 24.0;
        if let Some(i) = args.iter().position(|a| a == "--max-hours") {
            if let Some(v) = args.get(i + 1).and_then(|v| swift_double(v)) {
                h = v;
            }
        }
        (h > 0.0).then_some(h)
    }

    /// `--parent-pid N`: the process whose exit quits the app. `None` when absent or not a real pid.
    pub fn parent_pid(args: &[String]) -> Option<i32> {
        let i = args.iter().position(|a| a == "--parent-pid")?;
        let v: i32 = args.get(i + 1)?.parse().ok()?;
        (v > 1).then_some(v)
    }
}

/// `Double(String)`: a plain decimal or scientific number, also `inf`/`nan` (rejected later by the callers' checks).
pub fn swift_double(s: &str) -> Option<f64> {
    if s.is_empty() || s.trim() != s || s.starts_with('+') && s.len() == 1 {
        return None;
    }
    s.parse::<f64>().ok()
}
