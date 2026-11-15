//! The viewer island resizes like a window (bug note 2026-11-07T01-40-23): its edges and corners, the rect a drag
//! makes, and the size kept per file.

use islands::model::geometry::{Point, Rect, Size};
use islands::model::viewer_size::{
    asked_island, centred_origin, content_limits, edges_at, fit, hand_size_stands, page_ask, page_ask_of, remember, remember_asked, remembered,
    remembered_kept, resize, Edges, Kept, Memory, PageAsk, KEEP,
};

const SIZE: Size = Size::new(760.0, 600.0);
const MIN: Size = Size::new(320.0, 200.0);
/// The MacBook's visible area under the menu bar, 8 pt in (y up).
const BOUNDS: Rect = Rect::new(8.0, 8.0, 1712.0, 1040.0);

fn e(left: bool, right: bool, top: bool, bottom: bool) -> Option<Edges> {
    Some(Edges { left, right, top, bottom })
}

#[test]
fn the_edges_and_corners_grab_and_the_middle_does_not() {
    assert_eq!(edges_at(Point::new(3.0, 300.0), SIZE), e(true, false, false, false), "left edge");
    assert_eq!(edges_at(Point::new(756.0, 300.0), SIZE), e(false, true, false, false), "right edge");
    assert_eq!(edges_at(Point::new(380.0, 596.0), SIZE), e(false, false, false, true), "bottom edge");
    assert_eq!(edges_at(Point::new(380.0, 2.0), SIZE), e(false, false, true, false), "top edge, thin");
    assert_eq!(edges_at(Point::new(750.0, 590.0), SIZE), e(false, true, false, true), "bottom-right corner, generous");
    assert_eq!(edges_at(Point::new(4.0, 585.0), SIZE), e(true, false, false, true), "bottom-left corner");
    assert_eq!(edges_at(Point::new(10.0, 2.0), SIZE), e(true, false, true, false), "top-left corner");
    assert_eq!(edges_at(Point::new(752.0, 2.0), SIZE), e(false, true, false, false), "the close button's corner: sideways only");
    assert_eq!(edges_at(Point::new(380.0, 300.0), SIZE), None, "the content");
    assert_eq!(edges_at(Point::new(380.0, 20.0), SIZE), None, "the title row carries the island");
    assert_eq!(edges_at(Point::new(-1.0, 300.0), SIZE), None, "outside");
}

#[test]
fn a_drag_moves_only_the_grabbed_edges() {
    let start = Rect::new(500.0, 300.0, 760.0, 600.0);
    let r = resize(start, Edges { right: true, bottom: true, ..Edges::default() }, 140.0, -100.0, MIN, BOUNDS);
    assert_eq!(r, Rect::new(500.0, 200.0, 900.0, 700.0), "bottom-right corner: wider and taller, the top-left stays");
    let r = resize(start, Edges { left: true, ..Edges::default() }, -60.0, 50.0, MIN, BOUNDS);
    assert_eq!(r, Rect::new(440.0, 300.0, 820.0, 600.0), "left edge: the right edge stays, the height too");
    let r = resize(start, Edges { top: true, ..Edges::default() }, 0.0, 40.0, MIN, BOUNDS);
    assert_eq!(r, Rect::new(500.0, 300.0, 760.0, 640.0), "top edge up");
}

#[test]
fn a_drag_stops_at_the_smallest_size_and_the_display() {
    let start = Rect::new(500.0, 300.0, 760.0, 600.0);
    let r = resize(start, Edges { right: true, bottom: true, ..Edges::default() }, -2000.0, 2000.0, MIN, BOUNDS);
    assert_eq!((r.min_x(), r.max_y(), r.w, r.h), (500.0, 900.0, 320.0, 200.0), "never under the smallest; the top-left stays");
    let r = resize(start, Edges { left: true, top: true, ..Edges::default() }, -2000.0, 2000.0, MIN, BOUNDS);
    assert_eq!((r.min_x(), r.max_y(), r.max_x(), r.min_y()), (8.0, 1048.0, 1260.0, 300.0), "never off the display or under the menu bar");
}

#[test]
fn a_kept_size_fits_the_display_it_opens_on() {
    assert_eq!(fit(Size::new(1400.0, 900.0), MIN, Size::new(1712.0, 1040.0)), Size::new(1400.0, 900.0));
    assert_eq!(fit(Size::new(2400.0, 1300.0), MIN, Size::new(1712.0, 1040.0)), Size::new(1712.0, 1040.0), "from a bigger display");
    assert_eq!(fit(Size::new(100.0, 50.0), MIN, Size::new(1712.0, 1040.0)), MIN);
}

