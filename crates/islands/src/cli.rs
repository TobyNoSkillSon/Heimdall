//! The `islands` command: writes the island files the app watches (Swift: `islands/Sources/islands/main.swift`).
//! Every command prints one short line; the output, exit codes and files match the Swift CLI's.

mod plan;
mod put;
mod text;
pub mod usage;

use std::fs;
use std::path::Path;

use serde_json::Value;

use crate::model::file::{expand_tilde, ns_path, standardize};
use crate::model::{j, jsonfmt, time, Island, IslandFile, Json, OpenTarget, Paths, PlanList, ViewerAction, Visibility};

/// A one-line error on stderr, exit 2.
pub(crate) fn fail(s: &str) -> ! {
    eprintln!("{s}");
    std::process::exit(2)
}

/// The time the CLI stamps: epoch seconds to a tenth.
pub(crate) fn stamp_now() -> f64 {
    (time::now() * 10.0).round() / 10.0
}

pub(crate) fn cwd() -> String {
    std::env::current_dir().map(|d| d.to_string_lossy().into_owned()).unwrap_or_else(|_| "/".into())
}

/// `put x` / `open x` / … : `id · Title · preset state [progress] [value] [place] [open]`.
pub(crate) fn summary(o: &Json, id: &str) -> String {
    let i = Island::new(id, o.clone());
    let mut bits: Vec<String> = vec![i.preset().to_owned(), i.state().to_owned()];
    if let Some(p) = i.progress() {
        if i.state() == "running" {
            bits.push(format!("{}%", (p * 100.0) as i64));
        }
    }
    if let Some(v) = i.value() {
        bits.push(v);
    }
    if let Some((x, y)) = i.position() {
        bits.push(format!("placed at {},{}", Island::format(x), Island::format(y)));
    } else if let Some(side) = i.side() {
        bits.push(format!("dock {side}") + &i.edge_y().map(|y| format!(" at y {}", Island::format(y))).unwrap_or_default());
    }
    if i.expanded() {
        bits.push("open".into());
    }
    format!("{id} · {} · {}", i.title(), bits.join(" "))
}

fn screen_state() -> Json {
    IslandFile::read(&Paths::state()).and_then(|o| j::obj(o.get("islands")).cloned()).unwrap_or_default()
}

fn state_line(id: &str) -> String {
    let st = screen_state();
    let Some(s) = j::obj(st.get(id)) else { return " · not on screen".into() };
    let n = |k: &str| Island::format(j::num(s.get(k)).unwrap_or(0.0).round());
    let open = if j::boolean(s.get("open")) == Some(true) { "open" } else { "pill" };
    format!(" · {open} at {},{} {}×{}", n("x"), n("y"), n("width"), n("height"))
}

fn set_flags(ids: &[String], verb: &str, change: &[(&str, Value)]) {
    if ids.is_empty() {
        fail(&format!("islands {verb} <id>..."));
    }
    for id in ids {
        let Some(o) = IslandFile::patch(id, |o| {
            for (k, v) in change {
                o.insert((*k).to_owned(), v.clone());
            }
        }) else {
            fail(&format!("islands {verb}: no island {id}"))
        };
        println!("{verb} {}", summary(&o, id));
    }
}

/// An absolute path for a file the agent named relative to where it runs.
pub(crate) fn absolute(t: &str) -> String {
    if t.starts_with('/') || t.starts_with('~') || t.contains("://") {
        return ns_path(&expand_tilde(t));
    }
    standardize(&format!("{}/{t}", cwd()))
}

/// URLs stay as they are; a file (absolute, ~ or relative to the caller) becomes an absolute path; a bare domain gets
/// https.
pub(crate) fn resolve_target(t: &str) -> String {
    if t.contains("://") || t.starts_with("mailto:") {
        return t.to_owned();
    }
    let a = absolute(t);
    if t.starts_with('/') || t.starts_with('~') || Path::new(&a).exists() {
        return a;
    }
    if let Some(crate::model::island::Target::Url(u)) = OpenTarget::url(t) {
        return u;
    }
    a
}

/// Times for "started 12 min ago · updated 2 min ago": set when it is created, when what it shows changes, when it
/// ends.
pub(crate) fn stamp(o: &mut Json, old: Option<&Json>) {
    let now = j::number(stamp_now());
    if !o.contains_key("started") && old.is_none() {
        o.insert("started".into(), now.clone());
    }
    const SHOWN: [&str; 15] =
        ["title", "state", "stage", "progress", "value", "summary", "steps", "items", "kv", "image", "open", "parts", "path", "preset", "icon"];
    let changed = match old {
        None => true,
        Some(old) => SHOWN.iter().any(|k| match (old.get(*k), o.get(*k)) {
            (None, None) => false,
            (Some(a), Some(b)) => !j::eq(a, b),
            _ => true,
        }),
    };
    if changed {
        o.insert("updated".into(), now.clone());
    }
    let state = Island::new("", o.clone()).state().to_owned();
    let was = old.map(|w| Island::new("", w.clone()).state().to_owned());
    if state == "done" || state == "failed" {
        if was.as_deref() != Some(state.as_str()) || !o.contains_key("ended") {
            o.insert("ended".into(), now);
        }
    } else {
        o.remove("ended");
    }
}

