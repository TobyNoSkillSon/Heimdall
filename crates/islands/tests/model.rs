//! Swift `IslandModelTests/ModelTests.swift`: placement, snapping, steps and targets, looks, merge, malformed values.

use islands::jobj;
use islands::model::island::{host, Target};
use islands::model::*;

fn sz(w: f64, h: f64) -> Size {
    Size::new(w, h)
}
fn r(x: f64, y: f64, w: f64, h: f64) -> Rect {
    Rect::new(x, y, w, h)
}
fn auto(sizes: &[Size], width: f64, top: f64, bottom: f64, obstacles: &[Rect]) -> Vec<Rect> {
    auto_column(sizes, width, top, bottom, 8.0, 8.0, obstacles)
}
fn edge(sizes: &[Size], pins: &[Option<f64>], width: f64, top: f64, highest: f64, bottom: f64) -> Vec<Rect> {
    edge_column(sizes, pins, width, top, highest, bottom, 8.0, 8.0, &[])
}
const SCREEN: Rect = Rect::new(0.0, 0.0, 1728.0, 1117.0);
const VISIBLE: Rect = Rect::new(0.0, 70.0, 1728.0, 1015.0);
fn snap14(x: Rect, neighbours: &[Rect]) -> Rect {
    snap(x, SCREEN, VISIBLE, 1079.0, neighbours, 8.0, 14.0)
}

#[test]
fn column_stacks_down_the_right_edge() {
    let c = auto(&[sz(100.0, 36.0), sz(140.0, 36.0)], 1000.0, 120.0, 900.0, &[]);
    assert_eq!(c[0], r(892.0, 120.0, 100.0, 36.0));
    assert_eq!(c[1], r(852.0, 164.0, 140.0, 36.0), "right-aligned, 8 pt below");
}

#[test]
fn column_continues_to_the_left_when_full() {
    let c = auto(&[sz(100.0, 36.0); 3], 1000.0, 100.0, 190.0, &[]);
    assert_eq!(c.iter().map(|x| x.min_x()).collect::<Vec<_>>(), [892.0, 892.0, 784.0]);
    assert_eq!(c[2].min_y(), 100.0);
}

#[test]
fn column_steps_past_placed_islands() {
    // The user put something where the second slot would be: it goes under it.
    let placed = r(850.0, 160.0, 150.0, 120.0);
    let c = auto(&[sz(100.0, 36.0), sz(100.0, 36.0)], 1000.0, 120.0, 900.0, &[placed]);
    assert_eq!(c[0].min_y(), 120.0);
    assert!(!c[0].intersects(&placed));
    assert_eq!(c[1].min_y(), 288.0);
}

#[test]
fn steps_and_targets_parse() {
    assert_eq!(Step::parse("Download=done").0, "Download");
    assert_eq!(Step::parse("Download=done").1, "done");
    assert_eq!(Step::parse("Convert = now").1, "now");
    assert_eq!(Step::parse("a=b").0, "a=b");
    assert_eq!(Step::parse("Install").1, "todo");
    let Some(Target::Url(u)) = OpenTarget::url("https://example.com/x") else { panic!("a URL") };
    assert_eq!(host(&u).as_deref(), Some("example.com"));
    assert!(matches!(OpenTarget::url("/tmp/compare3.html"), Some(Target::File(_))));
    assert_eq!(OpenTarget::url("example.com"), Some(Target::Url("https://example.com".into())));
    assert_eq!(OpenTarget::label("/tmp/compare3.html"), "compare3.html");
    assert_eq!(OpenTarget::label("https://www.example.com/a"), "example.com");
    assert_eq!(Island::new("x", jobj! {"open": ""}).open(), None);
    assert!(OpenTarget::missing("/nonexistent/file.html"));
}

#[test]
fn snap_never_enters_the_menu_bar() {
    assert_eq!(snap14(r(600.0, 1100.0, 120.0, 28.0), &[]).max_y(), 1079.0);
}

