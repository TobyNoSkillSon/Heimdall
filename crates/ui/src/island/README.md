# Notch island

Everything Heimdall shows at the notch is one object on one clock: one black shape grows out of the notch and returns
into it, and every element (lines, the agent's turn, the voice and its words, questions, notes, the reader, drawing
mode, pointing, drops, health) is a function of its spring. This README owns the island's behaviour rules.

Read this when: you want to change what the island shows or when, add an island event or card, change its timing,
debug a click or drop at the notch, or run the island's tests and visual judge.
Code: `crates/ui/src/island/object/` (platform-neutral model), `crates/ui/src/island/mac/` (AppKit), `crates/ui/src/island/drop.rs`, `crates/ui/src/island/chat.rs`.

## Interface to the app

- Only `ui::Ui::send(UiUpdate::…)` in and `UiAction` callbacks out (`crates/ui/src/model.rs`).
- `HeimdallSpeaking(false)` is the audio-ended input; `SpokenText` and `SpeechLevel` feed the words and the bars;
  `LastReply` is the reply as written, for the pull-down; `Chat` is today's conversation, built by
  `crates/app/src/chat_history.rs`.
- Out: `StopSpeaking`, `PullOut`, `TypedWithDrop`, `Secret` (a `SecretText`, its `Debug` prints `<secret>`), question
  answers (with `by_hand`), drops.

## The object (`object/`)

- `engine.rs`, `Island`: `apply(event, t)` is the only way state changes; `frame(t)` is pure (shape, every slot, the
  drawing light, pointing marks); `needs_frames(t)` says Moving / Loop / None; `needs_ticks(t)` is the live clock (a
  loop of thinking dots or the spinner alone needs none: those layers loop in Core Animation); `deadline()` is the next
  one-shot change; `hit(t, p)` returns only stable targets during a motion, so Send keeps taking taps; `track(t)` is
  never empty; `delay_motion` pushes back a motion whose first frame could not paint in time.
- Transition rule: the shape springs (`spring::GROW`, `spring::CALM`), shared elements glide, incoming content fades
  in scaled to fit, outgoing content leaves with the shape; one content layer per place (what leaves clears before what
  arrives; ear pairs and button rows swap together); at most three motion starts a second (`MOTION_GAP`), but a change
  while the shape still grows redirects that growth; never shorter than the notch while as wide; live content (reader
  units, numerals, the waveform, pointer feedback) cuts.
- Endings retract: no check after a spoken turn, no "back" after a restart, no chosen-answer stage, no "Attached" after
  drawing mode's Send, no dimmed words when the voice stops. A command's own result still shows.
- Pace budgets (`every_motion_keeps_apples_pace` in `tests.rs`): `GROW` and `CALM` reach 90 % within 0.255 s; an
  arrival effect starts within 0.30 s of its motion; a retraction stops asking for frames by 0.6 s, a growth by 0.7 s.
