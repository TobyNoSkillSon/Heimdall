//! Lab only: Heimdall's Vella observer and link state (vella.rs) alone, on a main run loop, printing every
//! `dev.vella.dictation.state` it receives and the link's state after it. No capture, UI, archive or agent; it
//! touches nothing of the running Heimdall. For checking a real Vella's notifications end to end.
//!
//! cargo run -p app --bin heimdall-vella-link-lab -- [SECONDS]   (default 60, at most 600)
#[allow(dead_code)]
#[path = "../vella.rs"]
mod vella;
use objc2_foundation::{NSDate, NSRunLoop};
use std::time::{Duration, Instant};

fn main() {
    let seconds: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(60).min(600);
    let (tx, rx) = crossbeam_channel::bounded::<vella::Heard>(64);
    // Installs the observer and posts dev.heimdall.vella.query, as the app does at start.
    vella::observe(tx);
    println!("{}", serde_json::json!({"event":"observing","seconds":seconds,"query_posted":vella::QUERY}));
    let mut link = vella::Link::default();
    let end = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < end {
        NSRunLoop::currentRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(0.1));
        while let Ok(h) = rx.try_recv() {
            let notice = vella::Notice::parse(&h.info);
            let change = notice.as_ref().map(|n| format!("{:?}", link.apply(n, h.at_ns, h.at_unix)));
            println!("{}", serde_json::json!({"event":"heard","info":h.info,"parsed":notice,"change":change,
                "on":link.on(),"keys":link.keys(),"link":link.summary()}));
        }
        let now = capture::host_now_ns();
        if link.expire(now) {
            println!("{}", serde_json::json!({"event":"expired","on":link.on(),"link":link.summary()}));
        }
    }
    println!("{}", serde_json::json!({"event":"done","on":link.on(),"link":link.summary()}));
}
