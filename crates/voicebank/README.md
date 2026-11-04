# voicebank — pre-generated voice lines and the clip store

Code map for the voice bank. The voice bank is personal-edition only.

- `store.rs`: `Store`, `ClipMeta`, `Origin`, `Stored`; clips in the archive store (`heimdall-archive`), `copy_clip`.
- `lines.json`, `lines.rs` (`LineSet`/`Line`): the 52 reply lines. `acks.json`, `acks.rs`, `ack_gate.rs`: the acknowledgement set and its gate.
- `fill.rs`: `tick`, `next_missing`, `Conditions` (the in-app fill gate), `TAKES` = 3.
- `Generator`: Supertonic, or a second sidecar voice in `drama3.rs`, which checks each take with `scripts/drama3_asr.py`.
- `import.rs`: validated lab-bank import. `src/bin/heimdall-voicebank.rs`: `fill`, `stats`, `curate`, `import`.

Depends on `speak`, `heimdall-archive`.

**Testing:** `cargo nextest run -p voicebank`, about 25 s in all (import rejection cases dominate). The ack set fills
and imports offline with `scripts/ack_clips.sh`.