- `scene.rs`, the one state machine (`scene(t)` picks the one thing to say). Its constants:
  - Voice: its look waits up to `WORDS_WAIT` (0.25 s) for words; ends at once on `SpeechCut` or `Stop`, else at the
    audio's end + `GRACE` (0.45 s), or `VOICE_STALL` (3 s) after its last sign of life. Each `SpokenText` sentence is
    added below the last, none replaced; a voice starting again within `REPLY_JOIN` (3 s) continues the same reply;
    the row shows the newest sentences within `SPEAK_LINES` (6).
  - Lines: `read_seconds` (`object/mod.rs`): 1.8–3.4 s by kind, +50 ms a character past 12, at most 8 s. A different
    line replaces one after `MIN_LINE` (1 s), but a line to read keeps its whole read time and the next waits. Only
    health problems rest as a token.
  - Notes and turns: each note belongs to the turn that showed it; a turn ends with its `LastReply` or when a new run
    starts (a `NO_REPLY` run sends no reply); the next reply or note starts the next turn and retires the last turn's
    notes (at most `MAX_TURN_NOTES`, 6). A note opens the pull-down, which pulls itself up after `NOTE_UNATTENDED`
    (12 s) on screen with the pointer away (`Event::Pointer`); its X (`Target::NoteClose`) removes it for good.
  - Switches (`Event::Switch`, from ⌃⌘. ⌃⌘/ ⌃⌘, or a tile): the glyph turns slashed or whole at `SWITCH_TURN`
    (0.45 s) of its `SWITCH_SECONDS` (1.5 s) moment. While Listening or Seeing is off, both ear glyphs rest at
    `SWITCH_TOKEN_ALPHA` (0.5), the off one slashed; a click opens the last turn. Reduce Motion: no transitions, no bounces.
  - Vella (`Event::Vella`): Vella's mark over "Dictating to Vella" for `VELLA_SECONDS` (1.6 s), then the mark rests in
    the left ear beside Vella's shortcut (`Ear::Num`; the keyboard symbol past `VELLA_KEYS_MAX`, 4 characters). With
    Listening off the Listening token shows instead. The mark is `Content::Vella` from `crates/ui/assets/island/vella.png`,
    never redrawn.
  - Catching up (`HealthCue::Busy { key, part, label }`): the stack symbol over its words, layers lighting in turn
    (`Fx::Variable` for `VARIABLE_SYMBOLS`, else breathing; still under Reduce Motion) for `BUSY_READ` (4 s), then the
    stack and a dim hourglass rest in the ears until `Recovered` or `Clear` for its key. The same key with new words
    changes only the words in place. It ranks below a restart and an unacknowledged problem, above the pending command
    glyph and tokens; it never makes the helmet red.
  - Emergency (`Event::Emergency`, from `UiUpdate::AgentRelay`): the agent's glyph and an orange triangle rest in the
    ears; the emergency notice alone leaves nothing; chosen relay mode draws nothing at rest.
  - Nudge (`Event::Nudge`, from `crates/app/src/islands/nudge.rs`): one small pill (bell or question mark), bursts
    coalesce (`NUDGE_COALESCE`), it waits while the island shows anything else. Its landing jiggle is intended:
    `FrameModel::jiggle` marks those frames and the motion judge leaves them out of its width-reversal count.
- `layout.rs`: the template, size classes and constants (`STOP`, `WIDE_W` 700, `SCREEN_MARGIN` 160, `ATTACH` 52,
  `ASK_RING_D` 20), `reply_max_w`, `composer_placeholder` (the composer's only placeholder words).
- `measure.rs`: the `Measure` trait (`Approx` for the simulator, `Fuzz` adversarial widths); lines break on per-word
  widths, and a sum within `EDGE` (0.5 pt) of the fit is measured whole; `wrap_path` wraps a path whole.
- `chat_layout.rs` (conversation rows laid out once, cached two generations), `prose.rs` (paragraph gaps, hanging list
  items, first sentence bold), `reader.rs` (plays `crates/rsvp` plans), `gallery.rs` (one named scenario per state),
  `spring.rs`, `geom.rs`.
- `sim.rs`: drives the real `Island` on virtual time and checks invariants I1–I22 every frame; the table at its top
  defines each (e.g. I6 nothing draws at rest, I13 three motions a second, I19 no success mark after speech, I20 a
  reply's words only grow while spoken, I22 an agent's note never stays open unattended).
- Older models in `island/`: `agent_notice.rs` (questions, answers, grants), `drawing/` (strokes, pointing specs),
  `reader.rs` (`ui::reader` types), `click_source.rs` (hardware click check).

## Questions (`agent_notice.rs`, `object/layout.rs`)

- Several questions: Send is an arrow disc after the step chips, lit once anything is answered; it sends the answers
  and every other question empty (the agent hears it as unanswered; T3 gets "(unanswered)", and Send stays dim on a
  T3 card while an options-only question is open). Chips show a tick when answered, an accent dot while open. Return in
  the Other row brings up the next open question and sends once none is left (`Event::AskReturn`). One question: Send
  sits in its Other row.
- Countdown (`NoticeAction::Ask { countdown: true, .. }`): m:ss rounded up beside a ring of `ASK_RING_D` that empties
  clockwise; each second is a cut. A deadline per second only while such a card is on screen (`countdown_until`). The
  UI never expires it: at `0:00` it stays open for the controller to close or replace.
- Grants (`GrantItem::Delete`, `LoginItem`, `Keychain`: trash, power, key glyphs): a recessed well in `Font::Path`
  wrapped by `measure::wrap_path`. A click grants only when press and release came from the HID system
  (`click_source`, `Event::HardwareClick`); those picks go out as `by_hand`.
- An open card has no close X: its lip (`Target::AskTuck`) tucks it away unanswered (picks kept, digit keys gone); the
  notch brings it back ahead of the last turn. Esc in its field dismisses it. A new question shows at once while another
  is tucked. Without a notch the card has no lip and never tucks.
- While Heimdall speaks the card docks under his words; with the conversation pulled down, at its bottom outside the scroll.

## Pull-down, conversation and composer

- At rest, while the last turn has anything, the notch alone is a transparent click target (only under a real notch;
  nothing drawn, no timer). The pointer at the notch shows a peek lip (`Lip::Peek`) that goes when it leaves. While
  Heimdall sends, thinks, compacts, works or speaks, the whole shape is the target.
- A click, drag down or two-finger swipe down shows the last turn: reply, then that turn's notes, scrolling inside half
  the screen; a wide table widens it up to `reply_max_w`. A click or drag up on the lip band pulls it up.
- Stop: while Heimdall speaks the left ear is `layout::STOP`; `Target::StopSpeech` ends the look and sends
  `UiAction::StopSpeaking` (the app cuts the voice and labels the waking line; see decision-chain.md).
- ⌃⌘H (`Event::OpenChat`) pulls down the user's conversation with the keyboard in the composer (`Fields::focus_composer`;
  the click target is a non-activating panel). Pressed again while it shows (`owners_reply_open`) it pulls it up; one
  hidden under the reader or drawing mode stays. A pull-down a note opened stays that note's alone.
