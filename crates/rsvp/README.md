# rsvp

Fast-reading planner for Heimdall's island reader. Text goes in; a list of timed display units comes out. Pure functions, no I/O, no dependencies. The island reader (`crates/ui/src/island/object/reader.rs`, lead-in in `crates/ui/src/island/reader.rs`) plays these plans.

```rust
let mut plan = rsvp::plan(text, &rsvp::Config { wpm: 240, ..Default::default() });
for u in &plan.units { /* show u.text for u.duration_ms, focus letter u.focus_index, then blank u.pause_after_ms */ }
plan.retime(265); // dial moved: same units, new times
```

## What the reader gets

`Plan { units, total_ms, words, wpm }`, with each `Unit { text, focus_index, duration_ms, pause_after_ms, sentence_index, word_count }`. `focus_index` is a char index into `text.chars()`, not a byte index.

Seek helpers on `Plan`:
- `at(t_ms)` gives the unit at a time, whether it is showing or in its blank, and the offset into it.
- `start_ms(i)` and `remaining_ms(i)` give where a unit starts and how much time is left from there.
- `sentence_start(i)`, `back(i)` and `next_sentence(i)` move by sentence. `back` goes to the start of the current sentence, or to the previous sentence when `i` is already a sentence start.
- `sentence_range(s)` and `sentence_text(s)` serve the paused view, which shows the whole sentence.
- `words_before(i)` gives progress and feeds the settled-speed bookkeeping.

