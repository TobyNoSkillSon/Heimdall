# rsvp: the fast-reading planner

Plans the island reader's fast reading: text goes in, a list of timed display units comes out. Pure functions, no I/O,
no dependencies.

Read this when: you change how fast reading chunks or times words, the reader's dial, or the word tables.
Code: `crates/rsvp/src/lib.rs` (planner, `Config`, `Plan`), `src/token.rs` (tokeniser), `src/lexicon.rs` (word
ranks and roles); the reader that plays plans is `crates/ui/src/island/object/reader.rs` (lead-in in
`crates/ui/src/island/reader.rs`), its speed memory `crates/app/src/fast_read.rs`.

```rust
let mut plan = rsvp::plan(text, &rsvp::Config { wpm: 240, ..Default::default() });
for u in &plan.units { /* show u.text for u.duration_ms, focus letter u.focus_index, then blank u.pause_after_ms */ }
plan.retime(265); // dial moved: same units, new times
```

## What the reader gets

- `Plan { units, total_ms, words, wpm }`; each `Unit { text, focus_index, duration_ms, pause_after_ms,
  sentence_index, word_count }`. `focus_index` is a char index into `text.chars()`, not a byte index.
- Seek helpers on `Plan`: `at(t_ms)` (the unit at a time, showing or in its blank, and the offset), `start_ms(i)`,
  `remaining_ms(i)`, `sentence_start(i)`, `back(i)` (start of the current sentence, or the previous one when `i` is
  already a start), `next_sentence(i)`, `sentence_range(s)` and `sentence_text(s)` (the paused view's whole sentence),
  `words_before(i)`, `sentences()`.
- Also exported: `tokenize`, `focus_index`, `frequency_rank`, `warm()`, and `dial()`: `MIN_WPM` 150, `MAX_WPM` 450,
  25-wpm steps (`STEP_WPM`) through `DEFAULT_WPM` 240, the same grid as the reader. `warm()` builds the word tables
  ahead of time; otherwise the first plan pays for them (about 0.4 ms, measured).

## Planning steps

1. **Tokenise.** Split at whitespace only, so numbers (`1,000.50`, `10:30`, `$5`, `40%`), hyphenated words,
   contractions, URLs, paths and code names (`fast_read.rs`, `Plan::retime()`) stay whole. An em dash or ellipsis
   glued between two words splits after the mark; a lone one joins the previous word as a pause. Light markdown
   (backticks, bold, headings, bullets) is dropped. Each token gets a kind (word, name, acronym, number, literal,
   symbol), a role (function word, leader such as numerals or "very", content), letters, syllables, a frequency rank,
   and the pause after it: clause for `, ; :`, a dash or a closing bracket; sentence for `. ! ? …` or a line break
   before a new item; paragraph for a blank line. Abbreviations (`Dr.`, `e.g.`, `np.`), initials and dotted acronyms
   do not end sentences; `etc.`/`itd.` ends one only before a listed capitalised word; a stop before a lowercase word
   is a clause pause.
2. **Chunk** into units of 1 to `max_chunk_words` words. A function word leads the next word into its unit ("of the
   build"); a leader leads a content word ("40 percent"); consecutive names stay together; a short clause-closing
   particle joins the word before it ("timed out."). A unit that hits its word or width limit on a function word hands
   it to the next unit. A leftover auxiliary or object pronoun joins the unit before; a leftover article or preposition
   stays alone. Units never cross a pause and are never wider than `max_chunk_chars`. Words of `long_word_letters` or
   more and literals show alone, except that one function word of up to three letters may precede a long word.
3. **Weigh each unit** in multiples of `60000 / wpm` ms (`word_weight`): `(0.55 + 0.05·letters + 0.08·syllables) ×
   kind factor`, letters capped at 20 (40 for literals), syllables at 8. Ordinary words: `0.92 + frequency_effect ·
   log10(rank)`, capped at the unlisted-word value. Names, acronyms, numbers and literals use their `Config` factors.
   A function word inside a multi-word unit counts `light_word_weight`. The first unit of the message and the first
   after a sentence or paragraph are ×`restart_factor`; after a clause, half the extra.
4. **Add pauses** after the unit: `clause_pause`, `paragraph_pause`, or `sentence_pause` scaled ×0.8 for sentences of
   7 words or fewer and ×1.25 for more than 22. The last unit gets no pause: the reader owns the ending.
5. **Normalise.** Bisect for the one scale at which the display times, each clamped to `[min_unit_ms, max_unit_ms]`,
   plus the pauses add up to exactly `words × 60000 / wpm`. Rounding is cumulative, so `total_ms` drifts at most 1 ms.
   Why: without it the dial would be off by 20 to 30%.
6. **Recognition point.** One word: letter index 0, 1, 2, 3 or 4 for 1, 2–5, 6–9, 10–13 or 14+ letters, after
   leading punctuation. A multi-word unit: in the main word, the last word that is not a function word.

## Parameters (`Config::default()`)

| Field | Default | Basis |
|---|---|---|
| `wpm` | 240 | reading research default |
| `max_chunk_words` | 3 | design choice: short phrases read in one glance; 1 restores single words |
| `max_chunk_chars` | 16 | derived; the reader must fit this width |
| `long_word_letters` | 10 | long words get a fixation of their own |
| `min_unit_ms` / `max_unit_ms` | 100 / 2000 | 100 ms is above the roughly 83 ms per word at which short sentences are still understood; 2 s caps URLs |
| `clause_pause` / `sentence_pause` / `paragraph_pause` | 1.0 / 2.0 / 3.5 | reading research table |
| `restart_factor` | 1.2 | derived |
| `light_word_weight` | 0.5 | derived |
| `frequency_effect` | 0.06 per tenfold rank | derived, kept small because length carries most of it |
| `number_factor` / `name_factor` / `acronym_factor` / `literal_factor` | 1.35 / 1.2 / 1.2 / 1.3 | derived |

"Derived" values are starting points, not measured results.

## Word data

- `data/en.txt`: 10,000 English words, most frequent first. `data/gen_en.py` regenerates it from the Whisper
  tokenizer's merge order, keeping lowercase whole words found in `/usr/share/dict/words` or as a regular inflection.
- `data/pl.txt`: about 280 frequent Polish words, ordered by hand. Other Polish words count as unlisted.

## How the reader plays a plan

- Plan once at the predicted starting speed; the 600 ms lead-in stays in the UI.
- One timer per step: `duration_ms` with the unit drawn, then `pause_after_ms` blank (skip when 0). Draw `text` so
  `text.chars().nth(focus_index)` sits on the fixed focus point.
- Dial: `retime(wpm)` and keep the unit index (retiming 2,000 words takes about 0.05 ms, measured).
- Pause shows `sentence_range(units[i].sentence_index)`; Back seeks to `back(i)`, or `sentence_start(i)` once ended.
- The dial means average wpm, pauses included, so the speed memory in `crates/app/src/fast_read.rs` holds unchanged.
- The last unit has no blank: hold it, then show the done mark.

## Tests

- `cargo test -p rsvp`: tokeniser, lexicon and planner unit tests; property tests over 400 random texts at all 13
  dial speeds (`tests/properties.rs`: duration law, retime equals a fresh plan, unit width and pause rules, seek
  consistency, determinism, slower rare words, numbers, names and acronyms); golden plans for five recaps
  (`tests/golden/*.txt`, one Polish).
- Accept an intended golden change: `RSVP_BLESS=1 cargo test -p rsvp --test golden`, then review the `.plan` diff.
- Speed check, ignored by default, run on an idle Mac: `cargo test -p rsvp --test bench -- --ignored` (a 2,000-word
  text must plan in under 2 ms).
- Print a plan: `cargo run -p rsvp --example plan -- 240 < text.txt`.

## See also

- [islands-capabilities.md](../../docs/islands-capabilities.md): what the island shows, including the reader
- [mcp.md](../../docs/mcp.md): the `fast_read` tool
