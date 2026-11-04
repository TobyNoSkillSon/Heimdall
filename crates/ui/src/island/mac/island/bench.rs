//! The first ⌃⌘H pull-down of a long conversation, timed on the real offscreen host (layout with CoreText widths,
//! prewarm rasters, the layer tree and its commit): `heimdall-ui-demo --bench-chat N` (`BENCH_LIVE=1` for the live
//! clock's first frame, `BENCH_STALL_MS` to stall the first CoreText call, `BENCH_PARA=1` for paragraph replies) and
//! `--bench-layout N` (the layout alone). From the UI audit of 27 Oct (lab/ui-bench/README.md).

use std::fmt::Write as _;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Instant;

use objc2_foundation::MainThreadMarker;

use super::checks::{advance, spin};
use super::host::{with_host, B_AFTER_NS, B_APPLY_NS, B_COMMIT_NS, B_PREWARM_NS, B_TREE_NS};
use super::measure::{counters, reset_counters, AppKitMeasure};
use crate::island::chat::{ActionKind, ChatEntry, ChatKind, Heard, NoteShape};
use crate::island::object::{Event, FrameNeed, Measure, Target};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[(self.next() as usize) % xs.len()]
    }
    fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + (self.next() as usize) % (hi - lo + 1)
    }
}

const WORDS: &[&str] = &[
    "the", "build", "is", "green", "and", "I", "kept", "old", "one", "as", "rollback", "Dave", "pushed", "audio", "fix", "an", "hour", "ago", "memory",
    "free", "about", "sixty", "gigabytes", "no", "swap", "in", "use", "model", "cache", "cleared", "worker", "started", "on", "Verdict", "tests", "pass",
    "except", "two", "timeouts", "that", "fail", "identically", "base", "commit", "island", "notch", "conversation", "scroll", "reply", "spoken", "User",
    "asked", "for", "a", "summary", "of", "today's", "work", "plan", "tomorrow", "morning", "release", "Arsenal", "Vella", "Vireo", "download", "finished",
    "it", "took", "minutes", "GPU", "was", "busy", "with", "training", "run", "stopped", "at", "step", "loss", "went", "down", "to", "Heimdall", "said",
    "nothing", "new", "since", "last", "check", "email", "from", "recruiter", "Poznań", "job", "offer", "salary", "range", "looks", "fine", "should", "reply",
    "now", "or", "later", "calendar", "free", "after", "three", "meeting", "moved", "Thursday", "battery", "percent", "charging", "fans", "quiet", "temperature",
    "sixty-two", "degrees", "network", "slow", "Wi-Fi", "reconnected", "transcript", "saved", "archive", "clip", "ready", "screen", "recording", "paused",
    "because", "you", "were", "on", "call", "resumed", "when", "ended", "I'll", "stop", "there", "unless", "want", "more", "detail", "yes", "please", "go",
    "ahead", "do", "it", "carefully", "without", "breaking", "anything", "else", "keep", "settings", "as", "they", "are", "thanks", "great", "perfect",
];

