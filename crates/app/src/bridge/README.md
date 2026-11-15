# bridge: the app's agent glue

The app side of the agent harness: the agent thread and its commands, every MCP tool the agent calls, typed sends and
notices, the Agent window feed, and when Heimdall speaks or keeps quiet. `crates/agent` owns the Pi process, the
delivery ledger and the MCP host.

Read this when: you want to find the file that owns an agent-side behaviour in the app (a tool, speech timing, typed
messages, the Agent window feed, startup context, compaction fallback), or run the bridge's tests.
Code: `crates/app/src/bridge/`; tests in `crates/app/src/bridge/tests/`, declared at the foot of `mod.rs`.

## Files

| File | Owns |
|---|---|
| `mod.rs` | `Command` (everything sent to the agent thread), transport-image referencing, dry-run settling, test hooks |
| `supervisor.rs` | `AgentRuntime`: the agent thread, its command channel, restarts and shutdown; Agent off lets a running compaction or state-writing tool call finish first (`Stopping`); emergency mode's detection and held turns; island card actions (`AgentNoticeAction`) |
| `startup.rs` | `open`: the active session, pending reloads, the start context (catch-up of up to 30 min, reload reason, restart message), the compaction policy and idle maintenance, process ownership (`record_process_ownership`), the provider check, `background_calls_allowed` |
| `tools.rs` | `Tools` and `agent::mcp::ToolHost`: every MCP tool the agent calls (screen, archive, island, speech, drawing, T3, settings); `window_action`, `notice_action`, `notice`, `reload`; the agent's quota frames feed `background_gate`; `stop_pressed` (the island's speech Stop) |
| `delivery.rs` | typed sends (`validate_message`: empty or over 1 MiB is refused), outbox notices, `notice_outcome(s)`, agent state published to the window and island |
| `window.rs` | the Agent window feed: session history on open, then each streamed entry |
| `adapters.rs` | harness adapters: the dry-run logger, the unavailable stand-in, the recording adapter that archives each payload before transport |
| `health.rs` | agent health copy, de-duplicated, never raw transport errors |
| `intent.rs` | the durable menu and file stop intent |
| `compaction_fallback.rs` | when Pi's compaction fails (spent quota or credit, refused sign-in, history too long), Heimdall writes the summary itself on the next model whose window holds it, never the provider that just failed; a last-resort cut keeps the newest turns |
| `compaction_hold.rs` | when Heimdall may start a compaction of its own: held after a failed one until a reply succeeds, backed off after one that died with Pi |
| `typed_arrival.rs` | a typed message arriving (Agent window, island composer, a drop on the notch): speech stops, the wake sound plays, then an acknowledgement says it arrived |
| `speech.rs` | the lazy synthesizer (`LazySynth`), voice-speed sample, delivery-gated speech output, level tap; the call-voice hook: a guest's voice holds a waiting reply and, still open after `GUEST_CUT_AFTER` (0.4 s) while Heimdall is audible, stops it |
| `speech_policy.rs` | when Heimdall makes a sound: the wake sound on a spoken wake; nothing played during a conversation (a call line within `CONVERSATION_NS`, 45 s, or a guest's voice open) unless the user's address by name woke the run; a held line is said only within `HELD_FOR` (60 s); call voice at `CALL_VOICE_GAIN` (half); `ask` cards are silent |
| `hold.rs` | the user's turn on the microphone: in a call a line waits for a gap up to `PATIENCE` (10 s), then is skipped; an answer the user asked for waits for the user's own pause only; otherwise a line that would start while the user talks is skipped for the rest of that reply, shown as one growing island line, and the agent is told once |
| `call_interrupt.rs` | listening mode's cut-off: who may interrupt Heimdall on a call (the listened guest or an unlabelled voice; not another named voice, the user, Heimdall's echo or noise); a verdict still pending after `PENDING_MAX` (2.5 s) cuts |
| `echo_cancel.rs` | when Apple's echo cancellation runs: speaker output only, while speech or an acknowledgement is queued, playing or within `ECHO_CANCEL_TAIL` (3 s) of its end |
| `unread.rs` | replies shown as text that the user has probably not read yet (`WPM` 200 plus 2 s, an estimate): a newer reply keeps them in the pull-down (at most three) and the agent is told once (`unheard-speech:unread`) |
| `voice_preview.rs` | Speaking ▸ Voice: a clicked voice's sample, cached per voice in `<data>/voice-samples/`; never over the user or a reply; a newer click stops the last |
| `questions.rs` | voice answers to `ask` cards: the user's speech while a card is up, judged by the decider; fillers, asides and other speakers never answer; guarded T3 confirmations stay click-only |
| `ask_voice.rs` | a spoken choice read against the card's options: a number, a list ("1 and 3", "both"), an ordinal or the exact label |

Interface: `bridge::{AgentRuntime, Tools, Command}` and the helpers `pipeline/` and `mcp_*` call
(`archive_screenshot_action`, `notice_outcome(s)`, `window_problem`, `agent_thinking`, `LazySynth`, `SampleVoice`).

## Tests

- `cargo nextest run -p app -E 'test(/^bridge::/)'` runs the bridge against a fake Pi
  (`crates/agent/tests/fixtures/mock-adapter.py`).
- `pi_supervisor_tests::joined_loop_guard_aborts_turn_journals_once_and_keeps_pi_session` is timing-sensitive and can
  miss its deadline on a loaded Mac.
- Ignored lab tests run the real pinned Pi (`live_pi_smoke`, `HEIMDALL_PI_SMOKE_DATA`) or OpenCode
  (`opencode_start_lab`, `HEIMDALL_OPENCODE_LAB`); see their module docs. Run with `-- --ignored`.

## Behaviour lives in

- [crates/agent/README.md](../../../agent/README.md): delivery, starts, Stop, compaction, recovery, process ownership
- [docs/agent-window.md](../../../../docs/agent-window.md): the Agent window and island card contract
- [docs/resilience.md](../../../../docs/resilience.md): Agent off, failures and recovery
- [docs/emergency.md](../../../../docs/emergency.md): emergency mode
- [docs/models.md](../../../../docs/models.md): providers, budgets and quotas
- [docs/decision-chain.md](../../../../docs/decision-chain.md): the decider, barge-in and voice answers

## See also

- [crates/app/README.md](../../README.md): the app crate's map
- [docs/mcp.md](../../../../docs/mcp.md): the agent's tools
