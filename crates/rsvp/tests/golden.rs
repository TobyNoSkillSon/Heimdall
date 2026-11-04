//! Golden plans for five realistic Heimdall recaps at the default speed. RSVP_BLESS=1 rewrites the .plan files;
//! review the diff before committing.

use rsvp::{plan, Config, Plan};
use std::path::Path;

const RECAPS: &[&str] = &["build-failure", "meeting-recap", "research-digest", "day-summary-pl", "inbox-triage"];

fn render(p: &Plan) -> String {
    let mut s = format!("# {} words, {} units, {} ms at {} wpm\n# show_ms\tpause_ms\tsentence\tunit (focus letter in [ ])\n", p.words, p.units.len(), p.total_ms, p.wpm);
    for u in &p.units {
        let marked: String = u.text.chars().enumerate().map(|(i, c)| if i == u.focus_index { format!("[{c}]") } else { c.to_string() }).collect();
        s.push_str(&format!("{}\t{}\t{}\t{}\n", u.duration_ms, u.pause_after_ms, u.sentence_index, marked));
    }
    s
}

#[test]
fn recaps_match_their_golden_plans() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    let bless = std::env::var_os("RSVP_BLESS").is_some();
    let mut failures = Vec::new();
    for name in RECAPS {
        let text = std::fs::read_to_string(dir.join(format!("{name}.txt"))).unwrap();
        let p = plan(&text, &Config::default());
        // Sanity a golden file cannot hide: chunking happened, the dial holds, every sentence is reachable.
        assert!(p.units.len() < p.words, "{name}: no chunking");
        assert!(p.units.iter().filter(|u| u.word_count > 1).count() * 5 >= p.units.len(), "{name}: under a fifth of units are chunks");
        assert!((p.total_ms as f64 - p.words as f64 * 250.0).abs() <= 1.0, "{name}: {} ms for {} words", p.total_ms, p.words);
        assert_eq!(p.sentences(), p.units.last().unwrap().sentence_index + 1);
        // Long single words get more time per word than short single words.
        let per = |f: &dyn Fn(usize) -> bool| {
            let v: Vec<f64> = p.units.iter().filter(|u| u.word_count == 1 && f(u.text.chars().filter(|c| c.is_alphabetic()).count())).map(|u| u.duration_ms as f64).collect();
            v.iter().sum::<f64>() / v.len().max(1) as f64
        };
        assert!(per(&|n| n >= 7) > per(&|n| (1..=4).contains(&n)), "{name}: long words not slower");
        let rendered = render(&p);
        let path = dir.join(format!("{name}.plan"));
        if bless {
            std::fs::write(&path, &rendered).unwrap();
        } else if std::fs::read_to_string(&path).ok().as_deref() != Some(rendered.as_str()) {
            failures.push(format!("{name}: plan changed (RSVP_BLESS=1 to accept)\n{rendered}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
