# Island — the notch object

Everything the user sees at the notch is one object on one clock (DESIGN.md: `lab/island-design/`): lines, the agent's
turn, the voice and its words, questions, notes (pull-downs), the fast reader, drawing mode's keys, light and Send,
pointing marks, commands and health. One black shape grows out of the notch and returns into it; every element is a
function of its one spring. `lab/island-design/` (DESIGN.md) and `lab/testing/` (TOOLING.md) are untracked: they exist
only in the main checkout, not in worktrees.

**The object (platform-neutral, `object/`, `ui::island::object`):**
- `engine.rs` — `Island`: the interface (lab/testing/TOOLING.md §1.2). `apply(event, t)` is the only way state
  changes; `frame(t)` is pure (shape, every slot, the drawing light, the pointing marks); `needs_frames(t)` says
  Moving / Loop / None (`needs_ticks(t)` is the live window's clock: a loop of thinking dots or the spinner alone
  needs none, their layers loop as Core Animation in the render server; GPU trace 28 Oct 20:05); `deadline()` is the next one-shot change; `hit(t, p)` what a click lands on (only stable
  targets during a motion, so Send keeps taking taps); `track(t)` the shape's motion, never empty. One transition
  rule: the shape springs (DESIGN.md §4), shared elements glide, incoming content fades in scaled to fit, outgoing
  content leaves with the shape; one content layer per place (what leaves a place clears before what arrives there,
  ear pairs and button rows swap together); at most three motions a second, but a change while the shape is still
  growing redirects that growth; never shorter than the notch while as wide; live content (reader units, numerals,
  the waveform, pointer feedback) cuts.
- `scene.rs` — the one state machine: the voice (its look waits up to 0.25 s for its words; ends at once when its audio is cut
  (`SpeechCut`, `Stop`), at the audio's end + 0.45 s otherwise, or 3 s after its last sign of life; then it simply
  retracts), the reply's words (each `SpokenText` sentence is added below the last, none replaced, until the voice
  ends; a voice starting again within 3 s continues the same reply; the row shows the newest sentences within six
  lines, each starting its own line, evenly broken), the last reply (`LastReply` as written, else the spoken
  sentences) and its pull-down, the turn (sent, thinking, working words, compacting), lines (`read_seconds`: 1.8–3.4 s by kind, a little longer for long text, ≤ 8 s; errors too, keeping
  their kind; only health problems rest as a token; a different line replaces one after 1 s, but a line to read, a
  reply shown as text, keeps the island its whole read time and the next waits), questions, notes (each belongs to the agent's turn that showed
  it: a turn ends with its `LastReply` or when a new run starts (idle to thinking: a NO_REPLY run sends no reply), and
  the next reply, spoken reply or note then starts the next turn and retires the last turn's notes; a note opens the pull-down, which pulls itself up after `NOTE_UNATTENDED` = 12 s on screen
  without the pointer over it, `Event::Pointer`, leaving nothing beside the notch; its X, `Target::NoteClose`,
  removes it for good), the reader, drawing mode and its light,
  pointing, and commands/health through `Cues` (`mod.rs`, the queue of results and health moments). `scene(t)` picks
  the one thing to say; `deadline()` the next change.
