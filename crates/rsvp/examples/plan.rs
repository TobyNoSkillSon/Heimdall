//! Prints the plan for text on stdin: `cargo run -p rsvp --example plan -- [wpm] < text.txt`.

use std::io::Read;

fn main() {
    let wpm = std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(rsvp::DEFAULT_WPM);
    let mut text = String::new();
    std::io::stdin().read_to_string(&mut text).expect("stdin");
    let p = rsvp::plan(&text, &rsvp::Config { wpm, ..Default::default() });
    println!("{} words, {} units, {} ms at {} wpm", p.words, p.units.len(), p.total_ms, p.wpm);
    for u in &p.units {
        let marked: String = u.text.chars().enumerate().map(|(i, c)| if i == u.focus_index { format!("[{c}]") } else { c.to_string() }).collect();
        println!("{:>5} ms  +{:>4} ms  s{:<3} {}", u.duration_ms, u.pause_after_ms, u.sentence_index, marked);
    }
}
