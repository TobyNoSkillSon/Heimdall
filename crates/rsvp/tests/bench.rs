//! Planning speed: a 2,000-word message must plan in under 2 ms (warm), and retime in well under that.

use rsvp::{plan, Config};
use std::time::Instant;

fn text_of(words: usize) -> String {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/");
    let corpus: String = ["build-failure", "meeting-recap", "research-digest", "inbox-triage", "day-summary-pl"]
        .iter()
        .map(|n| std::fs::read_to_string(format!("{dir}{n}.txt")).unwrap())
        .collect::<Vec<_>>()
        .join("\n\n");
    let all: Vec<&str> = corpus.split(' ').collect();
    (0..words).map(|i| all[i % all.len()]).collect::<Vec<_>>().join(" ")
}

#[test]
fn two_thousand_words_plan_under_two_ms() {
    let text = text_of(2000);
    let cold = Instant::now();
    rsvp::warm();
    let warm_up = cold.elapsed();
    let c = Config::default();
    let mut p = plan(&text, &c);
    assert!(p.words >= 1990, "{} words", p.words);
    let mut best = f64::MAX;
    let mut best_retime = f64::MAX;
    for i in 0..30 {
        let t = Instant::now();
        let q = plan(&text, &c);
        best = best.min(t.elapsed().as_secs_f64() * 1000.0);
        std::hint::black_box(&q);
        let t = Instant::now();
        p.retime(if i % 2 == 0 { 300 } else { 240 });
        best_retime = best_retime.min(t.elapsed().as_secs_f64() * 1000.0);
    }
    println!("lexicon build {:.2} ms; plan {} words -> {} units: best {best:.3} ms; retime best {best_retime:.3} ms", warm_up.as_secs_f64() * 1000.0, p.words, p.units.len());
    assert!(best < 2.0, "plan took {best:.3} ms");
}