The crate also exports `tokenize`, `focus_index`, `dial()` (the 150–450 grid in 25-wpm steps through 240, the same as the reader's), `frequency_rank` and `warm()`. Calling `warm()` builds the word tables ahead of time; otherwise the first plan pays about 0.4 ms for them.

## Algorithm

Sources are cited by section of the research note `rsvp-fast-reading.md` (§1 is "Comprehension versus speed": its chunking, per-word timing and recognition-point parts). It is not in this repository; a copy sits untracked in the main checkout's `docs/research/`.

1. **Tokenise.** Split at whitespace only, so these stay whole: numbers (`1,000.50`, `10:30`, `2026-10-18`, `$5`, `40%`), hyphenated words, contractions, URLs, paths, file names and code names (`fast_read.rs`, `Plan::retime()`). An em dash or ellipsis glued between two words is split after the mark. A lone dash or ellipsis joins the previous word as a pause. Light markdown is dropped: backticks, bold, headings, bullets. Each token gets:
   - a **kind**: word, name, acronym, number, literal or symbol;
   - a **role**: function word, leader (numerals, "very", "more") or content;
   - its letters and syllables (vowel groups with English silent-e, which also holds for Polish), and a **frequency rank**;
   - the **pause after it**: clause for `, ; :`, a dash or a closing bracket; sentence for `. ! ? …` or a line break before a new item; paragraph for a blank line.

   Abbreviations (`Dr.`, `e.g.`, `np.`), initials and dotted acronyms do not end sentences. `etc.`/`itd.` ends one only when a listed capitalised word follows. A stop followed by a lowercase word is a clause pause.
2. **Chunk** into units of 1–3 words. These rules come from the product owner's direction, which overrides the research's single-word recommendation in §1; `max_chunk_words: 1` restores single words.
   - A function word leads the next word into its unit ("of the build").
   - A leader leads a content word ("three things", "40 percent").
   - Consecutive names stay together ("Ada Lovelace").
   - A short particle that closes a clause joins the word before it ("timed out.").
   - A unit that hits its word or width limit on a function word hands that word to the next unit: "it is" | "a test".
   - A leftover auxiliary or object pronoun joins the unit before it ("the limit is"). A leftover article or preposition stays alone rather than join the wrong phrase.
   - Units never cross a pause. They are never wider than `max_chunk_chars`.
   - Long words (`long_word_letters`+) and literals show alone, except that one function word of up to three letters may precede a long word ("the onboarding", "w ustawieniach,").
3. **Weigh each unit**, in multiples of the base time `60000 / wpm`:
   - Each word weighs `(0.55 + 0.05·letters + 0.08·syllables) × kind factor`. The length part is a continuous fit of the Spritz patent bands from §1: about 0.85 for 4 letters, 1.0 for 6, 1.3 for 10 and 1.6 for 14.
   - The kind factor for ordinary words is `0.92 + 0.06·log10(rank)`, capped at the value for unlisted words (about 1.19). This is the frequency effect: rarer words are read more slowly (§1, word length and frequency).
   - Fixed kind factors: names 1.2, acronyms 1.2, numbers 1.35, literals 1.3.
   - A function word inside a multi-word unit counts half (`light_word_weight`).
   - The first unit of the message, and the first after a sentence or paragraph, is ×1.2. After a clause it is ×1.1.
4. **Add pauses** after the unit. A clause adds 1.0. A sentence adds 2.0, or 1.6 for sentences of 7 words or fewer and 2.5 for more than 22, following the patent's length-dependent sentence blank. A paragraph adds 3.5. These are the research table's values (§1). The last unit gets no pause, because the reader owns the ending.
5. **Normalise.** Find the one scale at which the display times, each clamped to `[min_unit_ms, max_unit_ms]`, plus the scaled pauses add up to exactly `words × 60000 / wpm`. Bisection works because the sum only grows with the scale. Without this, the dial would be off by 20–30% (§1, recommended timing model). Rounding to whole milliseconds is cumulative, so `total_ms` never drifts by more than 1 ms.
6. **Recognition point.** For one word, the letter about a third in: index 0, 1, 2, 3 or 4 for 1, 2–5, 6–9, 10–13 or 14+ letters, skipping leading punctuation (§1, recognition point; the speedread/Spritz convention). In a multi-word unit it goes in the main word, which is the last non-function word, so the eye lands on the content and takes in the short words beside it.

## Parameters (`Config::default()`)

| Field | Default | Basis |
|---|---|---|
| `wpm` | 240 | research default (§1, default speed) |
| `max_chunk_words` | 3 | two words at a time; 1 = single words |
| `max_chunk_chars` | 16 | derived: wide enough for common phrases ("five pilot users", "w ustawieniach,"); the reader must fit this width |
| `long_word_letters` | 10 | long words get a fixation of their own |
| `min_unit_ms` / `max_unit_ms` | 100 / 2000 | 100 ms is above the roughly 83 ms per word at which short sentences are still understood (Potter, §1); 2 s caps URLs |
| `clause_pause` / `sentence_pause` / `paragraph_pause` | 1.0 / 2.0 / 3.5 | research table (§1) |
| `restart_factor` | 1.2 | derived: re-entry after a blank, like the lead-in in §4 |
| `light_word_weight` | 0.5 | derived: function words are mostly taken in with their neighbour |
| `frequency_effect` | 0.06 per tenfold rank | derived from the frequency effect (§1), kept small because length already carries most of it |
| `number_factor` / `name_factor` / `acronym_factor` / `literal_factor` | 1.35 / 1.2 / 1.2 / 1.3 | derived; numbers, names and codes cannot be predicted from context |

Everything marked derived is a starting point to tune on the user, not a measured result.

## Word data

- `data/en.txt` holds 10,000 English words, most frequent first. `data/gen_en.py` regenerates it from the Whisper tokenizer's merge order (MIT licence), keeping only lowercase whole-word tokens found in `/usr/share/dict/words` or as a regular inflection of one. Merge order approximates frequency in speech and web text, and the ranks are coarse, which suits a log-scale effect.
- `data/pl.txt` holds about 280 frequent Polish words, ordered by hand. Polish content words outside it count as unlisted, so Polish timing rests mostly on length and syllables.

## How the island reader consumes it

- **Show.** Plan once with the predicted starting speed. Keep the 600 ms lead-in in the UI; the plan starts at the first unit.
- **Play.** Run one timer per step: `duration_ms` with the unit drawn, then `pause_after_ms` blank (skip it when 0). Draw `text` so that `text.chars().nth(focus_index)` sits on the fixed red point. A unit can extend left of that point, because function words come before the main word.
- **Dial.** Call `retime(wpm)` and keep the current unit index. Resume the current unit with its remaining share, or simply restart it. Retiming 2,000 words takes about 0.05 ms.
- **Pause and back.** On pause, show `sentence_text(units[i].sentence_index)`. "Back" seeks to `back(i)`.
- **Progress and settled speed.** Take progress from `words_before(i) / words` and the time remaining from `remaining_ms(i)`. The speed memory in `crates/app/src/fast_read.rs` keeps working unchanged: the dial still means average wpm, pauses included.
- **End.** The last unit has no blank. Hold it, then show the done mark.

## Tests

Run `cargo test -p rsvp`. It covers:
- unit tests for the tokeniser, lexicon and planner;
- property tests over 400 random texts at all 13 dial speeds: the duration law, retime equal to a fresh plan, no empty or over-wide unit, no unit across a pause or sentence, punctuation adds a pause exactly when present, seek consistency, determinism, longer words never shorter, rare words slower than frequent ones, numbers, names and acronyms slower, pauses growing from clause to sentence to paragraph, and restart lengthening;
- golden plans for five realistic recaps (`tests/golden/*.txt`, including one in Polish). To accept intended changes, set `RSVP_BLESS=1` and review the `.plan` diff;
- a benchmark (`tests/bench.rs`) asserting that a 2,000-word text plans in under 2 ms.

`cargo run -p rsvp --example plan -- 240 < text.txt` prints a plan.