- The conversation (`chat.rs`) opens on the latest exchange; `chat::split` places the live turn by its replies and note
  ids so nothing shows twice; it follows the end unless the user scrolled up; what scrolls up goes under the top bar as
  a veil (`Content::Body` `veil`). Pull out: the band's pin or a drag sends `UiAction::PullOut`.
- Native fields (composer and Other) have no placeholder and sit `field::TEXT_SHIFT` off their slots so typed text lands
  where the drawn placeholder was. The composer scrolls on one line; it never wraps. Text editing is readied at install
  (`Fields::prepare_composer`).
- Copy (`mac/island/copy.rs`): words are layers, so a right-click on a message or the reply offers Copy of the whole
  message (`Layout` `copies`, `Island::copy_at`); ⌘C copies the field's selection, else the reply (`Island::copy_shown`).
- Edit keys: Heimdall has no main menu, so the key panel performs ⌘V ⌘X ⌘A ⌘Z ⇧⌘Z itself (`copy::edit_command`; ⌘C is
  `copy::command_c`).
- Secret field: the lock (`Target::ComposerLock`) swaps in an `NSSecureTextField` (`Target::ComposerSecret`); Return
  sends `Event::SecretSent` → `UiAction::Secret`, never a typed message, and unlocks; closing the pull-down unlocks and
  wipes it. The app stores it (`crates/app/src/secrets.rs`).

## Drops (`drop.rs`, `mac/drop/`)