- `layout.rs` — DESIGN.md §2's template and size classes, numbers from `lab/island-design/render/render.swift`.
- `measure.rs` — the `Measure` trait (`Approx` for the simulator, `Fuzz` adversarial widths) and the wrap: lines break
  on per-word widths (`Measure::words`: each word shaped once, a line's width the sum of its words and spaces), and a
  sum within `EDGE` of the fit is measured whole, so every break matches shaping the whole line.
- `chat_layout.rs` — the conversation's rows, each laid out once and cached (two generations), so a new message lays
  out that message alone.
- `spring.rs`, `geom.rs`,
  `reader.rs` (crates/rsvp plans on the island's clock), `gallery.rs` (one named scenario per state, for the tests
  and the gallery).
- `sim.rs` — the simulator: drives the real `Island` on virtual time and checks invariants I1–I22 every frame (I19 no
  success mark after speech, I20 a reply's words only grow while spoken, I22 an agent's note never stays open
  unattended).
  `tests.rs` — recon bugs as scenarios, the user's flows, grid pairs at 8 phases, a triple slice, seeded random
  sequences, adversarial widths, determinism, coverage, speed.

**Switches (24 Oct).** `Event::Switch` comes from ⌃⌘. ⌃⌘/ ⌃⌘, or a menu tile. Its glyph (mic, eye, islands)
shows in its ear and turns slashed or whole at `SWITCH_TURN` on the same image view, where the system's magic replace
draws or lifts the slash. While Listening or Seeing is off, the ears hold both glyphs at the same dim
(`SWITCH_TOKEN_ALPHA`), the off one slashed, and a click opens the last turn. Under Reduce Motion, symbols change
without a transition and one-off bounces do not play.

**Vella (docs/vella.md).** `Event::Vella` (`UiUpdate::Vella` from the app's Vella link) says the user is dictating to
Vella. On: Vella's mark over "Dictating to Vella" for `VELLA_SECONDS` (a stack), then the mark rests in the left ear
beside Vella's shortcut keys (`Ear::Num`, the keyboard symbol when they are longer than four characters), drawn once
with no effect or timer; a keybind change while on changes only the token. Off: the token goes in one motion. With
Listening off the Listening token shows instead. The mark is `Content::Vella`, Vella's own icon embedded from
`assets/island/vella.png` (rendered by Vella's `scripts/icon.swift`, `assets/island/SOURCES.md`), never redrawn.
Gallery cards `vella_dictating`, `vella_stopped`; renders `lab/ui-renders/vella-dictation/`.

**Older models still used:** `agent_notice.rs` (questions and their answers), `drawing/` (strokes, pointing specs),
`reader.rs` (`ui::reader` types the app uses).

**Countdown (30 Oct, the quota recovery card).** `NoticeAction::Ask { countdown: true, .. }` shows the time left until
`expires_ms` as m:ss (rounded up, so `0:00` only once time is out) beside a 20 pt ring that empties clockwise from
twelve, at the right of the question's first line over the options' dots (with step chips, at the left of their row).
Both are live elements: each second is a cut, no motion. The scene asks for a deadline at each second only while such
a card is on screen (`Model::countdown_until`), none once it is closed, hidden or at `0:00`. The UI never expires a
countdown card: at `0:00` it stays open for the controller to close or replace. Tests `countdown_tests.rs`; renders
`heimdall-ui-demo --recovery-countdown lab/ui-renders/recovery-countdown/`.

**Sending a card with questions left.** On a card with several questions, Send is an arrow disc after the step chips, lit (the accent, and a target)
once anything is answered; it sends what is answered and every other question with an empty answer, which the agent
hears as left unanswered (`Question <id>: User left q1 "…" unanswered`; a T3 question gets the words "(unanswered)",
and Send stays dim on a T3 card while a question that takes only its options is open, as T3 needs every answer). Each chip leads with its state: a tick once
answered, an accent dot while it still needs a pick; the chip is the way to it. The Other row is then for words only;
Return in it brings up the next open question and sends once none is left (`Event::AskReturn`). A one-question card
keeps Send in its Other row, lit with the first tick or typed word. Tests `ask_tests.rs`, `agent_notice` tests;
renders `heimdall-ui-demo --agent` (`island-question-partial*.png`, `island-question-tabs.png`) in
`lab/ui-renders/ask-card/`.

**Copying the island's words.** The words are drawn as layers, so a message is
copied whole rather than selected by the character: a right-click or Control-click on a message of the pulled-down
conversation (the user's or Heimdall's) or on the reply offers Copy (`Layout::copies`, `Island::copy_at`,
`mac/island/copy.rs`); ⌘C while the island has the keyboard copies the field's selection, else the reply
(`Island::copy_shown`); the copy glyph in the reply's band stays. Test `a_message_under_the_pointer_and_the_reply_copy_as_written`.

**Catching up (31 Oct, the archive locked for about four minutes and speech stalled with no sign).** `HealthCue::Busy
{ key, part, label }` from the app ("Catching up · 6,489 notes to save"): the stack `square.stack.3d.up.fill` in soft
white over its words (one line; five-digit counts fit), its layers lighting in turn (`Fx::Variable`, the system's
iterative, reversing variable colour with the others dimmed; breathe without the class or for a symbol not in
`VARIABLE_SYMBOLS`; still under Reduce Motion), for `BUSY_READ` (4 s) from its arrival, then the stack and a dim
hourglass rest in the ears until `Recovered` or `Clear` for its key, silently. The app resends it every few seconds:
the same key with new words changes only the words in place (no new arrival, the read time keeps running, a resting
token stays a token). It sits below a restart and an unacknowledged problem and above the pending command glyph and
the tokens; it never makes the helmet red. Gallery cards `health_catching_up`, `health_catching_up_restart`; check
`heimdall-ui-demo --catching-up <dir>` (renders `lab/archive-lock-1031/renders/`; the restart's arrow turning back
into the stack is a replace transition `renderInContext` never steps, so that state is the gallery's).

**AppKit (`mac/`, declared from `src/mac/mod.rs` with `#[path]`, module paths `crate::mac::*`):**
- `mac/island/` — `paint.rs` (`Tree::apply(&FrameModel)`: the one draw call, live and offscreen; SF Symbols are
  `NSImageView`s playing Apple's `NSSymbolEffect`s; `LightLayers`, the island's outline and the screen's edge as one
  soft line),
  `raster.rs` (each distinct content drawn once, with a blurred twin; note drawings sized to their content, tables
  with columns as wide as their cells up to the screen-bound width, then wrapping), `measure.rs` (CoreText widths,
  per-word sums for SF's text cut under 20 pt, display-cut runs shaped whole; its width cache rotates generations
  instead of emptying, `CACHE_MAX`),
  `host.rs` (the window, the click target, the clock: display link while moving, a timer for loops, one one-shot
  timer per deadline, nothing at rest; an arrival at rest wakes the symbol renderer, `Tree::wake_renderer`, before the
  motion's clock starts, and every frame is painted at its paint time; an apply that runs past a 120 Hz frame delays
  the motion it started, `Island::delay_motion`, so an opening spring starts when its first frame can paint; the
  composer's text editing is readied at install, `Fields::prepare_composer`), `bench.rs` (`--bench-chat`), `mod.rs` (the interface the UI uses: `apply`, `notice`, `alerting`),
  `checks.rs` (`--island`).
- `mac/draw/` — the drawing overlay (strokes, keys, capture; its keys and light are the island's), pointing (opacity
  from the island's frame), ink, hotkey, system profile, the app switch, and `check.rs` (`--drawing`). The overlay
  sits above menus (pop-up menu level + 1) and the island rises above it while it is up (`island::pin`). Over an
  open menu (the menu stays while they draw) it holds a still (`still.rs`): another app's menu survives
  the overlay ordered above it but closes when the overlay takes the keyboard, so the still goes under the ink first;
  Heimdall's own menu is captured shortly after it opens and comes back as the still once Drawing Mode is chosen
  (AppKit fades it out first, about 0.4 s). Send then marks the still.
- `mac/space.rs` (`crate::mac::overlay_space`) — still during Space swipes ("it should always be in
  front, an overlay on top of whatever is behind"): every live island window (the island, its click target, drawing
  mode's overlay, pointing) joins a SkyLight space of Heimdall's own at absolute level 100 each time it is ordered in
  (`CGSSpaceCreate` → `CGSSpaceSetAbsoluteLevel` → `CGSShowSpaces` → `CGSAddWindowsToSpaces`, the Floating Islands
  prototype's proven path), joins again after a display change (the space is made again if it was lost) and leaves it
  at quit. Symbols are resolved at run time; any missing one or failed step leaves today's `overlay_behavior()` path
  alone (the windows slide with the Space) and writes one `island: overlay space …` line to heimdall.log, as does
  the first verified join of each window. Checks and renders never reach SkyLight, except `--island`'s one probe with
  an invisible window parked off every display.
- `mac/streams.rs` (`crate::mac::streams`) — Show Islands on Streams: off by default, the
  island, its click target, drawing mode's overlay and pointing are unshared (`NSWindowSharingType::None`), so Discord,
  OBS, QuickTime and screenshots never show them; on, they are shared read-only, live and for windows made later.
  The floating islands follow the same switch through the helper's `streams.json` (crates/app `islands::streams`).
  Heimdall's own captures exclude them by title either way (`cargo run -p screen --example island_exclusion`).
  `--streams` checks it with macOS's `screencapture` (lab/ui-renders/islands-on-streams/).
- `mac/gallery.rs` — the animation gallery and the visual judge (`--gallery`, `--motion`).

**Interface to the app:** only through `ui::Ui::send(UiUpdate::…)` and `ui::UiAction` callbacks. `HeimdallSpeaking(false)`
is the audio-ended input; `SpokenText` and `SpeechLevel` feed the words and the bars; `LastReply` is the reply as written
for the pull-down.

**Endings and pace ("we don't need success after everything that happened … animations should be
simple").** A sequence ends by retracting: no check after a spoken turn, no "back" after a restart (it leaves), no
chosen-answer stage (an answered question retracts at once), no "Attached" after drawing mode's Send (the flash is the
confirmation), no dimmed words when the voice stops. A command's own result still shows (wanted every
screenshot counted). Springs are unchanged and already quicker than Apple's presets (measured on this Mac with
SwiftUI's `Spring`: smooth 90 % at 0.310 s, snappy 0.254 s, bouncy 0.210 s; the island's grow 0.220 s, retract
0.211 s). Around them: arrival effects start as the shape lands (0.23 s after the motion, was 0.48 s), a swap in place
overlaps its leaving and arriving content below a fifth of their opacity instead of passing an empty frame (readable
by 0.18 s, was 0.27 s), and a motion stops asking for frames once it is within a fifth of a pixel (0.63 s for a growth,
0.52 s for a retraction; was 0.82 s and 0.78 s). `every_motion_keeps_apples_pace` holds those budgets.

**The pull-down: the last turn.** While the last turn has anything to show, the notch itself is a target at rest
(only under a real notch): the click target stays over it alone, transparent, drawing nothing, no timer; nothing
rests beside the notch ("I would actually prefer if there is nothing on the left and right"). A click,
a drag down or a two-finger swipe down shows the last turn under the voice's ears (live bars while it speaks, the
waveform glyph when quiet): its reply left-aligned in a centred column, then that turn's notes only (each under a
header with its icon, title and close X), scrolling as one inside half the screen; a wide table widens the shape up
to `layout::reply_max_w` (700, never closer than 160 to the screen's sides). A click or a drag up on its lip band
pulls it up.
When the pointer reaches the notch (the click target's tracking area, no polling), a short lip with a grabber shows
just below the notch's bottom edge (`Lip::Peek`); the notch and the lip open the last turn,
and it goes as soon as the pointer leaves, so no strip below the notch takes clicks at rest. While Heimdall sends, thinks,
compacts, works or speaks, the whole shape is that target too. ⌃⌘H (`Event::OpenChat`)
pulls the conversation down as the user's, with only its composer when there is nothing yet, and puts the keyboard in the
composer (`Fields::focus_composer`; the click target is a non-activating panel, so Heimdall does not come forward); a
pull-down a note opened becomes their conversation. Pressed again while their conversation shows (`tobys_reply_open`),
however it opened and wherever the keyboard is, it pulls it up; one hidden under the reader or drawing mode stays. A field that had the keyboard and leaves gives it back (the panel is
ordered out and in). The composer's placeholder is the island's drawn one only (`layout::composer_placeholder`, its
one source of words); the native fields (composer and Other) have none, and sit `field::TEXT_SHIFT` off their slots so
typed text lands exactly where the placeholder was drawn. The composer scrolls on its one line; it never wraps. Check: `heimdall-ui-demo --composer <dir>`
(test `island_composer`, half a second): the island with its native views composited (empty, focused, typed, long,
2x crops), real key presses, backspace, ⌃⌘H. A send (`Event::ComposerSent`, then `Event::Sent`) keeps the user's
conversation open through the turn and scrolls it to the end; a pull-down a note opened pulls up into the send arrow.
**The conversation.**
Pulled down by the user, the last turn sits inside today's conversation (`chat.rs`, sent whole by the app as
`UiUpdate::Chat`, built by `crates/app/src/chat_history.rs` from the agent's session file and the archive's
`utterances`): their words to Heimdall in blue bubbles on the right, Heimdall's replies and spoken lines as plain text on
the left (faint with a muted speaker when they never reached them, a small cut-waveform when they talked over them),
quiet centred rows for a task started or finished and a question with the option they chose, and a small card per
notice with a table, text, SVG or image (`object/chat_layout.rs`, drawn as `Mark`s in the pull-down's bodies). It
opens on the latest exchange (their last words, then the turn) and scrolls up through the day, a fade at the bottom
while more is there; what scrolls up goes on under the top bar as a veil; renders
`lab/ui-renders/island-chat-veil-*.png`; with no turn of its own (after a restart) it opens at the end. After a send, and on each new
message, it follows the end (`ReplyView::tail`) unless the user scrolled up away from it. A reply beside a wide table
wraps to the widened column. A pull-down a note opened stays
the note's alone. `chat::split` places the live turn in the conversation by the replies it holds and its notes' ids, so
nothing shows twice. Pulling out (23 Oct: a table could not be pulled out): the band's pin and the drag take the turn's
notes with its reply (`UiAction::PullOut`; text and tables go into the document, an SVG or an image becomes its own
viewer island), each note's header has its own pin beside its X, and a notice's card in the conversation pulls that
notice out. Renders: `lab/ui-renders/island-chat-*.png` (gallery cards `chat_history`, `chat_only`).
A question never hides Heimdall's voice ("I should see you talking, and the decision goes down"):
while they speak, the card docks under their words (the speaking row's own lines, live as sentences arrive) and stays
alone once the voice ends; with the conversation pulled down, it docks at its bottom above the band, outside the
scroll. Renders: `lab/ui-renders/island-question-*.png` (cards `question_while_speaking`, `question_in_chat`).
An open card has no close X; it ends in the pull-down's lip (the same grabber and band, `Target::AskTuck`). A click or a drag up on it tucks the card away unanswered:
nothing goes to the agent, their picks stay, its digit keys go, and the notch (a click, a drag or a swipe down, the
peek lip under the pointer) brings it back ahead of the last turn; their pulled-down conversation docks it. Esc in its
field still dismisses it. A question that arrives while another is tucked shows at once; the tucked one waits behind
the notch, and an answered, closed or expired one is tucked no longer. Without a notch the card has no lip and never
tucks (nothing there could bring it back; moving to such a display brings a tucked card back). Renders:
`lab/question-lip/` (cards `question_tuck`, `no_notch_question`).
**Drops onto the notch.** `drop.rs` is
platform-neutral: the drag's pasteboard as a `Snapshot`, `classify` (files by extension, a temporary screenshot copied
into `~/Library/Application Support/Heimdall/drops/`, links, image data saved there with its web source, text; long
text kept as a file), `summary` (the island's word: "1 image", "Link", "Folder", "3 items") and `Motion`, the drop
target's pure motion: under a drag the notch swells 26 × 10 pt and a cool glow lights its edge; on a drop the item's
thumbnail flies into the shape, shrinking, slides under its edge, and the shape takes it with one soft bounce and
returns into the notch; then the island's own result cue (`CommandKind::Drop`) says what it took. Reduce Motion:
fades only. While the island shows more than the notch (speaking, a line, the conversation) the swell and glow are
the island's own ("the Dynamic Island as a whole isn't lighting up and swelling a little"):
`Motion::swell` gives the same springs as `drop::Swell`, the island's tree draws its shape that much larger with the
halo (`Tree::drop`, set by `Host::set_drop`; content and its mask stay put), the drop window draws only the carried
item, and the hand-over as the island grows or retracts mid-drag is seamless. `mac/drop/` is two windows over the notch, both `Heimdall Drop Target` (excluded from captures, level
above the click target): the drawn one joins the island's overlay space and never takes the mouse; an empty catcher
outside that space takes the drag, because macOS offers no drag to a window in a process's own SkyLight space (24 Oct
12:26, the first build drew nothing for a real drag). Both are ordered in only while a drag is under way anywhere (a global monitor sees the drag pasteboard's count change; a
press-drag whose source writes the pasteboard late is polled every 40 ms while the button is down, `DragWatch`; a
0.2 s check of the button while armed), on the display with the pointer, ordered out with no clock once the button
is up and the motion ended, so it never takes a click or covers the menu bar. Drops are taken within 340 × 100 pt
of the notch's top centre, or over the island's frame and 18 pt round it while it shows: the catcher is exactly that
area and follows the island as it resizes (`Host::render` tells it); the drawn window is the island window's frame. File promises (Photos, Mail) are received into the drops folder. The app decides when
the drop reaches the agent (`crates/app/src/pipeline/drops.rs`). Check: `heimdall-ui-demo --drop <dir>` (synthetic
pasteboards through a fake drag session sent to the view's own drag callbacks, the island's word, rest, and the
contact sheets `drop-sheet.png` and `drop-island-sheet.png`, each island state with the hover off and on and a
drag held while the island grows and retracts; renders in `lab/ui-renders/drop-on-notch/`). Live: `heimdall-ui-demo --drop-live`
with a real Finder or screenshot-thumbnail drag driven by `lab/drop-on-notch-live/`.
**Drops on the open pull-down (dropping attaches to the message being typed, or sends it like a notch drop).** While a drag is armed the click target ignores the mouse (`Host::pass_drags`): it joins the
overlay space, which sits above the catcher's ordinary window stack whatever their levels, so over the pull-down (and
over the notch at rest) a drag found it first and was never offered to the catcher (the journal's "macOS never offered
it to the notch" at 03:00:08). Where a drop goes is `drop::route`: with the pull-down showing and text in the composer
or the keyboard in it, `Event::ComposerAttach` puts it in the composer (a 52 pt thumbnail row above its line, each
with a remove badge, `Target::ComposerDetach`; the swell settles, no flight, no cue) and Return sends their words with it
(`Output::TypedDrop` → `UiAction::TypedWithDrop`, sent at once as one typed message with the images, the relay holding
it like a notch drop); otherwise it goes as a notch drop. Check: `heimdall-ui-demo --pulldown-drop <dir>` (test
`island_pulldown_drop`); renders in `lab/ui-renders/pulldown-drop/`.
Reply text (the pull-down and island lines) is laid out by `object/prose.rs`:
paragraph gaps, list items with hanging lines, the first sentence bold.
**Emergency mode (docs/claude-relay.md).** `Event::Emergency` (`UiUpdate::AgentRelay`): while a backup agent covers,
the agent's glyph and an orange warning triangle rest in the ears, still, after any moment; the notch still opens the
last turn. The emergency `Notice` alone reads and leaves nothing Oct 19:40). Chosen relay mode draws
nothing at rest. Gallery: `health_emergency_rest`, and `no_relay_*` for `crates/app/src/emergency_local.rs`'s cards.
**Nudge.** `Event::Nudge` (`UiUpdate::FloatingNudge` from `crates/app/src/islands/nudge.rs`: a floating island newly needs the user) plays one small
pill at the notch, a bell or a question mark; bursts coalesce and it waits while the island shows anything else. Its
landing jiggle is intended: `FrameModel::jiggle` marks its frames, and the motion judge leaves them out
of its width-reversal count (`mac/gallery.rs`; host test `crates/ui/tests/island_motion_nudge.rs`).

**Testing (offscreen, nothing reaches the screen, unless marked live):**
- `cargo test -p ui --lib island::` — the object and the simulator: grid and random sequences under every invariant.
- `heimdall-ui-demo --island <dir>` — stills of every card's states, window properties, clicks through the click
  target's real event path (answers, the last reply pulled down by a tap and a drag on the notch and up by its band),
  the cost at rest (measured, 3 s).
- `heimdall-ui-demo --drawing <dir>` — drawing mode and pointing in process, Send by a tap (through the overlay, the
  click target, and while the island moves), a Send end to end with a stand-in capture, and over an open menu: the
  levels, the still held before the keyboard is taken, Heimdall's menu's still, and Send marking the still.
- `heimdall-ui-demo --drawing-over-menu <other|own> [secs]` — a live lab check (on screen for a few seconds, no
  pointer events): real drawing mode over an open menu, each change printed with its uptime for a recorder in another
  process (24 Oct: every 60 Hz frame kept the menu from open to close over another app's status menu).
- `heimdall-ui-demo --gallery <dir>` / `--motion <dir>` — every card at 120 Hz through the same draw call, judged
  (symmetry, clipping, smoothness, rest) with the main thread's cost per frame (`anims/judge.json`).
- `heimdall-ui-demo --bench-chat N` / `--bench-layout N` — the first ⌃⌘H of an N-row conversation timed on the
  offscreen host (CoreText calls, cache rotations, apply and commit); env knobs in `mac/island/bench.rs`, results in
  `lab/ui-bench/README.md`. `crates/ui/tests/island_chat_bench.rs` runs it at 400 rows.
- Never run the demo without one of these flags' intent: without a flag it only prints its help; `--live`,
  `--drop-live` and `--drawing-over-menu` are the only flags that show anything on screen.