#[test]
fn snap_to_corner_and_neighbour() {
    let corner = snap14(r(1590.0, 90.0, 120.0, 28.0), &[]);
    assert_eq!(corner.origin(), Point::new(1600.0, 78.0));
    let n = r(300.0, 1051.0, 100.0, 28.0);
    assert_eq!(snap14(r(420.0, 1040.0, 120.0, 28.0), &[n]).origin(), Point::new(408.0, 1051.0));
    assert_eq!(snap14(r(310.0, 1005.0, 80.0, 28.0), &[n]).origin(), Point::new(310.0, 1015.0), "centred under it");
}

#[test]
fn snap_never_drops_onto_a_neighbour() {
    let n = r(600.0, 600.0, 300.0, 28.0);
    let s = snap14(r(700.0, 600.0, 100.0, 28.0), &[n]);
    assert!(!s.intersects(&n.inset(1.0, 1.0)));
    assert_eq!(s.min_x(), 908.0);
}

#[test]
fn snap_leaves_free_placements_alone() {
    let x = r(700.0, 500.0, 120.0, 28.0);
    assert_eq!(snap14(x, &[]), x);
}

#[test]
fn look_follows_state() {
    let look = |o: Json| Island::new("a", o).look().to_owned();
    assert_eq!(look(jobj! {"preset": "task", "state": "done"}), "result");
    assert_eq!(look(jobj! {"preset": "task", "state": "waiting"}), "waiting");
    assert_eq!(look(jobj! {"preset": "list", "state": "failed"}), "failed");
    assert_eq!(look(jobj! {"preset": "waiting", "state": "done"}), "result");
    assert_eq!(look(jobj! {"preset": "bogus"}), "task");
    assert_eq!(look(jobj! {"preset": "note"}), "note");
}

#[test]
fn merge_removes_null_and_keeps_position() {
    let base = jobj! {"title": "A", "position": {"x": 1, "y": 2}, "progress": 0.5};
    let m = IslandFile::merge(&base, &jobj! {"progress": null, "title": "B"});
    assert!(m.get("progress").is_none());
    assert_eq!(m["title"], "B");
    assert!(Island::new("a", m).position().is_some());
}

#[test]
fn malformed_values_are_clamped() {
    let i = Island::new("a", jobj! {"progress": 7, "position": {"x": "no"}, "parts": "nope"});
    assert_eq!(i.progress(), Some(1.0));
    assert_eq!(i.position(), None);
    assert!(i.parts().is_empty());
    assert_eq!(Island::format(2.10), "2.1");
    assert_eq!(Island::format(412.0), "412");
}

#[test]
fn safe_id_cannot_escape_the_folder() {
    assert!(!Paths::safe_id("../../etc/passwd").contains('/'));
}

#[test]
fn plain_items_keep_order_and_skip_blanks() {
    let i = Island::new("a", jobj! {"items": ["One", "", {"title": "Two"}, 3, "Three"]});
    assert_eq!(i.items(), ["One", "Two", "Three"]);
}

#[test]
fn open_island_pushes_the_column_down() {
    let (pill, open) = (sz(120.0, 36.0), sz(360.0, 240.0));
    let closed = auto(&[pill, pill, pill, pill], 1500.0, 100.0, 900.0, &[]);
    let opened = auto(&[pill, open, pill, pill], 1500.0, 100.0, 900.0, &[]);
    assert_eq!(opened[2].min_y() - closed[2].min_y(), open.h - pill.h);
    for k in 0..3 {
        assert!(!opened[k].intersects(&opened[k + 1]));
    }
}

#[test]
fn left_column_mirrors_the_right() {
    let c = left_column(&[sz(100.0, 36.0), sz(140.0, 36.0)], 1000.0, 120.0, 900.0, 8.0, 8.0, &[]);
    assert_eq!(c[0], r(8.0, 120.0, 100.0, 36.0));
    assert_eq!(c[1], r(8.0, 164.0, 140.0, 36.0), "left-aligned, 8 pt below");
}

