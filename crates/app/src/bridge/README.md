# bridge — the app's agent glue

The app side of the agent harness (`crates/agent` owns the Pi process, delivery ledger and MCP host).

| File | Owns |
|---|---|
| `mod.rs` | `Command` (everything sent to the agent thread), transport-image referencing, dry-run settling, test hooks |
| `supervisor.rs` | `AgentRuntime`: the agent thread, its command channel, restarts and shutdown. Agent off lets a running compaction or a state-writing tool call finish first (`Stopping`, `crucial_work`; "Stopping after compaction…" in the menu's Agent caption); a model switch parked behind that compaction is kept for the next Agent on, which also restarts the MCP server the wait shut down (docs/resilience.md). Emergency mode: `agent::emergency::Detector` spots the failure here and turns are held; only Agent off (`StopReason::Agent`) ends it, Quit keeps it for the next launch. The controller is `crates/app/src/emergency.rs` (docs/emergency.md). A real assistant reply marks its provider's quota back for background jobs (`background_gate::quota_back`; docs/models.md) |
| `compaction_hold.rs` | when Heimdall may start a compaction of its own: held after a failed one until a reply succeeds, and backed off after one that died with Pi (docs/resilience.md) |
| `startup.rs` | `open`: active session, pending reloads, the start context of every start (catch-up of up to 30 min, reload reason, restart message), the compaction policy and the idle-maintenance step (`agent::maintenance`), process ownership, provider check |
| `tools.rs` | `Tools` and `agent::mcp::ToolHost`: every MCP tool the agent calls (screen, archive, island, speech, drawing, T3, settings); the agent's quota frames feed `background_gate::quota_warned` / `quota_spent` |
| `delivery.rs` | typed sends, outbox notices (kinds and pacing in `agent::notice`), agent state published to the window and island |
| `adapters.rs` | harness adapters: dry-run logger, unavailable stand-in, the recording adapter that archives each payload before transport |
| `speech.rs` | lazy synthesizer, voice-speed sample, delivery-gated speech output, level tap |
| `voice_preview.rs` | Speaking ▸ Voice: a clicked voice's sample, cached per voice in `<data>/voice-samples/` (local voices rendered ahead at idle, ElevenLabs on first click), never over the user or a reply, a newer click stops the last |
| `speech_policy.rs` | when Heimdall makes a sound and when it keeps quiet: the wake sound on a spoken wake; nothing played while the user is in a conversation (a call line in the last 2 min) except in a run their address by name woke: the text goes to the island, `speech_held` is journalled (`mcp_actions.rs`) and the `speak` result says so; `ask` cards silent |
| `echo_cancel.rs` | when Apple's echo cancellation runs: output on speakers only, while Heimdall's speech or an ack is queued, playing or within `ECHO_CANCEL_TAIL` of its end |
| `hold.rs` | the user's turn on the microphone: a line that would start while they talk is skipped, and the skip latches for the rest of that reply (`speech_skipped` journalled); one due within `QUIET_AFTER` of their stop waits out the pause; once the reply is over its lines show as one island line (the reply stays in the pull-down) and the agent is told once that they were not said; lines their own wake made stale are dropped unshown |
| `unread.rs` | final replies shown as text (Speaking off, held or failed) that the user has probably not read yet (200 wpm + 2 s, an estimate): a reply arriving inside that time keeps them in the pull-down above it (at most three, newest last) and tells the agent once, as context (`unheard-speech:unread`), which reply they probably missed |
| `health.rs` | agent health copy (de-duplicated, never raw transport errors) |
| `intent.rs` | the durable menu/file stop intent |
| `window.rs` | Agent window feed: session history on open, then each streamed entry |
| `questions.rs` | voice answers to `ask` cards: d1 judges the user's talk while a card is up at a lower send score; speech d1, their name or "send it" sent to Heimdall closes the card at its wake and overrides a click made while saying it; fillers, asides and other speakers never do; guarded T3 confirmations stay click-only |
| `ask_voice.rs` | a spoken choice read against the card's options: a number, a list ("1 and 3", "both"), an ordinal or the exact label |

**Interface:** `bridge::{AgentRuntime, Tools, Command}` and the helpers `pipeline/` and `mcp_*` call
(`archive_screenshot_action`, `notice_outcome(s)`, `window_problem`, `agent_thinking`, `LazySynth`, `SampleVoice`).

**Testing:** `cargo nextest run -p app -E 'test(/^bridge::/)'`, with a fake Pi (`agent/tests/fixtures/
mock-adapter.py`); the test modules sit in `tests/`, declared at the foot of `mod.rs`.
`pi_supervisor_tests::joined_loop_guard_aborts_turn_journals_once_and_keeps_pi_session` is timing-sensitive: it can
miss its evidence deadline when the Mac is loaded. Ignored lab tests run the real pinned Pi (`live_pi_smoke`,
`HEIMDALL_PI_SMOKE_DATA`) or OpenCode (`opencode_start_lab`, `HEIMDALL_OPENCODE_LAB`); see their module docs.