/// `FileManager.removeItem`: a file, or a folder with everything in it.
pub(crate) fn remove_item(p: &Path) -> bool {
    match fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(p).is_ok(),
        Ok(_) => fs::remove_file(p).is_ok(),
        Err(_) => false,
    }
}

/// The visible `.json` files of a folder, sorted by name.
fn json_files(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut v: Vec<_> = fs::read_dir(dir)
        .map(|r| r.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default();
    v.retain(|p| {
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        p.extension().is_some_and(|e| e == "json") && !name.starts_with('.')
    });
    v.sort_by_key(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
    v
}

/// Every readable island file, sorted by file name: (id, object).
pub(crate) fn all_islands() -> Vec<(String, Json)> {
    json_files(&Paths::islands())
        .into_iter()
        .filter_map(|u| {
            let id = u.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            IslandFile::read(&u).map(|o| (id, o))
        })
        .collect()
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

pub fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(run(args));
}

fn run(mut args: Vec<String>) -> i32 {
    if args.is_empty() {
        println!("{}", usage::all());
        return 0;
    }
    let cmd = args.remove(0);
    Paths::ensure();
    match cmd.as_str() {
        "put" => put::run(args),
        "move" => move_island(&args),
        "view" => view(&args),
        "open" | "expand" | "pin" => set_flags(&args, &cmd, &[("expanded", Value::Bool(true)), ("hidden", Value::Bool(false))]),
        "close" | "collapse" | "unpin" => set_flags(&args, &cmd, &[("expanded", Value::Bool(false))]),
        "hide" => set_flags(&args, &cmd, &[("hidden", Value::Bool(true))]),
        "show" => set_flags(&args, &cmd, &[("hidden", Value::Bool(false))]),
        "hide-all" | "show-all" => {
            let hide = cmd == "hide-all";
            if let Err(e) = Visibility::set_hidden(hide) {
                fail(&format!("{}: {e}", Paths::visibility().display()));
            }
            println!("{}", if hide { "islands hidden" } else { "islands shown" });
        }
        "clear" => {
            let mut n = 0;
            // The plan lists stay (Goals, Scheduled, Settled hold the user's goals and plans; `islands plan` edits them).
            for (id, _) in all_islands().into_iter().filter(|(id, _)| !PlanList::ALL.contains(&id.as_str())) {
                if remove_item(&Paths::file(&id)) {
                    n += 1;
                }
            }
            if let Ok(r) = fs::read_dir(Paths::responses()) {
                for e in r.flatten() {
                    remove_item(&e.path());
                }
            }
            println!("cleared {n} island{}", plural(n));
        }
        "state" => state(&args),
        "quit" => quit(),
        "rm" => {
            let all = args.len() == 1 && args[0] == "--all";
            // A plan list is never removed here (re-review 31 Oct: `rm goals`, then a `put`, left an empty Goals): its
            // items are edited with `islands plan`, and `--all` leaves the plan lists.
            if let Some(p) = args.iter().find(|a| PlanList::ALL.contains(&a.as_str())) {
                fail(&format!("islands rm: {p} is a plan list; edit it with islands plan {p} (rm, prune), it is never removed"));
            }
            let ids: Vec<String> =
                if all { all_islands().into_iter().map(|(id, _)| id).filter(|id| !PlanList::ALL.contains(&id.as_str())).collect() } else { args.clone() };
            if ids.is_empty() && !all {
                fail("islands rm <id>... | --all");
            }
            let n = ids.iter().filter(|id| remove_item(&Paths::file(id))).count();
            println!("removed {n} island{}", plural(n));
        }
        "list" => {
            let all = all_islands();
            if args.iter().any(|a| a == "--json") {
                let v = Value::Array(all.into_iter().map(|(_, o)| Value::Object(o)).collect());
                println!("{}", jsonfmt::to_string(&v, false));
            } else if all.is_empty() {
                println!("no islands");
            } else {
                for (id, o) in &all {
                    println!("{}{}", summary(o, id), state_line(id));
                }
            }
        }
        "responses" => {
            let urls = json_files(&Paths::responses());
            let items: Vec<Json> = urls.iter().filter_map(|u| IslandFile::read(u)).collect();
            if args.iter().any(|a| a == "--json") {
                let v = Value::Array(items.into_iter().map(Value::Object).collect());
                println!("{}", jsonfmt::to_string(&v, false));
            } else if items.is_empty() {
                println!("no responses");
            } else {
                for o in &items {
                    let d = |k: &str| o.get(k).map(text::describe);
                    println!(
                        "{} · {} · {}",
                        d("island").unwrap_or_else(|| "?".into()),
                        d("kind").unwrap_or_else(|| "?".into()),
                        d("action").or_else(|| d("value")).unwrap_or_default()
                    );
                }
            }
            if args.iter().any(|a| a == "--consume") {
                for u in &urls {
                    remove_item(u);
                }
            }
        }
        "todo" | "plan" => plan::run(&cmd, args),
        "dir" => println!("{}", Paths::root().display()),
        "-h" | "--help" | "help" => println!("{}", usage::all()),
        _ => fail(&format!("islands: unknown command {cmd}\n\n{}", usage::all())),
    }
    0
}

fn move_island(args: &[String]) {
    if args.len() < 2 {
        fail("islands move <id> X Y | <id> --side left|right|top | <id> --auto");
    }
    let id = &args[0];
    let done = IslandFile::patch(id, |o| {
        o.remove("edgeY");
        if args[1] == "--auto" {
            o.remove("position");
            o.remove("side");
            return;
        }
        if args[1] == "--side" {
            if args.len() < 3 || !crate::model::Dock::SIDES.contains(&args[2].as_str()) {
                fail("islands move <id> --side left|right|top");
            }
            o.remove("position");
            o.insert("side".into(), Value::from(args[2].clone()));
            return;
        }
        let xy = (args.get(1).and_then(|v| text::swift_double(v)), args.get(2).and_then(|v| text::swift_double(v)));
        let (Some(x), Some(y)) = xy else { fail("islands move <id> X Y") };
        o.insert("position".into(), Value::Object(j::object([("x", j::number(x)), ("y", j::number(y))])));
        o.remove("side");
    });
    let Some(done) = done else { fail(&format!("islands move: no island {id}")) };
    println!("moved {}", summary(&done, id));
}

fn view(args: &[String]) {
    let usage = "islands view <path> [--id ID] [--title T] [--action \"Label=action\"]...";
    let Some(path) = args.first().filter(|p| !p.starts_with('-')) else { fail(usage) };
    let p = ns_path(&expand_tilde(path));
    let abs = if p.starts_with('/') { standardize(&p) } else { standardize(&format!("{}/{p}", cwd())) };
    if !Path::new(&abs).exists() {
        fail(&format!("islands view: no file {path}"));
    }
    let opt = |k: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned();
    // Proposed actions (viewer): each --action replaces the list; none given keeps what is there.
    let actions: Vec<Value> = (0..args.len())
        .filter(|&i| args[i] == "--action" && i + 1 < args.len())
        .filter_map(|i| ViewerAction::from_cli(&args[i + 1]))
        .map(|a| Value::Object(a.json()))
        .collect();
    let (id, o) = IslandFile::view(&abs, opt("--id"), opt("--title"), actions);
    println!("view {}", summary(&o, &id));
}

fn state(args: &[String]) {
    let st = screen_state();
    let pick = args.iter().find(|a| !a.starts_with('-'));
    if args.iter().any(|a| a == "--json") {
        let sel = match pick {
            Some(p) => st.get(p).cloned().unwrap_or(Value::Null),
            None => Value::Object(st.clone()),
        };
        println!("{}", jsonfmt::to_string(&sel, false));
    } else {
        let ids: Vec<String> = match pick {
            Some(p) => vec![p.clone()],
            None => {
                let mut k: Vec<String> = st.keys().cloned().collect();
                k.sort();
                k
            }
        };
        if ids.is_empty() {
            println!("nothing on screen");
        }
        for id in ids {
            println!("{id}{}", state_line(&id));
        }
    }
}

/// The app's pid from `.app.pid`. A pid of 1 or below is never the app: kill(0) would reach this whole process group,
/// kill(-1) every process the user owns (the Swift CLI signals them; this one says the app is not running).
fn app_pid(text: &str) -> Option<i32> {
    text.trim().parse::<i32>().ok().filter(|p| *p > 1)
}

fn quit() {
    // SAFETY: signal 0 only checks that the process exists.
    let alive = |pid: i32| unsafe { libc::kill(pid, 0) } == 0;
    let pid = fs::read_to_string(Paths::pid()).ok().and_then(|s| app_pid(&s));
    let Some(pid) = pid.filter(|p| alive(*p)) else {
        println!("app not running");
        return;
    };
    // SAFETY: plain signals to the app's pid.
    unsafe { libc::kill(pid, libc::SIGTERM) };
    for _ in 0..100 {
        if !alive(pid) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    if alive(pid) {
        println!("app still running (pid {pid})");
    } else {
        println!("app quit");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quit_never_signals_a_process_group_or_launchd() {
        assert_eq!(app_pid("4242\n"), Some(4242));
        assert_eq!(app_pid("0"), None);
        assert_eq!(app_pid("-1"), None);
        assert_eq!(app_pid("1"), None);
        assert_eq!(app_pid("soon"), None);
    }
}
