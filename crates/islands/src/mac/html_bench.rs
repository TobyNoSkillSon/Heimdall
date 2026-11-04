//! Lab: draw model HTML fragments through the shared renderer and time it (`lab/islands-html/bench.sh` runs it with
//! the process and memory measurements around it).
//!   FLOATING_ISLANDS_DIR=<lab dir> FloatingIslands --html-bench <out dir> <fragments dir> [rounds]
//! Per fragment: sanitize, the first render (web view made, if none), the same fragment again (a cache hit), then a
//! changed fragment (a re-render on a warm web view). PNGs of every bitmap on black (the island's fill) at 2×, then the
//! web view's release after `RELEASE_AFTER`. Lines go to stdout and `<out>/bench.txt`; the process exits after.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use objc2_core_foundation::{CGPoint, CGRect, CGSize};

use super::draw::{self, Bitmap};
use super::html;
use super::sys;
use crate::model::html as gate;

struct Bench {
    out: PathBuf,
    frags: Vec<(String, String)>,
    lines: RefCell<Vec<String>>,
    rounds: usize,
}

fn say(b: &Bench, s: String) {
    println!("{s}");
    b.lines.borrow_mut().push(s);
}

pub fn run(out: &str, dir: &str, rounds: usize) {
    let out = PathBuf::from(out);
    let _ = std::fs::create_dir_all(&out);
    let mut frags: Vec<(String, String)> = std::fs::read_dir(dir)
        .map(|d| d.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "html")).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|p| Some((p.file_stem()?.to_string_lossy().into_owned(), std::fs::read_to_string(&p).ok()?)))
        .collect();
    frags.sort();
    let b = Rc::new(Bench { out, frags, lines: RefCell::new(vec![]), rounds: rounds.max(1) });
    sys::after(1.0, move || step(b, 0, 0));
}

fn step(b: Rc<Bench>, round: usize, i: usize) {
    if i >= b.frags.len() {
        if round + 1 < b.rounds {
            // Next round: every fragment changed by one character (a fresh render each, warm web view).
            return sys::soon(move || step(b, round + 1, 0));
        }
        return finish(b);
    }
    let (name, src) = b.frags[i].clone();
    let t0 = sys::uptime();
    let clean = gate::sanitize(&if round == 0 { src } else { format!("{src}<!-- {round} --><i></i>") }, &|p| p.exists());
    let san_ms = (sys::uptime() - t0) * 1000.0;
    let clean = match clean {
        Ok(c) => c,
        Err(e) => {
            say(&b, format!("{name}: refused: {e}"));
            return sys::soon(move || step(b, round, i + 1));
        }
    };
    let (_, made_before, alive, _) = html::stats();
    let t1 = sys::uptime();
    let frag = clean.html.clone();
    let b2 = b.clone();
    let asked = frag.clone();
    html::request(&asked, 328.0, 2.0, move |d| {
        let ms = (sys::uptime() - t1) * 1000.0;
        let (_, made, _, _) = html::stats();
        let Some(d) = d else {
            say(&b2, format!("{name} r{round}: not drawn ({:?})", html::failed(&frag, 328.0, 2.0)));
            return sys::soon(move || step(b2, round, i + 1));
        };
        let t2 = sys::uptime();
        let hit = html::cached(&frag, 328.0, 2.0).is_some();
        let hit_ms = (sys::uptime() - t2) * 1000.0;
        say(&b2, format!(
            "{name} r{round}: sanitize {san_ms:.2} ms ({} elements, {} B in, {} B out) | render {ms:.1} ms ({}) | {}x{} pt, bitmap {}x{} px | cache hit {hit} {hit_ms:.3} ms",
            clean.elements, b2.frags[i].1.len(), frag.len(),
            if made > made_before { "new web view" } else if alive { "warm web view" } else { "web view made" },
            d.width, d.height, d.image.width(), d.image.height()
        ));
        if round == 0 {
            write_on_black(&d, &b2.out.join(format!("{name}.png")));
        }
        sys::soon(move || step(b2, round, i + 1));
    });
}

fn write_on_black(d: &html::Drawn, path: &std::path::Path) {
    let pad = 16.0;
    let (w, h) = (d.width + 2.0 * pad, d.height + 2.0 * pad);
    let Some(bm) = Bitmap::new((w * 2.0) as usize, (h * 2.0) as usize) else { return };
    bm.fill(CGRect::new(CGPoint::ZERO, CGSize::new(w * 2.0, h * 2.0)), &super::look::cg(&super::look::black()));
    bm.draw(&d.image, CGRect::new(CGPoint::new(pad * 2.0, pad * 2.0), CGSize::new(d.width * 2.0, d.height * 2.0)));
    if let Some(img) = bm.image() {
        draw::write_png(&img, path);
    }
}

fn finish(b: Rc<Bench>) {
    let (renders, made, alive, _) = html::stats();
    say(&b, format!("done: {renders} renders, {made} web views made, alive now {alive}; waiting {} s for the release", html::release_after() + 3.0));
    println!("BENCH-IDLE-START");
    sys::after(html::release_after() + 3.0, move || {
        let (_, _, alive, _) = html::stats();
        say(&b, format!("after release: web view alive {alive}"));
        println!("BENCH-RELEASED");
        let _ = std::fs::write(b.out.join("bench.txt"), b.lines.borrow().join("\n") + "\n");
        // Hold a while so the outside script can see the web process leave.
        sys::after(12.0, || super::terminate());
    });
}
