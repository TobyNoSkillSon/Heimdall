# voicebank: pre-generated voice lines and the clip store

Short lines generated ahead of time so a reply can play a stored clip instead of synthesizing, plus the durable store
of every voice clip Heimdall generates (bank takes and spoken replies).

Read this when: you change the reply lines, the acknowledgement set, the background fill or the clip store.
Code: `crates/voicebank/src/`, `crates/app/src/voice_bank.rs`.

## Files

- `lib.rs`: the `Generator` trait (`id`, `settings`, `synthesize`, `set_language`, `set_direction`). Clips are looked
  up by the generator's `id`, so a new speech model means a new id and a refill. Default language: English only.
- `store.rs`: `Store`, `ClipMeta`, `Origin`, `Stored`; clips live in the archive store (`heimdall-archive`), `copy_clip`.
- `lines.json`, `lines.rs` (`LineSet`, `Line`): the 52 reply lines.
- `acks.json`, `acks.rs`, `ack_gate.rs`: the acknowledgement set and its gate. The public edition plays no
  acknowledgements ([editions.md](../../docs/editions.md)).
- `fill.rs`: `tick`, `tick_languages`, `next_missing`, `Conditions` (the in-app fill gate), `TAKES` = 3.
- `drama3.rs`: the personal edition's offline fill backend; it never stores Supertonic audio under its own id.
- `import.rs`: validated import of a lab bank into a data folder; `ensure_lab_path` refuses a lab bank inside the live
  profile or with `..` components.
- `src/bin/heimdall-voicebank.rs`: `fill`, `stats`, `curate`, `import`. `fill --backend cpu|metal|auto` uses Supertonic;
  `--backend drama3` uses `drama3.rs`.

Depends on `speak`, `heimdall-archive`.

## Tests

- `cargo test -p voicebank` (about 25 s; the import rejection cases in `tests/import_cli.rs` dominate).
- `scripts/ack_clips.sh --lab DIR --into DATA [--apply]` fills the acknowledgement set offline and imports it; its
  checks are `scripts/test_ack_clips.py`.

## See also

- [voices.md](../../docs/voices.md): voice choice
- [archive.md](../../docs/archive.md): the store the clips live in
- [crates/speak/README.md](../speak/README.md): the synthesizers behind `Generator`