#[test]
fn left_column_pushes_down_when_one_opens() {
    let closed = left_column(&[sz(100.0, 36.0), sz(100.0, 36.0)], 1000.0, 120.0, 900.0, 8.0, 8.0, &[]);
    let open = left_column(&[sz(360.0, 300.0), sz(100.0, 36.0)], 1000.0, 120.0, 900.0, 8.0, 8.0, &[]);
    assert_eq!(closed[1].min_y(), 164.0);
    assert_eq!(open[1].min_y(), 428.0);
    assert!(!open[0].intersects(&open[1]));
}

#[test]
fn top_row_is_centred_on_the_notch_and_never_overlaps() {
    let row = top_row(&[sz(100.0, 36.0), sz(140.0, 36.0)], 1728.0, 44.0, None, 8.0, 8.0);
    assert_eq!(row[0], r(740.0, 44.0, 100.0, 36.0));
    assert_eq!(row[1], r(848.0, 44.0, 140.0, 36.0));
    let wide = top_row(&[sz(500.0, 36.0); 4], 1728.0, 44.0, None, 8.0, 8.0);
    assert_eq!(wide[3].min_y(), 88.0, "a full row continues below");
    for a in 0..4 {
        for b in a + 1..4 {
            assert!(!wide[a].intersects(&wide[b]));
        }
    }
}

#[test]
fn todo_items_group_and_tick() {
    assert_eq!(TodoGroup::parse("Echo bug=waiting").1.as_deref(), Some("waiting"));
    assert!(TodoGroup::parse("Echo bug=done").2);
    assert_eq!(TodoGroup::parse("Arsenal: a=b").0, "Arsenal: a=b");
    let i = Island::new(
        "open",
        jobj! {"preset": "todo", "items": ["Plain", {"title": "Port", "group": "running"}, {"title": "Test", "group": "waiting", "done": true}]},
    );
    let g = i.grouped_items().unwrap();
    assert_eq!(g.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(), ["running", "waiting", "discussed"]);
    assert_eq!(g.last().unwrap().1[0].title, "Plain");
    assert_eq!(i.side().as_deref(), Some("left"));
    assert!(!i.waits_on_owner(), "the only waiting item is ticked off");
    assert!(Island::new("x", jobj! {"items": ["a", "b"]}).grouped_items().is_none(), "a plain list stays plain");
    assert_eq!(Island::new("x", jobj! {"position": {"x": 10, "y": 20}, "side": "left"}).side(), None);
}

#[test]
fn edge_column_keeps_pinned_heights_and_flows_around() {
    let s = [sz(100.0, 36.0); 4];
    // Third island pinned at 140: the auto ones before it step past, the one after continues below.
    let c = edge(&s, &[None, None, Some(140.0), None], 1000.0, 120.0, 40.0, 900.0);
    assert_eq!(c.iter().map(|x| x.min_y()).collect::<Vec<_>>(), [120.0, 208.0, 164.0, 252.0], "the pin sits right under the island above it");
    // A pin level with or above a slot: the stack steps past it and the pin keeps its height.
    let p = edge(&s, &[None, None, Some(110.0), None], 1000.0, 120.0, 40.0, 900.0);
    assert_eq!(p.iter().map(|x| x.min_y()).collect::<Vec<_>>(), [154.0, 198.0, 110.0, 242.0]);
    for a in 0..4 {
        for b in a + 1..4 {
            assert!(!c[a].inset(1.0, 1.0).intersects(&c[b]), "{a} {b}");
        }
    }
    assert!(c.iter().all(|x| x.max_x() == 992.0));
    // Pins clamp to the screen: under the menu bar, above the bottom.
    let cl = edge(&s[..2], &[Some(0.0), Some(2000.0)], 1000.0, 120.0, 40.0, 900.0);
    assert_eq!(cl[0].min_y(), 40.0);
    assert_eq!(cl[1].max_y(), 900.0);
    // Two pinned at the same height: no overlap.
    let d = edge(&s[..2], &[Some(500.0), Some(500.0)], 1000.0, 120.0, 40.0, 900.0);
    assert_eq!(d[0].min_y(), 500.0);
    assert_eq!(d[1].min_y(), 544.0);
    // An open island grows over a pinned one below: it moves down; overflow at the bottom sweeps back up.
    let e = edge(&[sz(300.0, 400.0), s[0]], &[None, Some(300.0)], 1000.0, 120.0, 40.0, 900.0);
    assert_eq!(e[1].min_y(), 528.0);
    let f = edge(&s[..2], &[Some(880.0), Some(880.0)], 1000.0, 120.0, 40.0, 900.0);
    assert_eq!(f[1].max_y(), 900.0);
    assert_eq!(f[0].max_y(), 856.0);
}

