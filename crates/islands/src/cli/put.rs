//! `islands put <id> [options]`: create or update one island in one call.

use std::io::Read;

use serde_json::Value;

use super::{absolute, fail, resolve_target, stamp, summary, text, usage};
use crate::model::{j, Dock, IslandFile, Json, Paths, Presets, Step, TodoGroup};

fn parse_json(s: &str) -> Value {
    serde_json::from_str(s).unwrap_or_else(|_| fail(&format!("islands: invalid JSON: {}", text::prefix(s, 80))))
}

/// The checkable half of docs/island-style.md, applied to what this call sets (old files are left alone).
pub(crate) fn style_problem(o: &Json, over: &Json, id: &str, new: bool) -> Option<String> {
    if new && j::str(o.get("title")).unwrap_or("").is_empty() && j::str(o.get("preset")) != Some("viewer") {
        return Some("a new island needs --title: 1-3 words naming the thing (\"Qwen-Image\", \"iPad sale\")".into());
    }
    if let Some(t) = j::str(over.get("title")) {
        let n = text::words(t);
        if !(1..=3).contains(&n) {
            return Some(format!("--title is 1-3 words naming the thing; \"{t}\" has {n}"));
        }
        let norm = |s: &str| s.to_lowercase().replace('_', "-").replace(' ', "-");
        if t == t.to_lowercase() && norm(t) == norm(id) {
            return Some(format!("--title names the thing for User, not an id: \"{t}\""));
        }
    }
    if let Some(s) = j::str(over.get("stage")) {
        if text::words(s) > 2 {
            return Some(format!(
                "--stage is one or two present-tense words (\"Downloading\", \"Final tests\"); \"{s}\" has {}",
                text::words(s)
            ));
        }
    }
    if let Some(s) = j::str(over.get("summary")) {
        if s.contains('\n') {
            return Some("--summary is one line".into());
        }
        let n = text::count(s);
        if n > 110 {
            return Some(format!("--summary is one plain line (110 characters at most); this one has {n}"));
        }
    }
    None
}

fn item_object(t: &str) -> Value {
    let (title, group, done) = TodoGroup::parse(t);
    if group.is_none() && !done {
        return Value::from(title);
    }
    let mut o = Json::new();
    o.insert("title".into(), Value::from(title));
    if let Some(g) = group {
        o.insert("group".into(), Value::from(g));
    }
    if done {
        o.insert("done".into(), Value::Bool(true));
    }
    Value::Object(o)
}

