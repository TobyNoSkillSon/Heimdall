//! `islands plan [goals|scheduled|settled] VERB …`. On Goals the verbs work on its asks (`items`); its goals are listed,
//! written by the Goals job. There is no To do list any more: old callers' `todo` (the list name,
//! `--to todo`, the `islands todo` command) means Goals' asks, silently; `islands todo --id X` still works on X.

use std::collections::HashSet;

use serde_json::Value;

use super::{fail, stamp, text, usage};
use crate::model::due::Cal;
use crate::model::{j, time, Due, GoalState, Goals, Island, IslandFile, Json, Paths, Plan, PlanList, TaskState, TodoGroup};

/// The items of a file as objects (a bare string becomes `{"title": …}`; anything else is dropped).
fn objects(o: Option<&Json>) -> Vec<Json> {
    j::arr(o.and_then(|o| o.get("items")))
        .map(|a| {
            a.iter()
                .filter_map(|v| match v {
                    Value::Object(o) => Some(o.clone()),
                    Value::String(s) => Some(j::object([("title", Value::from(s.clone()))])),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

fn print_list(id: &str) {
    let o = IslandFile::read(&Paths::file(id)).unwrap_or_default();
    let l = Island::new(id, o.clone()).list().unwrap_or_else(|| "todo".into());
    let shown = Plan::display_order(&o, &l);
    println!("{} ({id}){}", PlanList::title(&l), if shown.is_empty() { " · nothing here" } else { "" });
    let (now, cal) = (time::now(), Cal::current());
    let mut last: Option<String> = None;
    for (n, it) in shown.iter().enumerate() {
        let g = if l == "todo" { it.group.clone().unwrap_or_else(|| "discussed".into()) } else { String::new() };
        if l == "todo" && last.as_deref() != Some(g.as_str()) {
            println!("  {}", TodoGroup::label(&g));
            last = Some(g.clone());
        }
        let mut line = format!("  {}. {} {}", n + 1, if it.done { "[x]" } else { "[ ]" }, it.title);
        // A due time wins over `when`, as on screen.
        if let Some(d) = it.due {
            line += &format!(" · {}", Due::label(d, now, &cal).0);
        } else if let Some(w) = &it.when {
            line += &format!(" · {w}");
        }
        if it.proposal.is_some() {
            line += &it.chosen.as_ref().map(|c| format!(" · chose {c}")).unwrap_or_else(|| " · proposal".into());
        }
        if let Some(i) = &it.id {
            line += &format!("  ({i})");
        }
        println!("{line}");
    }
    if l == "goals" {
        print_goals(&o, now);
    }
}

/// Goals, compactly: one line per goal (its state when not active, done of all known tasks, how long since it moved,
/// its id), then its latest move and the task in hand.
fn print_goals(o: &Json, now: f64) {
    let goals = Goals::parse(o);
    if goals.is_empty() {
        return;
    }
    println!("  Goals");
    for g in &goals {
        let mut line = format!("  - {}", g.title);
        match g.state {
            GoalState::Done => line += " · done",
            GoalState::Paused => line += " · paused",
            GoalState::Active => {}
        }
        if let Some((d, n)) = g.progress() {
            line += &format!(" · {d}/{n} tasks");
        }
        if let Some(m) = g.moved {
            let mins = ((now - m).max(0.0) / 60.0) as i64;
            let ago = if mins < 60 { format!("{} min", mins.max(1)) } else if mins < 1440 { format!("{} h", mins / 60) } else { format!("{} d", mins / 1440) };
            line += &format!(" · moved {ago} ago");
        }
        if let Some(i) = &g.id {
            line += &format!("  ({i})");
        }
        println!("{line}");
        let current: Vec<&str> = g.tasks.iter().filter(|t| t.state == TaskState::Now).map(|t| t.title.as_str()).collect();
        let mut sub = g.now.clone().unwrap_or_default();
        if !current.is_empty() {
            sub += &format!("{}now: {}", if sub.is_empty() { "" } else { " · " }, current.join(", "));
        }
        if !sub.is_empty() {
            println!("      {sub}");
        }
    }
}

pub fn run(cmd: &str, args: Vec<String>) {
    let help = usage::PLAN;
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{help}");
        std::process::exit(0);
    }
    let mut rest = args.clone();
    let mut take = |flag: &str| -> Option<String> {
        let k = rest.iter().position(|a| a == flag)?;
        if k + 1 >= rest.len() {
            fail(&format!("islands {cmd}: {flag} needs a value"));
        }
        let v = rest[k + 1].clone();
        rest.drain(k..=k + 1);
        Some(v)
    };
    let tid = take("--id");
    let when = take("--when");
    let detail = take("--detail");
    let proposal = take("--proposal");
    let item_id = take("--item-id");
    let goal = take("--goal");
    // Old callers' `todo` is Goals' asks.
    let to_list = take("--to").map(|t| if t == "todo" { "goals".to_owned() } else { t });
    let due_text = take("--due");
    if let Some(t) = &to_list {
        if !PlanList::ALL.contains(&t.as_str()) {
            fail(&format!("islands {cmd}: --to takes goals, scheduled or settled"));
        }
    }
    let due: Option<f64> = due_text.map(|t| {
        Due::parse(&t, time::now(), &Cal::current()).unwrap_or_else(|| {
            fail(&format!("islands {cmd}: --due \"{t}\" is not a time (\"2026-10-24 10:00\", \"tomorrow 10:00\", +2h)"))
        })
    });
    let group_flag =
        rest.iter().find(|a| ["--running", "--waiting", "--discussed"].contains(&a.as_str())).map(|a| a[2..].to_owned());
    rest.retain(|a| !a.starts_with("--"));
    // Goals by default; an old caller's `todo` list name is Goals.
    let mut list = "goals".to_owned();
    if cmd == "plan" {
        if let Some(first) = rest.first() {
            if PlanList::ALL.contains(&first.as_str()) || first == "todo" {
                list = if first == "todo" { "goals".into() } else { first.clone() };
                rest.remove(0);
            }
        }
    }
    let verb = rest.first().cloned().unwrap_or_else(|| "list".into());
    let operands: Vec<String> = rest.iter().skip(1).cloned().collect();
    let id: String = tid.clone().unwrap_or_else(|| list.clone());

    if verb == "list" {
        if cmd == "plan" && tid.is_none() && args.first().is_none_or(|a| !PlanList::ALL.contains(&a.as_str()) && a != "todo") {
            PlanList::ALL.iter().for_each(|l| print_list(l));
        } else {
            print_list(&id);
        }
        std::process::exit(0);
    }
    if !["add", "done", "undo", "move", "rm", "prune"].contains(&verb.as_str()) {
        fail(help);
    }
    if verb != "prune" && operands.is_empty() {
        fail(help);
    }
    // One system (docs/one-system.md): nothing enters Scheduled without his yes, nothing Settled without
    // proof. Those go through Heimdall's `plan_edit` (propose, then his yes; Settled with its proof), never this CLI.
    let into = to_list.clone().unwrap_or_else(|| id.clone());
    // `ISLANDS_LAB_PLAN_WRITES=1`: the CLI's own tests and lab fixtures only.
    let lab = std::env::var("ISLANDS_LAB_PLAN_WRITES").as_deref() == Ok("1");
    if !lab && (verb == "add" && matches!(id.as_str(), "scheduled" | "settled")) || (!lab && verb == "move" && to_list.as_ref().is_some_and(|t| t != &id) && matches!(into.as_str(), "scheduled" | "settled")) {
        fail(&format!("islands {cmd}: {into} takes nothing from here: a promise is proposed for User's yes and Settled fills from proof, both through Heimdall's plan_edit"));
    }
    if verb == "move" && group_flag.is_none() && to_list.is_none() && when.is_none() && due.is_none() {
        fail(&format!("islands {cmd} move N|ID|\"TEXT\" --running|--waiting|--discussed | --to goals|scheduled|settled [--when W]"));
    }

    // N (from the listing), an item id, the exact title, or a unique part of it → index in the file's items.
    let resolve = |r: &str, o: &Json| -> usize {
        let l = Island::new(&id, o.clone()).list().unwrap_or_else(|| "todo".into());
        let shown = Plan::display_order(o, &l);
        if let Ok(n) = r.parse::<i64>() {
            if n < 1 || n as usize > shown.len() {
                fail(&format!("islands {cmd}: no item {n}"));
            }
            return shown[n as usize - 1].index;
        }
        let all = Plan::items(o);
        if let Some(it) = all.iter().find(|it| it.id.as_deref() == Some(r) || it.key == r) {
            return it.index;
        }
        if let Some(it) = all.iter().find(|it| it.title == r) {
            return it.index;
        }
        let hits: Vec<_> = all.iter().filter(|it| text::contains_ci(&it.title, r)).collect();
        if hits.len() != 1 {
            fail(&format!("islands {cmd}: {} \"{r}\"", if hits.is_empty() { "no item matches" } else { "several items match" }));
        }
        hits[0].index
    };
    let fresh = |id: &str, list: &str| -> Json {
        if cmd == "todo" && tid.is_some() {
            return j::object([
                ("id", Value::from(id)),
                ("title", Value::from("List")),
                ("preset", Value::from("todo")),
                ("icon", Value::from("checklist")),
            ]);
        }
        let mut o = PlanList::new_island(list);
        o.insert("id".into(), Value::from(id));
        o
    };

    let mut said = String::new();
    let out = IslandFile::locked(|| {
        // A plan list starts from its file, else its closed or kept copy (never from nothing: review 31 Oct, a missing
        // goals.json was made fresh and its goals were lost); a file that does not parse is never replaced.
        let path = Paths::file(&id);
        let old = IslandFile::read(&path);
        if old.is_none() && path.exists() {
            fail(&format!("islands {cmd}: {} does not parse; not replacing it", path.display()));
        }
        let base = if old.is_none() && PlanList::ALL.contains(&id.as_str()) { IslandFile::plan_start(&id) } else { old.clone() };
        let mut o = base.clone().unwrap_or_else(|| fresh(&id, &list));
        let mut items = objects(base.as_ref());
        let taken: HashSet<String> = Plan::items(&o).into_iter().filter_map(|i| i.id).collect();
        let own_list = j::str(o.get("list")).unwrap_or("todo").to_owned();
        match verb.as_str() {
            "add" => {
                // A new item brings a closed (hidden) list back: closing never swallows what comes later (review 1 Nov).
                o.remove("hidden");
                let mut ids = taken.clone();
                for t in operands.iter().filter(|t| !t.is_empty()) {
                    let mut it = j::object([("title", Value::from(t.clone()))]);
                    let iid = item_id.clone().unwrap_or_else(|| Plan::slug(t, &ids));
                    ids.insert(iid.clone());
                    it.insert("id".into(), Value::from(iid));
                    if own_list == "todo" || group_flag.is_some() {
                        it.insert("group".into(), Value::from(group_flag.clone().unwrap_or_else(|| "discussed".into())));
                    }
                    if let Some(w) = &when {
                        it.insert("when".into(), Value::from(w.clone()));
                    }
                    if let Some(d) = due {
                        it.insert("due".into(), j::number(d));
                        it.remove("when");
                    }
                    if let Some(d) = &detail {
                        it.insert("detail".into(), Value::from(d.clone()));
                    }
                    if let Some(p) = &proposal {
                        it.insert("proposal".into(), Value::Object(j::object([("text", Value::from(p.clone()))])));
                        if group_flag.is_none() && own_list != "goals" {
                            it.insert("group".into(), Value::from("waiting"));
                        }
                    }
                    // A Goals ask names the goal it serves ("" for none).
                    if own_list == "goals" {
                        it.insert("goal".into(), Value::from(goal.clone().unwrap_or_default()));
                    }
                    items.push(it);
                }
                said = format!("added {}", operands.len());
            }
            "done" | "undo" => {
                for k in operands.iter().map(|r| resolve(r, &o)).collect::<Vec<_>>() {
                    if verb == "done" {
                        items[k].insert("done".into(), Value::Bool(true));
                    } else {
                        items[k].remove("done");
                    }
                }
                said = format!("{} {}", if verb == "done" { "ticked" } else { "unticked" }, operands.len());
            }
            "move" => 'mv: {
                let ks: Vec<usize> = operands.iter().map(|r| resolve(r, &o)).collect();
                for &k in &ks {
                    if let Some(g) = &group_flag {
                        items[k].insert("group".into(), Value::from(g.clone()));
                    }
                    if let Some(w) = &when {
                        items[k].insert("when".into(), Value::from(w.clone()));
                        // A new word without a date drops an old due time (it would win on screen or move the item back).
                        if due.is_none() {
                            items[k].remove("due");
                        }
                    }
                    if let Some(d) = due {
                        items[k].insert("due".into(), j::number(d));
                        items[k].remove("when");
                    }
                }
                match &to_list {
                    Some(dest) if *dest != own_list || tid.is_some() => {
                        // To another list: it leaves this island and arrives at the end of the other (same id, same keys).
                        if *dest == id {
                            break 'mv;
                        }
                        // The destination starts as any plan list a writer creates: its file, else a kept copy.
                        let Some(d_old) = IslandFile::plan_start(dest) else {
                            fail(&format!("islands {cmd}: {} does not parse; not replacing it", Paths::file(dest).display()));
                        };
                        let d_file = IslandFile::read(&Paths::file(dest));
                        let mut d = d_old.clone();
                        d.insert("id".into(), Value::from(dest.clone()));
                        let mut d_items = objects(Some(&d_old));
                        for &k in &ks {
                            let mut it = items[k].clone();
                            if dest == "goals" {
                                // An ask has no group; it names its goal.
                                it.remove("group");
                                it.entry("goal").or_insert(Value::from(goal.clone().unwrap_or_default()));
                            }
                            d_items.push(it);
                        }
                        d.insert("items".into(), Value::Array(d_items.into_iter().map(Value::Object).collect()));
                        stamp(&mut d, d_file.as_ref());
                        if let Err(e) = IslandFile::write_island(dest, &d) {
                            fail(&format!("islands {cmd}: {e}"));
                        }
                        let set: HashSet<usize> = ks.iter().copied().collect();
                        items = items.into_iter().enumerate().filter(|(k, _)| !set.contains(k)).map(|(_, v)| v).collect();
                        said = format!("moved {} to {}", ks.len(), PlanList::title(dest));
                    }
                    _ => {
                        said = format!("moved {}", ks.len())
                            + &group_flag.as_ref().map(|g| format!(" to {}", TodoGroup::label(g))).unwrap_or_default()
                            + &when.as_ref().map(|w| format!(" ({w})")).unwrap_or_default();
                    }
                }
            }
            "rm" => {
                let ks: HashSet<usize> = operands.iter().map(|r| resolve(r, &o)).collect();
                items = items.into_iter().enumerate().filter(|(k, _)| !ks.contains(k)).map(|(_, v)| v).collect();
                said = format!("removed {}", ks.len());
            }
            _ => {
                let before = items.len();
                items.retain(|it| j::boolean(it.get("done")) != Some(true));
                said = format!("pruned {}", before - items.len());
            }
        }
        o.insert("items".into(), Value::Array(items.into_iter().map(Value::Object).collect()));
        stamp(&mut o, old.as_ref());
        // A plan list's kept copy follows its file (`write_island`).
        if let Err(e) = IslandFile::write_island(&id, &o) {
            fail(&format!("islands {cmd}: {e}"));
        }
        o
    });
    let open = objects(Some(&out)).iter().filter(|o| j::boolean(o.get("done")) != Some(true)).count();
    println!("{cmd} {id} · {said} · {open} open");
}