fn sentence(r: &mut Rng, lo: usize, hi: usize) -> String {
    let n = r.range(lo, hi);
    let mut s: Vec<String> = (0..n).map(|_| r.pick(WORDS).to_string()).collect();
    if let Some(f) = s.first_mut() {
        let mut c = f.chars();
        *f = c.next().map(|x| x.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default();
    }
    s.join(" ") + "."
}

/// A reply whose length follows the session's measured distribution (median 264 chars, p90 641, max 1208).
fn reply(r: &mut Rng) -> String {
    let target = match r.range(0, 9) {
        0..=4 => r.range(80, 264),
        5..=8 => r.range(264, 641),
        _ => r.range(641, 1208),
    };
    let mut out = String::new();
    let list = r.range(0, 3) == 0;
    if std::env::var("BENCH_PARA").is_ok() {
        // Short paragraphs (2-3 sentences, 15-45 words), as replies are written: each within balance()'s 48 words.
        while out.len() < target {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            let mut p = String::new();
            for _ in 0..r.range(2, 3) {
                if !p.is_empty() { p.push(' '); }
                p.push_str(&sentence(r, 6, 14));
            }
            out.push_str(&p);
        }
        return out;
    }
    while out.len() < target {
        if list && !out.is_empty() {
            out.push_str("\n- ");
            out.push_str(&sentence(r, 4, 12));
        } else {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(&sentence(r, 5, 18));
        }
    }
    out
}

/// A day of conversation: `n` rows, the reader's cap being 400.
pub(crate) fn day(n: usize, seed: u64) -> Vec<ChatEntry> {
    let mut r = Rng(seed);
    let mut out = vec![];
    let mut at = 1_761_500_000_000u64;
    while out.len() < n {
        at += 30_000;
        let kind = match r.range(0, 19) {
            0..=6 => ChatKind::Owner(sentence(&mut r, 3, 25)),
            7..=14 => ChatKind::Heimdall { text: reply(&mut r), heard: if r.range(0, 9) == 0 { Heard::Cut } else { Heard::Whole } },
            15..=18 => ChatKind::Action { kind: if r.range(0, 1) == 0 { ActionKind::Started } else { ActionKind::Finished }, text: sentence(&mut r, 3, 9), answer: None },
            _ => ChatKind::Note { id: format!("n{}", out.len()), title: sentence(&mut r, 2, 5), shape: NoteShape::Table },
        };
        out.push(ChatEntry { at_ms: at, kind });
    }
    out
}

fn ms(ns: u64) -> f64 {
    ns as f64 / 1e6
}

fn breakdown() -> String {
    format!(
        "island.apply {:.1} ms, prewarm {:.1} ms, after(render) {:.1} ms [tree.apply {:.1}, commit {:.1}]",
        ms(B_APPLY_NS.load(Relaxed)),
        ms(B_PREWARM_NS.load(Relaxed)),
        ms(B_AFTER_NS.load(Relaxed)),
        ms(B_TREE_NS.load(Relaxed)),
        ms(B_COMMIT_NS.load(Relaxed))
    )
}

fn rss_mb() -> f64 {
    // Current resident size (task_info MACH_TASK_BASIC_INFO).
    #[repr(C)]
    #[derive(Default)]
    struct Basic {
        virtual_size: u64,
        resident_size: u64,
        resident_size_max: u64,
        user_time: [i32; 2],
        system_time: [i32; 2],
        policy: i32,
        suspend_count: i32,
    }
    extern "C" {
        fn mach_task_self() -> u32;
        fn task_info(task: u32, flavor: i32, info: *mut i32, count: *mut u32) -> i32;
    }
    let mut b = Basic::default();
    let mut count = (std::mem::size_of::<Basic>() / 4) as u32;
    unsafe { task_info(mach_task_self(), 20, &mut b as *mut Basic as *mut i32, &mut count) };
    b.resident_size as f64 / 1e6
}

fn stats(xs: &mut [f64]) -> String {
    if xs.is_empty() {
        return "n=0".into();
    }
    xs.sort_by(f64::total_cmp);
    let p = |q: f64| xs[((xs.len() - 1) as f64 * q).round() as usize];
    format!("n={} p50 {:.2} ms, p95 {:.2} ms, max {:.2} ms", xs.len(), p(0.5), p(0.95), xs[xs.len() - 1])
}

/// Ticks at 120 Hz on the virtual clock for `secs`; each tick's main-thread cost.
fn frames(secs: f64) -> (Vec<f64>, usize) {
    let mut v = vec![];
    let mut moving = 0;
    let n = (secs * 120.0) as usize;
    for _ in 0..n {
        let t0 = Instant::now();
        with_host(|h| {
            let t = h.clock.unwrap_or(0.0) + 1.0 / 120.0;
            h.clock = Some(t);
            h.apply(Event::Tick);
            if matches!(h.island.needs_frames(t), FrameNeed::Moving | FrameNeed::Loop { .. }) {
                moving += 1;
            }
        });
        let e = t0.elapsed().as_secs_f64() * 1000.0;
        if e > 8.0 {
            eprintln!("SLOW TICK {e:.1} ms at tick {}: {} | measure {}", v.len(), breakdown(), counters());
        }
        v.push(e);
    }
    spin(0.0);
    (v, moving)
}

fn open(label: &str, out: &mut String) -> f64 {
    reset_counters();
    let t0 = Instant::now();
    with_host(|h| h.open_chat());
    let total = t0.elapsed().as_secs_f64() * 1000.0;
    let _ = writeln!(out, "{label}: open_chat {total:.1} ms ({}) | measure {}", breakdown(), counters());
    let (mut f, moving) = frames(1.0);
    let _ = writeln!(out, "    frames after it (120 Hz, 1 s): {} | moving/loop ticks {moving}", stats(&mut f));
    total
}

fn close(out: &mut String) {
    let t0 = Instant::now();
    with_host(|h| h.apply(Event::Click(Target::ReplyBand)));
    let _ = writeln!(out, "    close: {:.1} ms ({})", t0.elapsed().as_secs_f64() * 1000.0, breakdown());
    advance(1.5);
}

pub(crate) fn run(n: usize) -> String {
    let mtm = MainThreadMarker::new().expect("main thread");
    objc2_app_kit::NSApplication::sharedApplication(mtm).setActivationPolicy(objc2_app_kit::NSApplicationActivationPolicy::Prohibited);
    super::set_sink(Box::new(|_| {}));
    super::install_offscreen(mtm);
    let mut out = String::new();
    let _ = writeln!(out, "rows {n}; rss start {:.0} MB", rss_mb());
    // Heimdall spoke before (the fonts are loaded, as in the running app).
    super::apply(Event::Speaking(true));
    advance(0.1);
    super::apply(Event::SpokenText(Some("Dave pushed the audio fix an hour ago, and I kept the old build as the rollback.".into())));
    advance(1.0);
    super::apply(Event::Speaking(false));
    advance(3.0);
    if let Some(ms) = std::env::var("BENCH_STALL_MS").ok().and_then(|s| s.parse::<u64>().ok()) {
        super::measure::STALL_NEXT_MS.store(ms, Relaxed);
    }
    let mut rows = day(n, 7);
    let chars: usize = rows.iter().map(|e| match &e.kind { ChatKind::Owner(t) => t.len(), ChatKind::Heimdall { text, .. } => text.len(), ChatKind::Action { text, .. } => text.len(), ChatKind::Note { title, .. } => title.len() }).sum();
    let _ = writeln!(out, "conversation: {} rows, {chars} chars", rows.len());
    let t0 = Instant::now();
    super::apply(Event::Chat(rows.clone()));
    let _ = writeln!(out, "Chat update while closed: {:.2} ms", t0.elapsed().as_secs_f64() * 1000.0);
    advance(1.0);
    if std::env::var("BENCH_LIVE").is_ok() {
        // The live clock (as the app runs): where the first painted frame falls in the opening motion.
        let virt = with_host(|h| h.clock).flatten().unwrap_or(0.0);
        with_host(|h| { h.epoch = Instant::now() - std::time::Duration::from_secs_f64(virt); h.clock = None; });
        let before = with_host(|h| h.now()).unwrap();
        with_host(|h| h.open_chat());
        let painted = with_host(|h| h.now()).unwrap();
        let (_, start) = with_host(|h| h.island.motion()).unwrap();
        let first = with_host(|h| h.island.frame(painted).shape).unwrap();
        let end = with_host(|h| h.island.frame(painted + 3.0).shape).unwrap();
        let from = with_host(|h| h.island.frame(start).shape).unwrap();
        let _ = writeln!(out, "LIVE CLOCK first ⌃⌘H: event at {before:.3}, motion start {start:.3}, first paint {painted:.3} ({:.0} ms into the motion); shape at start {:.0}x{:.0}, at first paint {:.0}x{:.0}, settled {:.0}x{:.0} => first frame shows {:.0}% of the height change", (painted - start) * 1000.0, from.w, from.h, first.w, first.h, end.w, end.h, 100.0 * (first.h - from.h) / (end.h - from.h).max(1e-6));
        let t = with_host(|h| h.now()).unwrap();
        with_host(|h| h.clock = Some(t));
        close(&mut out);
    }
    open("FIRST ⌃⌘H (cold)", &mut out);
    let _ = writeln!(out, "    rss after first open {:.0} MB", rss_mb());

    // While open: scroll, speech level, a new row arriving.
    let mut sc = vec![];
    for i in 0..40 {
        let t0 = Instant::now();
        with_host(|h| h.wheel(if i < 20 { -24.0 } else { 24.0 }, 0.0));
        sc.push(t0.elapsed().as_secs_f64() * 1000.0);
        advance(1.0 / 120.0);
    }
    let _ = writeln!(out, "    scroll events while open: {}", stats(&mut sc));
    let mut lv = vec![];
    for i in 0..60 {
        let t0 = Instant::now();
        super::apply(Event::SpeechLevel((i % 10) as f32 / 10.0));
        lv.push(t0.elapsed().as_secs_f64() * 1000.0);
    }
    let _ = writeln!(out, "    speech-level events while open (not speaking): {}", stats(&mut lv));
    // Speaking while the conversation is open: the level is part of the scene (33 updates a second).
    {
        let long = (0..6).map(|i| format!("Paragraph {i}: the build is green and I kept the old one as the rollback, Dave pushed the audio fix an hour ago and the tests pass except two timeouts.")).collect::<Vec<_>>().join("\n\n");
        super::apply(Event::LastReply(long));
        super::apply(Event::Speaking(true));
        super::apply(Event::SpokenText(Some("Paragraph 0: the build is green.".into())));
        advance(0.5);
        let mut sp = vec![];
        let mut ticks = vec![];
        for i in 0..90 {
            let t0 = Instant::now();
            super::apply(Event::SpeechLevel(((i * 7) % 10) as f32 / 10.0));
            sp.push(t0.elapsed().as_secs_f64() * 1000.0);
            // the frames between two levels (120 Hz: about 3.6 per level)
            for _ in 0..3 {
                let t1 = Instant::now();
                with_host(|h| { let t = h.clock.unwrap_or(0.0) + 1.0 / 120.0; h.clock = Some(t); h.apply(Event::Tick); });
                ticks.push(t1.elapsed().as_secs_f64() * 1000.0);
            }
        }
        let clock = with_host(|h| h.clock_report()).unwrap();
        let _ = writeln!(out, "    speaking while open: level events {} | ticks {} | clock link {} loop {}", stats(&mut sp), stats(&mut ticks), clock.0, clock.1);
        super::apply(Event::Speaking(false));
        super::apply(Event::SpokenText(None));
        advance(2.0);
    }
    // A reply streamed while the conversation is open (bug note 2026-10-26T20-37-07: the main thread stalled in
    // CoreText on every new string): each sentence as it is spoken, the reply as written growing with it.
    {
        let mut st = vec![];
        let mut written = String::new();
        super::apply(Event::Speaking(true));
        for k in 0..24 {
            let s = format!("Sentence {k} of the streamed reply says the build {k} is green and the rollback kept, {} more words.", k * 3 + 7);
            written.push_str(&s);
            written.push(' ');
            let t0 = Instant::now();
            super::apply(Event::SpokenText(Some(s)));
            super::apply(Event::LastReply(written.clone()));
            st.push(t0.elapsed().as_secs_f64() * 1000.0);
            advance(0.05);
        }
        super::apply(Event::Speaking(false));
        super::apply(Event::SpokenText(None));
        let _ = writeln!(out, "    streamed sentences while open (spoken + written): {} | measure {}", stats(&mut st), counters());
        advance(2.0);
    }
    // Scrolling up through the day, past the first screens (the veil under the bar is drawn on the way).
    {
        let mut up = vec![];
        for _ in 0..60 {
            let t0 = Instant::now();
            with_host(|h| h.wheel(-30.0, 0.0));
            up.push(t0.elapsed().as_secs_f64() * 1000.0);
            advance(1.0 / 120.0);
        }
        let _ = writeln!(out, "    scrolling up 60 x 30 pt while open: {}", stats(&mut up));
    }
    let (mut idle, moving) = frames(1.0);
    let _ = writeln!(out, "    ticks while open and still: {} | moving/loop ticks {moving}", stats(&mut idle));
    let clock = with_host(|h| h.clock_report()).unwrap();
    let _ = writeln!(out, "    clock while open and still: link {} loop {} deadline {:?}", clock.0, clock.1, clock.2);
    rows.push(ChatEntry { at_ms: rows.last().unwrap().at_ms + 1000, kind: ChatKind::Heimdall { text: "One more reply arrived while it was open.".into(), heard: Heard::Whole } });
    reset_counters();
    let t0 = Instant::now();
    super::apply(Event::Chat(rows.clone()));
    let _ = writeln!(out, "    a new row while open: {:.1} ms ({}) | measure {}", t0.elapsed().as_secs_f64() * 1000.0, breakdown(), counters());
    advance(1.0);
    close(&mut out);
    let clock = with_host(|h| h.clock_report()).unwrap();
    let _ = writeln!(out, "    clock at rest after close: link {} loop {} deadline {:?}", clock.0, clock.1, clock.2);

    open("SECOND ⌃⌘H (same rows)", &mut out);
    close(&mut out);

    rows.push(ChatEntry { at_ms: rows.last().unwrap().at_ms + 1000, kind: ChatKind::Owner("And what about tomorrow?".into()) });
    super::apply(Event::Chat(rows.clone()));
    advance(1.0);
    open("THIRD ⌃⌘H (one new row since)", &mut out);
    close(&mut out);
    let _ = writeln!(out, "rss end {:.0} MB", rss_mb());
    let cycles: usize = std::env::var("BENCH_LOOP").ok().and_then(|s| s.parse().ok()).unwrap_or(0);
    for c in 0..cycles { objc2::rc::autoreleasepool(|_| {
        rows.push(ChatEntry { at_ms: rows.last().unwrap().at_ms + 1000, kind: ChatKind::Heimdall { text: format!("Cycle {c}: another reply with a few words in it."), heard: Heard::Whole } });
        if rows.len() > 400 { rows.remove(0); }
        super::apply(Event::Chat(rows.clone()));
        advance(0.5);
        let t0 = Instant::now();
        with_host(|h| h.open_chat());
        let _o = t0.elapsed().as_secs_f64() * 1000.0;
        advance(1.0);
        for _ in 0..30 { with_host(|h| h.wheel(-60.0, 0.0)); advance(1.0 / 60.0); }
        with_host(|h| h.apply(Event::Click(Target::ReplyBand)));
        advance(1.5);
        });
        let _ = writeln!(out, "cycle {c}: rss at rest after pool drain {:.0} MB", rss_mb());
    }
    out
}

/// The layout alone, cold: as shipped (per-word widths, per-row cache) against shaping every line whole (the layout
/// before 27 Oct), the marks compared one by one.
pub(crate) fn layout_variants(n: usize) -> String {
    use crate::island::object::chat_layout::{clear_cache, lay};
    let mut out = String::new();
    let rows: Vec<ChatKind> = day(n, 7).into_iter().map(|e| e.kind).collect();
    let w = 420.0;
    let warm = AppKitMeasure::default();
    for f in [crate::island::object::Font::Spoken, crate::island::object::Font::Strong, crate::island::object::Font::Caption, crate::island::object::Font::Label] {
        warm.width("Heimdall warm", f);
    }
    struct Whole(AppKitMeasure);
    impl Measure for Whole {
        fn width(&self, text: &str, font: crate::island::object::Font) -> f64 {
            self.0.width(text, font)
        }
    }
    let time = |label: &str, out: &mut String, m: &dyn Measure, rows: &[ChatKind]| -> crate::island::object::chat_layout::Block {
        clear_cache();
        reset_counters();
        let t0 = Instant::now();
        let b = lay(m, rows, w);
        let _ = writeln!(out, "{label}: {:.1} ms | {}", t0.elapsed().as_secs_f64() * 1000.0, counters());
        b
    };
    let old = time("lay, every line shaped whole, cold (before 27 Oct)", &mut out, &Whole(AppKitMeasure::default()), &rows);
    let new = time("lay, per-word widths, cold (as shipped)", &mut out, &AppKitMeasure::default(), &rows);
    let lines = new.marks.iter().filter(|m| matches!(m, crate::island::object::Mark::Text { .. })).count();
    let differ = old.marks.iter().zip(new.marks.iter()).filter(|(a, b)| a != b).count() + old.marks.len().abs_diff(new.marks.len());
    let _ = writeln!(out, "marks differing {differ} of {} ({lines} text lines; height {:.0} vs {:.0} pt)", new.marks.len(), old.h, new.h);
    // A new row: the rows laid out, then one more (the per-row cache keeps the rest).
    let m = AppKitMeasure::default();
    clear_cache();
    let _ = lay(&m, &rows, w);
    let mut more = rows.clone();
    more.push(ChatKind::Heimdall { text: "One more reply arrived while it was open, with a second sentence to wrap.".into(), heard: Heard::Whole });
    reset_counters();
    let t0 = Instant::now();
    let _ = lay(&m, &more, w);
    let _ = writeln!(out, "lay after one new row (row cache warm): {:.2} ms | {}", t0.elapsed().as_secs_f64() * 1000.0, counters());
    let mut worst = 0.0f64;
    for r in rows.iter().filter(|r| matches!(r, ChatKind::Heimdall { .. })) {
        let m = AppKitMeasure::default();
        clear_cache();
        let t0 = Instant::now();
        let _ = lay(&m, std::slice::from_ref(r), w);
        worst = worst.max(t0.elapsed().as_secs_f64() * 1000.0);
    }
    let _ = writeln!(out, "worst single reply, cold: {worst:.1} ms");
    let t0 = Instant::now();
    let mut k = 0u64;
    for c in &new.chunks {
        let content = crate::island::object::Content::Body { lines: vec![], font: crate::island::object::Font::Spoken, color: crate::island::object::Rgba::WHITE, line_h: 20.0, h: 300.0, scroll: 0.0, notes: vec![], marks: c.marks.clone(), veil: 0.0 };
        k ^= content.key();
    }
    let _ = writeln!(out, "Content::key() over all {} chunks: {:.2} ms ({k:x})", new.chunks.len(), t0.elapsed().as_secs_f64() * 1000.0);
    out
}