#[test]
fn left_edge_column_mirrors() {
    let c = left_edge_column(&[sz(100.0, 36.0)], &[Some(300.0)], 1000.0, 120.0, 40.0, 900.0, 8.0, 8.0, &[]);
    assert_eq!(c[0], r(8.0, 300.0, 100.0, 36.0));
}

#[test]
fn edge_drop_goes_to_the_nearer_side_at_its_height() {
    let x = r(700.0, 500.0, 100.0, 36.0);
    let drop = |x: Rect, v: Vector| edge_drop(x, v, SCREEN, 1079.0, 78.0, 500.0);
    let slow = drop(x, Vector::ZERO);
    assert_eq!(slow.0, "left");
    assert_eq!(slow.1, 536.0);
    assert_eq!(drop(x.offset(300.0, 0.0), Vector::ZERO).0, "right");
    // A flick carries on along its path: thrown right and up, it lands right and higher, never in the menu bar.
    let flick = drop(x, Vector::new(2000.0, 200.0));
    assert_eq!(flick.0, "right");
    assert!((flick.1 - 636.0).abs() <= 1.0);
    assert_eq!(drop(x, Vector::new(0.0, 5000.0)).1, 1079.0);
    assert_eq!(drop(x, Vector::new(0.0, -5000.0)).1, 114.0);
}

#[test]
fn a_column_starting_low_moves_up_only_as_far_as_a_tall_stack_needs() {
    // A 1117 pt display: the column starts 60 % up from the bottom (447 pt from the top), the room ends at 1076.
    let (top, highest, bottom) = (447.0, 39.0, 1076.0);
    let short = edge(&[sz(220.0, 36.0), sz(220.0, 36.0)], &[None, None], 1728.0, top, highest, bottom);
    assert_eq!(short.iter().map(|r| r.y).collect::<Vec<_>>(), [447.0, 491.0], "a short stack starts at 40 % down");
    // An open list of 800 pt and a pill under it: lifted just enough that the pill's bottom meets the room's end.
    let tall = edge(&[sz(300.0, 800.0), sz(220.0, 36.0)], &[None, None], 1728.0, top, highest, bottom);
    assert_eq!(tall[1].max_y(), bottom);
    assert_eq!(tall[0].y, bottom - 36.0 - 8.0 - 800.0);
    // Taller than the room (never in practice: the height cap is lower): it stops under the menu bar.
    let huge = edge(&[sz(300.0, 1100.0)], &[None], 1728.0, top, highest, bottom);
    assert_eq!(huge[0].y, highest);
}