- `drop.rs` is platform-neutral: `classify` (files, links, image data, text; long text kept as a file; temporary
  screenshots and image data copied to `drops_dir()`, `drops/` in the data folder), `summary`, `route`, and the pure
  motion (`SWELL` 26 × 10 pt, glow, the thumbnail's flight). While the island shows more than the notch the swell is
  the island's own shape (`Tree::drop`, `Host::set_drop`).
- `mac/drop/` is two windows titled `Heimdall Drop Target`: the drawn one joins the island's overlay space and never
  takes the mouse; an empty catcher outside that space takes the drag. Why: macOS offers no drag to a window in a
  process's own SkyLight space.
- Both are ordered in only while a drag is under way (pasteboard count change; a late-writing source is polled every
  `POLL_S`, 40 ms, while the button is down, `DragWatch`; the button checked every 0.2 s while armed), and out once the
  button is up and the motion ended. Catch area: `ZONE_W` × `ZONE_H` (340 × 100 pt) at the notch's top centre, or the
  island's frame plus `ISLAND_MARGIN` (18 pt).
- While a drag is armed the click target ignores the mouse (`Host::pass_drags`). `drop::route`: with the pull-down
  showing and text or focus in the composer, the drop attaches (`Event::ComposerAttach`, `ATTACH` thumbnails,
  `Target::ComposerDetach`) and Return sends `UiAction::TypedWithDrop`; otherwise it is a notch drop.
- Late takes (`End::TakeLate`, `late_take`): a drag let go over the notch that macOS never dropped is taken from the
  pasteboard, unless it started in Heimdall, a key went down, another app's window covers the notch, or the pasteboard
  changed. The app decides when a drop reaches the agent (`crates/app/src/pipeline/drops.rs`).

## AppKit (`mac/`, declared from `crates/ui/src/mac/mod.rs` with `#[path]`)

- `mac/island/paint.rs`: `Tree::apply(&FrameModel)`, the one draw call, live and offscreen; SF Symbols are
  `NSImageView`s playing `NSSymbolEffect`s; `Tree::wake_renderer` wakes the symbol renderer before an arrival's clock.
- `mac/island/raster.rs` (each distinct content drawn once, with a blurred twin), `measure.rs` (CoreText widths, cache
  rotating generations at `CACHE_MAX` 8192), `field.rs`, `copy.rs`, `bench.rs`, `checks.rs`, `mod.rs` (`apply`,
  `notice`, `pin`).
- `mac/island/host.rs`: window, click target, clock (display link while moving, a timer for loops, one one-shot timer
  per deadline, nothing at rest). Display: the island opens on the pointer's display, or on the one settings
  `islands_display` locks it to while connected (drawing mode's display wins while it is up); a display change lays it
  out again at once, and each arrival reads its display afresh (`display_tests`).
- `mac/draw/`: drawing overlay at `NSPopUpMenuWindowLevel + 1`, with the island raised above it (`island::pin`); only
  drawing mode's own controls take a press (`host::draw_control`), any other press is ink. Over an open menu it holds a
  still (`still.rs`) placed under the ink before the overlay takes the keyboard.
- `mac/space.rs` (`crate::mac::overlay_space`): every island window joins a SkyLight space of Heimdall's own at
  absolute level 100 when ordered in, again after a display change, and leaves at quit. A missing symbol or failed step
  falls back to `overlay_behavior()` and logs one `island: overlay space …` line.
- `mac/streams.rs`: Show Islands on Streams, off by default: island windows are `NSWindowSharingType::None`; on, shared
  read-only. Heimdall's own captures exclude them by title either way (`cargo run -p screen --example island_exclusion`).
- `mac/gallery.rs`: the animation gallery and the motion judge.

## Test

1. `cargo test -p ui --lib island::`: the object and the simulator (grid and seeded random sequences, adversarial
   widths, determinism, pace) under every invariant. Modules: `object/tests.rs`, `ask_tests.rs`, `busy_tests.rs`,
   `composer_drop_tests.rs`, `countdown_tests.rs`, `emergency_tests.rs`, `heard_exit_tests.rs`, `notch_tests.rs`,
   `open_chat_tests.rs`, `secret_field_tests.rs`, `stuck_reply_tests.rs`, `switch_tests.rs`, `vella_tests.rs`,
   `island/tests.rs`, and the tests in `drop.rs`, `chat.rs`, `agent_notice.rs`, `click_source.rs`, `host.rs`.
2. `cargo test -p ui --test island_composer --test island_pulldown_drop --test island_chat_bench --test island_motion_nudge`.

Demo flags (`heimdall-ui-demo`, build in [crates/ui/README.md](../../README.md); offscreen, exit 1 on a failure):

- `--island <dir>`: stills of every card, window properties, clicks through the real event path, cost at rest.
- `--motion <dir>`, `--gallery <dir> [--only a,b] [--fps n]`: every card at 120 Hz through the same draw call, judged
  (symmetry, clipping, smoothness, rest) with main-thread cost per frame (`<dir>/anims/judge.json`).
- `--sheet <dir> (--seed N | --grid a,b,phase | --card name)`: key frames of a simulated sequence.
- `--island-loops`, `--composer`, `--pulldown-drop`, `--drop`, `--drawing`, `--recovery-countdown`, `--catching-up`,
  `--relay-activity`, `--streams`, `--islands-display`, `--agent` (question cards).
- `--bench-chat N`, `--bench-layout N`: the first ⌃⌘H of an N-row conversation (default 400); knobs in `mac/island/bench.rs`.
- On screen, only when the user is away: `--drop-live [secs]` (default 60), `--drawing-over-menu <other|own> [secs]`
  (default 3), `--live`.

## See also

- [crates/ui/README.md](../../README.md): the crate's code map, menu and demo build
- [islands-capabilities.md](../../../../docs/islands-capabilities.md): what islands show
- [agent-window.md](../../../../docs/agent-window.md): the question card from the agent's side
- [harness-autonomy.md](../../../../docs/harness-autonomy.md): approval grants
- [claude-relay.md](../../../../docs/claude-relay.md), [emergency.md](../../../../docs/emergency.md): relay and emergency mode
- [vella.md](../../../../docs/vella.md): dictation to Vella
- [decision-chain.md](../../../../docs/decision-chain.md): what Stop labels
- [viewer.md](../../../../docs/viewer.md): pulled-out notes as viewer islands
- [settings.md](../../../../docs/settings.md): `islands_display`, Show Islands on Streams