pub fn run(mut args: Vec<String>) {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{}", usage::PUT);
        std::process::exit(0);
    }
    let Some(id) = args.first().filter(|a| !a.starts_with('-')).cloned() else {
        fail(&format!("islands put: missing <id>\n\n{}", usage::PUT))
    };
    args.remove(0);
    // Plan lists are written by `islands plan` (and Heimdall's plan tools), never by put (re-review 31 Oct: put recreated
    // a removed Goals empty).
    if crate::model::PlanList::ALL.contains(&id.as_str()) {
        fail(&format!("islands put: {id} is a plan list; use islands plan {id} …"));
    }
    let mut over = Json::new();
    let (mut steps, mut facts, mut items): (Vec<Value>, Vec<Value>, Vec<String>) = (vec![], vec![], vec![]);
    let (mut reset_steps, mut reset_facts, mut reset_items) = (false, false, false);
    let null = || Value::Null;
    let mut i = 0;
    let next = |i: &mut usize| -> String {
        *i += 1;
        if *i >= args.len() {
            fail(&format!("islands put: {} needs a value", args[*i - 1]));
        }
        args[*i].clone()
    };
    while i < args.len() {
        let a = args[i].clone();
        let mut set = |k: &str, v: Value| {
            over.insert(k.to_owned(), v);
        };
        match a.as_str() {
            "--preset" => {
                let p = next(&mut i);
                if !Presets::ALL.contains(&p.as_str()) {
                    fail(&format!("islands put: preset must be one of {}", Presets::ALL.join(", ")));
                }
                set("preset", Value::from(p));
            }
            "--title" => set("title", Value::from(next(&mut i))),
            "--side" => {
                let v = next(&mut i);
                if !Dock::SIDES.contains(&v.as_str()) {
                    fail("islands put: --side takes left, right or top");
                }
                set("side", Value::from(v));
                set("position", null());
                set("edgeY", null());
            }
            "--state" => {
                let s = next(&mut i);
                if !Presets::STATES.contains(&s.as_str()) {
                    fail(&format!("islands put: state must be one of {}", Presets::STATES.join(", ")));
                }
                set("state", Value::from(s));
            }
            "--progress" => {
                let v = next(&mut i);
                if v == "none" {
                    set("progress", null());
                } else if let Some(d) = text::swift_double(&v) {
                    set("progress", j::number(d));
                } else {
                    fail("islands put: --progress takes 0..1 or none");
                }
            }
            "--value" => set("value", Value::from(next(&mut i))),
            "--stage" => set("stage", Value::from(next(&mut i))),
            "--path" => set("path", Value::from(next(&mut i))),
            "--icon" => set("icon", Value::from(next(&mut i))),
            "--open" => {
                let t = next(&mut i);
                set("open", if t.is_empty() { null() } else { Value::from(resolve_target(&t)) });
            }
            "--summary" => {
                let t = next(&mut i);
                set("summary", if t.is_empty() { null() } else { Value::from(t) });
            }
            "--image" => {
                let t = next(&mut i);
                set("image", if t.is_empty() { null() } else { Value::from(absolute(&t)) });
            }
            "--step" => {
                let t = next(&mut i);
                if !reset_steps {
                    reset_steps = true;
                    steps.clear();
                }
                if t != "none" && !t.is_empty() {
                    let (title, mark) = Step::parse(&t);
                    steps.push(Value::Object(j::object([("title", Value::from(title)), ("state", Value::from(mark))])));
                }
                set("steps", if steps.is_empty() { null() } else { Value::Array(steps.clone()) });
            }
            "--item" => {
                let t = next(&mut i);
                if !reset_items {
                    reset_items = true;
                    items.clear();
                }
                if t != "none" && !t.is_empty() {
                    items.push(t);
                }
                set("items", if items.is_empty() { null() } else { Value::Array(items.iter().map(|t| item_object(t)).collect()) });
            }
            "--kv" => {
                let t = next(&mut i);
                if !reset_facts {
                    reset_facts = true;
                    facts.clear();
                }
                if t != "none" && !t.is_empty() {
                    let Some(eq) = t.find('=') else { fail("islands put: --kv takes Key=Value") };
                    facts.push(Value::Object(j::object([
                        ("label", Value::from(text::trim_spaces(&t[..eq]))),
                        ("value", Value::from(text::trim_spaces(&t[eq + 1..]))),
                    ])));
                }
                set("kv", if facts.is_empty() { null() } else { Value::Array(facts.clone()) });
            }
            "--parts" => {
                let Value::Array(p) = parse_json(&next(&mut i)) else { fail("islands put: --parts takes a JSON array") };
                set("parts", Value::Array(p));
            }
            "--at" => {
                let v = next(&mut i);
                let xy: Vec<f64> =
                    v.split(',').filter(|s| !s.is_empty()).filter_map(|s| text::swift_double(text::trim_spaces(s))).collect();
                if xy.len() != 2 {
                    fail("islands put: --at takes X,Y");
                }
                set("position", Value::Object(j::object([("x", j::number(xy[0])), ("y", j::number(xy[1]))])));
                set("side", null());
                set("edgeY", null());
            }
            "--auto" => {
                set("position", null());
                set("side", null());
                set("edgeY", null());
            }
            "--expanded" => set("expanded", Value::Bool(true)),
            "--collapsed" => set("expanded", Value::Bool(false)),
            "--order" => {
                let Some(d) = text::swift_double(&next(&mut i)) else { fail("islands put: --order takes a number") };
                set("order", j::number(d));
                set("edgeY", null());
            }
            "--from-notch" => set("origin", Value::Object(j::object([("from", Value::from("notch"))]))),
            "--json" | "-" => {
                let text = if a == "-" {
                    let mut b = vec![];
                    let _ = std::io::stdin().read_to_end(&mut b);
                    String::from_utf8(b).unwrap_or_default()
                } else {
                    next(&mut i)
                };
                let Value::Object(o) = parse_json(&text) else { fail("islands put: --json takes a JSON object") };
                // Keep nulls: they delete keys in the final merge.
                for (k, v) in o {
                    set(&k, v);
                }
            }
            _ => fail(&format!("islands put: unknown option {a}\n\n{}", usage::PUT)),
        }
        i += 1;
    }
    let url = Paths::file(&id);
    let out = IslandFile::locked(|| {
        let old = IslandFile::read(&url);
        let base = old.clone().unwrap_or_else(|| j::object([("id", Value::from(id.clone()))]));
        let mut out = IslandFile::merge(&base, &over);
        out.insert("id".into(), Value::from(id.clone()));
        if let Some(problem) = style_problem(&out, &over, &id, old.is_none()) {
            fail(&format!("islands put: {problem}\nSee docs/island-style.md or islands put --help."));
        }
        stamp(&mut out, old.as_ref());
        if let Some(p) = j::str(out.get("preset")) {
            if !Presets::ALL.contains(&p) {
                fail(&format!("islands put: unknown preset {p}"));
            }
        }
        if let Err(e) = IslandFile::write_atomic(&out, &url) {
            fail(&format!("islands put: {e}"));
        }
        out
    });
    println!("put {}", summary(&out, &id));
}