#[test]
fn the_size_is_kept_per_file() {
    let d = std::env::temp_dir().join(format!("viewer-size-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    let store = d.join("viewer-size.json");
    let map = std::path::Path::new("/Users/t/Work/mind-map.html");
    assert_eq!(remembered(&store, map), None, "nothing kept yet: the content's size");
    assert!(remember(&store, map, Size::new(1200.0, 800.0)).unwrap());
    assert!(!remember(&store, map, Size::new(1200.0, 800.0)).unwrap(), "the same size again writes nothing");
    assert_eq!(remembered(&store, map), Some(Size::new(1200.0, 800.0)));
    assert_eq!(remembered(&store, std::path::Path::new("/Users/t/other.md")), None, "per file");
    let mut m = Memory::load(&store);
    for i in 0..KEEP + 5 {
        m.set(&format!("/f{i}"), Size::new(400.0, 300.0));
    }
    assert_eq!(m.len(), KEEP, "the oldest go");
    assert_eq!(m.get(&map.to_string_lossy()), None);
    std::fs::write(&store, b"not json").unwrap();
    assert!(Memory::load(&store).is_empty(), "a broken file is an empty memory");
    let _ = std::fs::remove_dir_all(&d);
}

// ---- a page's own size (13 Nov: a job ad beside a CV, unreadable in a 920 pt viewer) ----

/// The user's MacBook: the display, its room for a viewer under the menu bar (8 pt in), the island's chrome around the
/// page (16 pt each side; a 48 pt title row and 16 pt under the page), the smallest a viewer gets.
const SCREEN: Size = Size::new(1728.0, 1117.0);
const ROOM: Size = Size::new(1712.0, 1117.0 - 38.0 - 6.0 - 8.0);
const EXTRA: Size = Size::new(32.0, 64.0);
const VMIN: Size = Size::new(320.0, 144.0);

fn ask(w: Option<f64>, h: Option<f64>) -> Option<PageAsk> {
    Some(PageAsk { w, h })
}

/// The viewer's island for a page asking `a`, as `FileViewer::island_size` and `Ctl::viewer_size` work it out
/// (an HTML page whose body is as tall as `body_h`, under today's limits of 920 pt and 78 % of the display).
fn island_for(a: Option<PageAsk>, body_h: f64) -> Size {
    let default_w = (SCREEN.w * 0.56).max(480.0).min(920.0) - EXTRA.w;
    let default_h = SCREEN.h * 0.78 - EXTRA.h;
    let lim = content_limits(a, default_w, default_h, SCREEN, EXTRA);
    let h = if a.is_some_and(|a| a.h.is_some()) { lim.h } else { lim.h.min(body_h) };
    asked_island(Size::new(lim.w + EXTRA.w, h + EXTRA.h), a, VMIN, ROOM)
}

#[test]
fn a_page_states_its_size_in_a_meta_tag() {
    let head = |m: &str| format!("<!doctype html><html><head><meta charset=utf-8>{m}<title>x</title></head><body>hi</body></html>");
    assert_eq!(page_ask(&head(r#"<meta name="heimdall-viewer" content="width=1500, height=1000">"#)), ask(Some(1500.0), Some(1000.0)));
    assert_eq!(page_ask(&head(r#"<META Name='Heimdall-Viewer' Content='Width = 1400px; height=900'>"#)), ask(Some(1400.0), Some(900.0)), "case, quotes, spaces, px");
    assert_eq!(page_ask(&head(r#"<meta content="width=1300" name=heimdall-viewer />"#)), ask(Some(1300.0), None), "attribute order, bare name, width alone");
    assert_eq!(page_ask(&head(r#"<meta name=heimdall-viewer content=width=1300/>"#)), ask(Some(1300.0), None), "unquoted, self-closed");
    assert_eq!(page_ask(&head(r#"<meta name="heimdall-viewer" content="height=700">"#)), ask(None, Some(700.0)), "height alone");
    assert_eq!(page_ask(&head(r#"<meta name="viewport" content="width=device-width">"#)), None, "the viewport tag is not a request");
    assert_eq!(page_ask(&head("")), None, "no tag: nothing asked");
    assert_eq!(page_ask(&head(r#"<meta name="heimdall-viewer" content="width=wide, height=-5">"#)), None, "nonsense is not a request");
    assert_eq!(page_ask(&head(r#"<meta name="heimdall-viewer" content="width=99999">"#)), None, "over 10,000 is not a number a page means");
    let late = format!("<html><head></head><body>{}<meta name=\"heimdall-viewer\" content=\"width=1500\"></body></html>", "x".repeat(70_000));
    assert_eq!(page_ask(&late), None, "only the page's start is read");
    let polish = format!("<head><title>Inżynier ąę</title>{}<meta name=heimdall-viewer content=width=1200></head>", "ż".repeat(100));
    assert_eq!(page_ask(&polish), ask(Some(1200.0), None), "multibyte text before the tag");
    let d = std::env::temp_dir().join(format!("viewer-ask-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    let f = d.join("review.html");
    std::fs::write(&f, head(r#"<meta name="heimdall-viewer" content="width=1500,height=1000">"#)).unwrap();
    assert_eq!(page_ask_of(&f), ask(Some(1500.0), Some(1000.0)));
    assert_eq!(page_ask_of(&d.join("gone.html")), None);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_page_that_asks_nothing_opens_as_before() {
    assert_eq!(island_for(None, 3000.0), Size::new(920.0, (1117.0f64 * 0.78).max(240.0)), "a tall page: 920 pt by 78 % of the display");
    assert_eq!(island_for(None, 300.0), Size::new(920.0, 364.0), "a short page: as tall as its body");
    let natural = Size::new(260.0, 120.0);
    assert_eq!(asked_island(natural, None, VMIN, ROOM), natural, "no request: no minimum or room applied, as before");
}

#[test]
fn a_page_gets_the_size_it_asks_for() {
    assert_eq!(island_for(ask(Some(1500.0), Some(1000.0)), 600.0), Size::new(1532.0, 1064.0), "1500 × 1000 of page, plus the chrome");
    assert_eq!(island_for(ask(Some(1300.0), None), 3000.0), Size::new(1332.0, (1117.0f64 * 0.78).round()), "width alone: today's height rule");
    assert_eq!(island_for(ask(None, Some(500.0)), 3000.0), Size::new(920.0, 564.0), "height alone: today's width");
}

#[test]
fn a_page_asking_more_than_the_screen_gets_the_room_under_the_menu_bar() {
    let s = island_for(ask(Some(4000.0), Some(3000.0)), 600.0);
    assert_eq!(s, ROOM, "the whole room, 8 pt in, under the menu bar");
    let sf = Rect::new(0.0, 0.0, SCREEN.w, SCREEN.h);
    let row_top = SCREEN.h - 38.0 - 6.0;
    let (x, y) = centred_origin(s, sf, row_top);
    assert_eq!((x, y), (8.0, 8.0), "placed 8 pt in from the side and the bottom");
    assert!(y + s.h <= row_top, "clear of the menu bar and the notch");
    let s = island_for(ask(Some(1500.0), Some(1000.0)), 600.0);
    let (x, y) = centred_origin(s, sf, row_top);
    assert!(x >= 8.0 && x + s.w <= SCREEN.w - 8.0 && y >= 8.0 && y + s.h <= row_top, "1532 × 1064 fits whole: {x},{y}");
    let small = Size::new(920.0, 871.0);
    assert_eq!(centred_origin(small, sf, row_top), ((SCREEN.w / 2.0 - 460.0).round(), (SCREEN.h / 2.0 - 871.0 / 2.0 + 20.0).round()), "today's sizes: centred 20 pt high, as before");
    let laptop = island_for(ask(Some(1500.0), Some(1000.0)), 600.0);
    let on_small = asked_island(laptop, ask(Some(1500.0), Some(1000.0)), VMIN, Size::new(1264.0, 740.0));
    assert_eq!(on_small, Size::new(1264.0, 740.0), "a smaller display: its room");
}

#[test]
fn a_page_asking_less_than_the_minimum_gets_the_minimum() {
    assert_eq!(island_for(ask(Some(40.0), Some(20.0)), 600.0), VMIN);
}

#[test]
fn owners_own_resize_wins_over_the_request_it_was_made_against() {
    let a = ask(Some(1500.0), Some(1000.0));
    let dragged = Kept { size: Size::new(1100.0, 800.0), asked: a };
    assert_eq!(hand_size_stands(Some(&dragged), a), Some(Size::new(1100.0, 800.0)), "same request: his size");
    assert_eq!(hand_size_stands(Some(&dragged), ask(Some(1600.0), Some(1000.0))), None, "the page asks something new: the page");
    assert_eq!(hand_size_stands(Some(&dragged), None), Some(Size::new(1100.0, 800.0)), "the page stopped asking: his size");
    let before = Kept { size: Size::new(924.0, 974.0), asked: None };
    assert_eq!(hand_size_stands(Some(&before), a), None, "a drag from before the page asked yields to the request");
    assert_eq!(hand_size_stands(Some(&before), None), Some(Size::new(924.0, 974.0)), "no request: his size, as before");
    assert_eq!(hand_size_stands(None, a), None);
    // Through the file: a drag is kept with what the page asked, and an old file without it still reads.
    let d = std::env::temp_dir().join(format!("viewer-size-ask-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    let store = d.join("viewer-size.json");
    let page = std::path::Path::new("/Users/t/Work/review.html");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(&store, br#"{"files":[{"path":"/Users/t/Work/review.html","width":924,"height":974}]}"#).unwrap();
    assert_eq!(remembered_kept(&store, page), Some(before), "an entry from before requests were kept");
    assert!(remember_asked(&store, page, Size::new(1400.0, 900.0), a).unwrap());
    assert!(!remember_asked(&store, page, Size::new(1400.0, 900.0), a).unwrap(), "unchanged: nothing written");
    let k = remembered_kept(&store, page).unwrap();
    assert_eq!(k, Kept { size: Size::new(1400.0, 900.0), asked: a });
    assert_eq!(hand_size_stands(Some(&k), a), Some(Size::new(1400.0, 900.0)), "dragged again under the request: his size wins");
    assert_eq!(remembered(&store, page), Some(Size::new(1400.0, 900.0)));
    let _ = std::fs::remove_dir_all(&d);
}