#[test]
fn visibility_is_shown_unless_the_flag_says_hidden() {
    use islands::model::Visibility;
    let o = |v: serde_json::Value| v.as_object().cloned();
    assert!(!Visibility::hidden_in(None), "no file: shown");
    assert!(!Visibility::hidden_in(o(serde_json::json!({})).as_ref()));
    assert!(!Visibility::hidden_in(o(serde_json::json!({"hidden": "yes"})).as_ref()), "only a boolean hides");
    assert!(!Visibility::hidden_in(o(serde_json::json!({"hidden": false})).as_ref()));
    assert!(Visibility::hidden_in(o(serde_json::json!({"hidden": true})).as_ref()));
    assert!(Visibility::hidden_in(o(serde_json::json!({"hidden": 1})).as_ref()), "as Swift's `as? Bool` reads 1");
}

/// Heimdall's Show Islands on Streams (`streams.json`, written by crates/app `islands::streams`): only `"shown": true`
/// (or 1) shows the islands in captures; missing, false or malformed keeps them out, the default.
#[test]
fn streams_flag_is_shown_only_when_true() {
    use islands::model::file::Streams;
    assert!(!Streams::shown_in(None), "no file: out of captures");
    assert!(Streams::shown_in(Some(&jobj! {"shown": true})));
    assert!(Streams::shown_in(Some(&jobj! {"shown": 1})));
    assert!(!Streams::shown_in(Some(&jobj! {"shown": false})));
    assert!(!Streams::shown_in(Some(&jobj! {"shown": "yes"})));
    assert!(!Streams::shown_in(Some(&jobj! {"hidden": true})), "another file's key");
}

/// Bug 24 Oct: an island's open button on a `.md` opened Xcode. Every kind the viewer shows goes there, the rest to the system.
#[test]
fn open_buttons_route_text_files_to_the_viewer() {
    let d = std::env::temp_dir().join(format!("islands-route-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    let f = |n: &str, body: &[u8]| {
        let p = d.join(n);
        std::fs::write(&p, body).unwrap();
        p.to_string_lossy().into_owned()
    };
    let (md, txt, rs, html, png) =
        (f("plan.md", b"# Plan"), f("notes.txt", b"hi"), f("main.rs", b"fn main() {}"), f("report.html", b"<h1>r</h1>"), f("a.png", &[0x89, b'P', b'N', b'G', 0, 0]));
    // Pictures and HTML reports too (30 Oct): everything the island viewer can show opens there.
    for p in [&md, &txt, &rs, &png, &html] {
        assert_eq!(OpenTarget::route(p), Some(OpenRoute::Viewer(p.clone())), "{p}");
    }
    let enc = format!("file://{}", md.replace("plan.md", "plan%2Emd"));
    assert_eq!(OpenTarget::route(&enc), Some(OpenRoute::Viewer(md.clone())));
    assert_eq!(OpenTarget::route("https://example.com/a.md"), Some(OpenRoute::System(Target::Url("https://example.com/a.md".into()))));
    let gone = d.join("gone.md").to_string_lossy().into_owned();
    assert_eq!(OpenTarget::route(&gone), None, "a missing file opens nowhere (the button says Not found)");
    assert_eq!(OpenTarget::route(""), None);
    let _ = std::fs::remove_dir_all(&d);
}

/// A running task's island carries its report's button before the page exists.
#[test]
fn a_page_to_come_keeps_its_button_while_the_work_runs() {
    let d = std::env::temp_dir().join(format!("islands-button-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    let page = d.join("report.html").to_string_lossy().into_owned();
    let b = |running| OpenTarget::button(&page, running);
    assert_eq!(b(true), OpenButton { label: "report.html".into(), ready: false, live: true });
    assert_eq!(b(false), OpenButton { label: "Not found: report.html".into(), ready: false, live: false });
    std::fs::write(&page, "<h1>r</h1>").unwrap();
    for running in [true, false] {
        assert_eq!(b(running), OpenButton { label: "report.html".into(), ready: true, live: true });
    }
    assert!(OpenTarget::button("https://example.com", false).live);
    assert!(!OpenTarget::button("not a target", true).live);
    let _ = std::fs::remove_dir_all(&d);
}
