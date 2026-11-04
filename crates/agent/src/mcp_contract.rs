//! Immutable MCP v2 catalog and the handler's schema validator. No coercion.
use anyhow::{bail, ensure, Result};
use serde_json::{json, Value};
pub const PROTOCOL: &str = "2025-06-18";
pub const TEXT_BYTES: usize = 8192;
pub const IMAGE_BYTES: usize = 8 * 1024 * 1024;
pub const REQUEST_BYTES: usize = 128 * 1024;
fn string(min: usize, max: usize) -> Value {
    json!({"type":"string","minLength":min,"maxLength":max})
}
fn choice(values: &[&str]) -> Value {
    json!({"type":"string","enum":values})
}
fn integer(min: usize, max: usize, default: usize) -> Value {
    json!({"type":"integer","minimum":min,"maximum":max,"default":default})
}
fn object(properties: Value, required: &[&str]) -> Value {
    json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
}
/// One question of `ask`'s `questions` form (24 Oct): a step chip, the question, multi-select, Other, and two to four
/// options, each a label or a label with a description.
fn ask_question() -> Value {
    let mut labelled = json!({"label":string(1,LABEL_CHARS),"description":string(0,DESCRIPTION_CHARS)});
    // An option's grant for the deny-list hook (`actions::AskGrant`), offered only once grants are switched on.
    if heimdall_emergency::grants::ENABLED {
        labelled["grant"] = object(json!({
            "delete":{"type":"array","minItems":1,"maxItems":8,"items":string(1,1024)},
            "loginItems":{"type":"array","minItems":1,"maxItems":4,"items":string(1,128)},
        }), &[]);
    }
    let option = json!({"oneOf":[string(1, LABEL_CHARS), object(labelled, &["label"])]});
    object(
        json!({
            "header":string(1,HEADER_CHARS),
            "question":string(1,QUESTION_CHARS),
            "multiSelect":{"type":"boolean","default":false},
            "allowOther":{"type":"boolean","default":true},
            "options":{"type":"array","minItems":2,"maxItems":4,"items":option},
        }),
        &["question","options"],
    )
}
fn tool(name: &str, description: &str, schema: Value, read: bool) -> Value {
    json!({"name":name,"description":description,"inputSchema":schema,"annotations":{"readOnlyHint":read,"destructiveHint":false,"openWorldHint":name.starts_with("t3_") || name.starts_with("task_") || name == "delegate_task"}})
}
/// The pull-down's title: one to three words (the user: "one word, maybe two words, maybe three words").
pub const NOTICE_CHARS: usize = 40;
pub const QUESTION_CHARS: usize = 120;
/// The legacy `options` form (one question, one to three words each).
pub const OPTION_CHARS: usize = 24;
/// The `questions` form (24 Oct, one adaptive card): up to four questions, each with a step chip of up to 12
/// characters, options labelled up to 40 characters with a description of up to 80, and Other's words up to 500.
pub const QUESTIONS_MAX: usize = 4;
pub const HEADER_CHARS: usize = 12;
pub const LABEL_CHARS: usize = 40;
pub const DESCRIPTION_CHARS: usize = 80;
pub const OTHER_CHARS: usize = 500;
/// Pull-down detail bounds: a long answer, a table, an SVG graphic.
pub const DETAIL_TEXT_CHARS: usize = 16_000;
pub const DETAIL_SVG_CHARS: usize = 65_536;
pub const TABLE_COLUMNS: usize = 8;
pub const TABLE_ROWS: usize = 200;
/// Reply path (19 Oct): Heimdall shows no status icons for the agent (the user: "those icons, they shouldn't even show
/// anything … they are only useful for us for testing"); tool outcomes go to the tool log. What is left is content.
const ISLAND_NOTICE: &str = "Show User something to read rather than hear: a table, a list, code, links, a chart or an image. It opens in the island's pull-down under a title of one to three words. detail is text, a table (headers + rows), an SVG drawn on black (use light colours), or an archived image by archive_id. Pass notice_id to replace one you showed earlier. Not for status or progress: say those with speak.";
const ASK: &str = "Ask User one adaptive card: one short question, or up to four in one card, each with two to four options. Options are numbered 1 to 4 on the card; each may carry a short description. Pass questions, each with question, options (a label, or {label, description}), optional header (a step chip of up to 12 characters), multiSelect (several options may be picked) and allowOther (default true: a last row where he types his own answer). A question with options still works for one quick question (labels of one to three words). He answers by click, by Option and a digit, by saying the number (\"2\", \"one and three\", \"both\") or by typing in the last row. Returns pending with request_id. For questions, his answer comes back as one line per question, keyed by its id (q1, q2 ...): the labels he picked, and his typed words. When he talks to you while it is up, the card stays open and you are told; it closes on his click, on Option and a digit, on typed text, on his dismiss (the x or Esc, which comes back as \"dismissed\"), or when you call ask_close because his words answered it. A question longer than one sentence: ask by voice instead. Silence never approves.";
const ASK_CLOSE: &str = "Close an open ask card when his words answer it: call this once what he said, while it was up or before it appeared, answers the question. Talk to you while it is up does not close it. Pass request_id and his answer in a few words.";
/// Pointing and drawing mode (18 Oct). `POINT_GUIDE` is `ui::drawing::AGENT_GUIDE` verbatim (crates/app tests that
/// they match): the commands in a few lines and the instruction to act without deliberating (the user: "just don't
/// think, draw as fast as possible").
pub const POINT_GUIDE: &str = "Draw on User's screen to show him something. Do not deliberate: look at the screenshot \
and draw at once; approximate is fine. One command per line, coordinates in the screenshot's pixels from its top-left:
size W H  (the screenshot's size; first)
circle|box|underline|arrow X Y W H \"label\"  (the target's box; arrow also takes a point X Y)
path <SVG path: M L H V Q C Z> \"label\"
Labels are optional, 1-3 words, never the target's own text. Optional after a label: left|right|above|below. \
Optional after an arrow: from X Y. Optional lines: hold SECONDS, display N (1 is the main display; default: the one \
User is on).";
pub const SPEC_CHARS: usize = 4000;
const POINT_TAIL: &str = " Display-only: his clicks pass through. Returns one line at once; the marks hold while you speak, then fade. Without a screenshot of the screen in this turn, call look first.";
const LOOK: &str = "Bring an app forward (switching to its Space if needed) and get a screenshot of the display User is on, as an image to point at, with its size line for point. Without app, only the screenshot. Use it when no screenshot came with the turn.";
const DRAWING_MODE: &str = "Open or close User's drawing overlay on the display he is on: he draws on the screen, Return attaches the marked screenshot to his next message, Esc closes. Open it when he asks to draw or mark something for you; to show him something yourself, use point.";
// T3 (night 20 Oct, "we should literally take the same tool calls … that agent in T3 code has access
// to"): T3's own orchestrator v2 names. delegate_task/task_status/task_cancel are Heimdall's outside-client version of
// T3's in-thread trio (same meaning: workState, completion wakes the caller); t3_thread_list/read/send are T3's thread
// tools with compact answers ("maximum one tool call … responses from them should be easy").
const T3_TOOLS: &str = "List the rest of T3 Code's orchestrator tools (projects, worktrees, queues, scheduled tasks, pull requests, previews, model settings, archiving), filtered by query words; with tool, that tool's full description and its arguments, one per line.";
const T3_CALL: &str = "Call any tool from t3_tools with its arguments in args, on any thread or project, also ones User started. Returns a short plain answer. You have no current thread or project: always pass threadId or projectId. clientRequestId is filled in. Deleting a project or scheduled task, changing environment settings, and interrupting, restarting, archiving, editing the queue of or any other destructive call on a thread you did not start return CONFIRM_REQUIRED with a question: ask it, and after User clicks Yes repeat the call with confirm set to the ask's request_id. A spoken yes does not count.";
const DELEGATE_TASK: &str = "Start a task in a new T3 thread (full access) and return at once with its taskId, which is also its threadId. task is the thread's whole brief. target picks the model: model id, optional providerInstanceId, and options such as {\"effort\":\"high\"}. path is the project folder (default: the Vault); isolation worktree gives it its own git worktree. When the task is done, a notice brings its final reply in full, so end your turn: no polling. A task waiting on its own sub-agents or background commands is not done and wakes nothing. Heimdall puts the standing task rules above every brief by itself, so write only the task: Heimdall is untouchable, the cursor, system changes, no contacting anyone, checks, design rules, picks, BF16 checks, and the cleanup clause (before its final reply the task permanently deletes, rm -rf on exact paths, the build output it made itself, keeping its deliverables and reporting the disk freed; never cargo clean or a shared target/ another task may be building in, never models, User's data or uncommitted work).";
const TASK_STATUS: &str = "Your tasks. Without taskId: one line each, newest first: taskId, title, state (working, waiting on its sub-agents, done, failed), model. With taskId: its state and its latest reply in full.";
const TASK_CANCEL: &str = "Stop or close one of your tasks: interrupts its running turn, if any, and archives its thread (history kept). Use it when User drops the work, or once he has seen and accepted a finished task's result. Its own sub-agents may still finish on their own.";
const T3_THREAD_LIST: &str = "List T3 threads across all projects, also User's own, one line each: title, state, how long ago, threadId; active ones first, then the most recent. query filters by title words.";
const T3_THREAD_READ: &str = "Read any T3 thread: its state line and its latest messages in full (last, default 3), oldest first. threadIds reads up to five threads in one call. view activity lists the latest timeline items (tool calls, errors, sub-agents) one line each; itemId returns one item's whole text. details adds its runs and settings.";
const T3_THREAD_SEND: &str = "Send a message to any T3 thread, also User's own or one of your tasks. mode: auto (default: starts an idle thread, steers a running turn), steer, queue (a separate later turn) or restart. When the run it starts or joins is done, a notice brings its reply, so end your turn. Restarting a thread you did not start returns CONFIRM_REQUIRED like t3_call.";
// Archive clips ("one tool … where you just choose the time … that outputs essentially a video").
// Served by the app's `mcp_clip.rs` over `heimdall_archive::clip`; tasks run the same as `heimdall-clip`.
pub const CLIP_MAX_NS: u64 = 2 * 3_600_000_000_000;
const ARCHIVE_CLIP: &str = "Turn a time window of the archive into a movie file: the screen of one display at its capture times, each audio source (microphone, apps, every call) on its own track, and the transcript as speaker-labelled subtitles plus an .srt beside it. start and end are at most two hours apart; display_id defaults to the busiest display; width and fps shrink or resample the video; path is an absolute .mp4 or .mov (default: Heimdall's clips folder). Returns the file and what each track holds. Tasks get the same movie by running heimdall-clip.";
// Bug notes (bugnote worker, 19 Oct). The user: "just one tool … write it in plain English". Personal edition only: the
// public build has no bug inbox, and its prompt offers a GitHub report instead (docs/bugs.md).
pub const BUG_NOTE_TEXT_CHARS: usize = 8000;
pub const BUG_NOTE_EVIDENCE_CHARS: usize = 32_000;
#[cfg(not(feature = "public"))]
const BUG_NOTE: &str = "File a bug note for the overnight round that fixes Heimdall: in plain English, what went wrong, when, and what was expected. Use it when User says something is broken, or when you notice something wrong or wasteful yourself (a failed or misleading tool, a wall of JSON, a slow step). Put an error line or a short excerpt of the bad output in evidence. Heimdall stamps the note with the real date and time, so give times of day, and a date only for another day. Returns one line.";
#[cfg(not(feature = "public"))]
fn bug_note_tool() -> Value {
    tool("bug_note",BUG_NOTE,object(json!({"text":string(1,BUG_NOTE_TEXT_CHARS),"evidence":string(1,BUG_NOTE_EVIDENCE_CHARS)}), &["text"]),false)
}
// Scheduled wakes (selfwake, night 21 Oct; the user 01:38: "When I tell you 'at seven, do this', you receive that
// notification later"). Handled by `agent::wakes`.
const SCHEDULE_WAKE: &str = "Wake yourself later with a note to yourself: at a local time (\"07:00\" is the next 07:00; \"2026-10-23 07:00\" a given day) or after in_minutes. The note comes back as a notice that starts a turn, also after a restart; one that came due while Heimdall was off arrives at its next start, marked late. Returns one line with the wake id and its time.";
const LIST_WAKES: &str = "Your pending scheduled wakes, one line each: id, local time, note.";
const CANCEL_WAKE: &str = "Cancel one pending scheduled wake by the id schedule_wake or list_wakes gave.";
fn wake_tools() -> [Value; 3] {
    [
        tool("schedule_wake",SCHEDULE_WAKE,object(json!({"message":string(1,crate::wakes::MESSAGE_CHARS),"at":string(4,16),"in_minutes":{"type":"integer","minimum":1,"maximum":43_200}}), &["message"]),false),
        tool("list_wakes",LIST_WAKES,object(json!({}), &[]),true),
        tool("cancel_wake",CANCEL_WAKE,object(json!({"id":string(1,64)}), &["id"]),false),
    ]
}
// Plan refresher (night 1022, 01:13/01:36; since 31 Oct Goals are their own job and there is no To do list): Scheduled, Settled and the
// unbound job islands update themselves; `refresh_plan` asks for a pass now and returns at once (app `plan_refresher.rs`).
const REFRESH_PLAN: &str = "Refresh the job islands without a thread now, and let the background pass propose a promise for Scheduled when you and User agreed on something with a real time; it also shows a plan list he closed again. Returns at once; if anything changed a notice brings you the short diff. Nothing it drafts enters Scheduled without User's yes, and Goals and Settled keep themselves (their own background job; Settled only from proof), so never edit them for bookkeeping.";
// Quiet speaker naming: voices nobody named carry code labels; this names one, by speech.
/// `timeline_search` ("a lot more token efficient").
const TIMELINE_SEARCH: &str = "Search what User's Mac heard and showed: literal query, vector.query by meaning, time, kinds, tags. One line per moment: time · source · similarity · [handle] matching words. id: a handle, for the whole text. cursor: the More line's. output: full (whole texts, ids), frames (screenshots), clip (a movie of the hits).";
const NAME_VOICE: &str = "Name a voice when User tells you who someone is. voice is the label the transcript shows for them: a code such as \"Speaker Fri 23 Oct 21h #3\" (find it with timeline_search: \"the last person we talked to about the server is John\") or a name to correct; or \"latest\" for the unnamed voice that spoke last in his last conversation (\"I was just talking to Elizabeth\"), no search needed. Every earlier line of that voice is relabelled and Heimdall knows the voice from then on. Returns one line: with \"latest\", also what that voice last said and any other unnamed voice of that conversation, to rename if it was the wrong one.";
// Listening to guests ("listen to Vim right now … or don't listen to him").
const LISTEN_TO: &str = "Start or stop acting on a call guest's requests when User says so (\"listen to Vim\", \"don't listen to him\"). speaker is their name as the transcript shows it. While on, their lines show marked (listened to) and their requests by name wake you; it stays on until he turns it off. Only User's own words count: in a turn a guest woke or steered, it is refused. Returns one line.";
fn voice_tools() -> [Value; 2] {
    [
        tool("name_voice",NAME_VOICE,object(json!({"voice":string(1,128),"name":string(1,128)}), &["voice","name"]),false),
        tool("listen_to",LISTEN_TO,object(json!({"speaker":string(1,128),"on":{"type":"boolean"}}), &["speaker","on"]),false),
    ]
}
// Personalise ("Some people may want a different name, a different icon … you should know how to
// do it"), kept as the older name for `settings`' name, user_name, icon and voice keys (docs/personalise.md).
const PERSONALISE: &str = "Older name for settings; use settings. Changes your name (also your wake word), the name you call User, your mark in the menu bar and notch, and your speaking model and voice, only when User asks. Pass only what changes; no arguments returns the current values and the choices. icon is \"helmet\", an SF Symbol name or an absolute PNG, PDF or SVG path; icon_svg is your own drawing: one colour, a square viewBox, solid filled shapes that read at 16 points. speaking_model with voice picks a preset or a voice of that model. wake_words is ignored: the wake word is the name. Returns one line.";
// Settings ("the agent can change any Heimdall setting … that's kind of the whole point"): every
// setting the menu shows, through the menu's own actions (crates/app/src/settings_tool.rs).
const SETTINGS: &str = "Read or change any Heimdall setting the menu shows. list: every setting and its value. get key: its value and choices. set key value: change it, only when User asks or it plainly serves what he asked. item names the microphone, app or task for extra_microphone, app_audio and background_task (\"enable app audio from Ego\": key app_audio, item Ego, value true); devices and apps go by the names the menu shows. A change applies at once, as a click in the menu would, and the menu shows it. Never switch yourself off: agent false (and engine false while you run on the local engine) is refused unless owner_asked quotes User's own words asking for it, as the transcript has them. agent_model and agent_context restart you at once: say what you are doing first and make it the turn's last call. A new name or user_name reloads you after this turn with the conversation kept, so end the turn. The name is also your wake word. ack_delay is in seconds, as User says it (2 or 2.5). Never quit or restart Heimdall for a setting. Returns one line.";
pub const SETTINGS_KEYS: [&str; 30] = [
    "listening", "seeing", "speaking", "agent", "engine",
    "microphone", "extra_microphone", "app_audio",
    "recording_resolution", "recording_fps", "islands_on_streams",
    "agent_model", "agent_thinking", "agent_speed", "agent_context", "agent_relay", "agent_relay_model", "agent_relay_account",
    "name", "user_name", "icon", "speaking_model", "voice", "voice_speed", "wake_sound", "ack_delay",
    "background_model", "background_task", "show_islands", "launch_at_login",
];
pub const SPEAKING_MODELS: [&str; 5] = ["drama3", "supertonic", "phonon", "own_engine", "elevenlabs"];
pub const NAME_CHARS: usize = 32;
pub const ICON_SVG_CHARS: usize = 20_000;
fn identity_tools() -> [Value; 2] {
    let name = string(1, NAME_CHARS);
    let value = json!({"oneOf":[string(1,ICON_SVG_CHARS),{"type":"boolean"},{"type":"number","minimum":0,"maximum":10_000_000u64}]});
    [
        tool("personalise",PERSONALISE,object(json!({"name":name,"user_name":name,"wake_words":{"type":"array","minItems":0,"maxItems":8,"items":name},
            "icon":string(1,4096),"icon_svg":string(1,ICON_SVG_CHARS),"speaking_model":choice(&SPEAKING_MODELS),"voice":string(1,128)}), &[]),false),
        tool("settings",SETTINGS,object(json!({"action":choice(&["list","get","set"]),"key":choice(&SETTINGS_KEYS),"item":string(1,128),
            "value":value,"owner_asked":string(1,500)}), &["action"]),false),
    ]
}
// Taste ledger: the user's judgements, kept for a later model or reviewer (`crate::taste`).
const TASTE_NOTE: &str = "Record one judgement of User's in the taste ledger the moment you hear it: praise, a rejection or complaint, a correction of you, or a standing preference. words are his exact words; why is the quality he cares about, in one line (\"reports must read at a glance\"); subject names what he judged. Add what he was looking at: lines (handles or ids from timeline_search, the line he said it in first, then any context), screen_at (when the screen showed it) and files (paths or URLs of the work). at defaults to the first line's time, else now. When he judges a change made after an earlier verdict, pass that entry's id as follows. A repeat is skipped. Returns one line.";
const TASTE_SEARCH: &str = "Search User's past judgements in the taste ledger by topic words (\"report headings\", \"voice\"), optionally one verdict and a since date. One line each, best match first: time, verdict, kind, subject, his words, why, id.";
fn taste_tools() -> [Value; 2] {
    let refs = |max: usize| json!({"type":"array","minItems":1,"maxItems":max,"items":string(1,crate::taste::REF_CHARS)});
    let time = json!({"type":"string","format":"date-time","maxLength":64});
    [
        tool("taste_note",TASTE_NOTE,object(json!({
            "subject":string(1,crate::taste::SUBJECT_CHARS),"kind":choice(&crate::taste::KINDS),"verdict":choice(&crate::taste::VERDICTS),
            "words":string(1,crate::taste::WORDS_CHARS),"why":string(1,crate::taste::WHY_CHARS),"at":time,
            "lines":refs(crate::taste::MAX_LINES),"screen_at":time,"files":refs(crate::taste::MAX_FILES),"follows":string(7,64)}), &["subject","kind","verdict","words","why"]),false),
        tool("taste_search",TASTE_SEARCH,object(json!({"query":string(1,200),"verdict":choice(&crate::taste::VERDICTS),"since":time,"limit":integer(1,crate::taste::SEARCH_LIMIT,10)}), &[]),true),
    ]
}
fn plan_refresher_tools() -> [Value; 1] {
    [tool("refresh_plan",REFRESH_PLAN,object(json!({}), &[]),false)]
}
// ---- floating islands host (night 1022) ----
// The agent's Floating Islands tools, served by crates/app/src/islands/tools.rs; words checked there against
// islands/docs/island-style.md.
const ISLANDS_PUT: &str = "Show or update a floating island on User's screen for work that runs a while. One call carries title (1-3 words naming the thing), state (running, waiting, done, failed), stage (1-2 words: \"Downloading\"), summary (one line), steps, items, kv, image, open (a URL or file shown as a button) and parts (text, stat, bars, line, list, image, button, toggle, slider), and icon: an SF Symbol name or a tiny SVG of plain shapes drawing what the island is about. Merges into the island with that id; an empty list clears it. thread binds it to a T3 thread: Heimdall then keeps its state, stage and the thread's questions (as buttons) in sync, also through new runs. sent is what a job was sent to do, one line, kept once written; goal is the Goals id it serves (plan_edit show), drawn as a chip and lighting that goal while it runs. His clicks come back as notices. Returns one line.";
const ISLANDS_VIEW: &str = "Open any file in a viewer island: markdown, text, code, image, PDF, audio, video, HTML, or anything Quick Look shows. actions puts proposed-action buttons at its top; his click comes back as a notice. An HTML report speaks back too: its own buttons call window.heimdall.send(action, value, label) and his choice reaches you as a notice; send(\"save\", {file: \"picks.json\", data}) also writes the picks beside the page. Returns one line.";
const ISLANDS_REMOVE: &str = "Remove islands by id once their work is over and User has seen the result. Returns one line.";
const PLAN_EDIT: &str = "User's plan islands: Goals (his goals and their steps), Scheduled (promises with a time) and Settled (finished work with its proof). Nothing enters Goals or Scheduled without his yes, so when you want something there, propose it: a new goal (list goals, icon, about), a next step under a goal (list goals, goal: its id from show) or a promise (list scheduled, due: the local date-time, \"2026-10-24T10:00\"; tonight is 23:00), placed where it would live, with why: the words behind it. One proposal shows per list; say it to him in one short line. His yes by voice is accept (quote his words; due when he names another time: \"move it to Thursday\"), his no is decline; a click on Yes arrives as a notice and is already done. Unanswered, a proposal is archived after two days and never comes back. add is only for what he asked for in his own words: a promise with due and quote (his words), or an ask in Goals (kind and goal: something only he can do now, a decision, an approval, a test; it shows on Scheduled's waiting line until answered; a proposal on it gives his buttons). Settled takes proof only (proof: a commit, a passing test run, an install or an output file; or quote: his own words for a decision). move changes a promise's time, tick and undo mark what is done, remove takes something off; show puts a list on screen again (also one he closed) and returns what it holds, proposals and what waits on him included. item is a short line naming the project first (\"Arsenal: pick the icon\"), or an existing item's or proposal's id or title. Returns one line.";
fn islands_tools() -> [Value; 4] {
    let action = object(json!({"label":string(1,OPTION_CHARS),"action":string(1,64)}), &["label","action"]);
    let actions = json!({"type":"array","minItems":1,"maxItems":4,"items":action});
    let path = string(1, 4096);
    [
        tool("islands_put",ISLANDS_PUT,object(json!({
            "id":string(1,64),"title":string(1,40),"state":choice(&["running","waiting","done","failed"]),"stage":string(1,40),"summary":string(1,200),
            "steps":{"type":"array","maxItems":20,"items":object(json!({"title":string(1,80),"state":choice(&["done","now","failed","todo"])}), &["title"])},
            "items":{"type":"array","maxItems":30,"items":object(json!({"title":string(1,100),"group":choice(&["running","waiting","discussed"]),"done":{"type":"boolean"}}), &["title"])},
            "kv":{"type":"array","maxItems":8,"items":object(json!({"label":string(1,40),"value":string(1,80)}), &["label","value"])},
            "image":path,"open":path,"parts":{"type":"array","maxItems":20,"items":{"type":"object"}},
            "icon":string(0,2048),
            "side":choice(&["left","right","top"]),"thread":string(1,512),"expanded":{"type":"boolean"},
            "sent":string(1,110),"goal":string(0,64)}), &["id"]),false),
        tool("islands_view",ISLANDS_VIEW,object(json!({"path":path,"title":string(1,40),"actions":actions}), &["path"]),false),
        tool("islands_remove",ISLANDS_REMOVE,object(json!({"ids":{"type":"array","minItems":1,"maxItems":20,"items":string(1,64)}}), &["ids"]),false),
        tool("plan_edit",PLAN_EDIT,object(json!({
            "action":choice(&["propose","accept","decline","add","move","tick","undo","remove","show"]),"item":string(1,100),"list":choice(&["goals","scheduled","settled"]),
            "due":string(16,40),"when":string(1,40),"detail":string(1,2000),"why":string(1,300),"quote":string(1,500),"proof":string(1,500),
            "icon":string(1,64),"about":string(1,140),
            "kind":choice(&["approve","test","pick","send","decide","review","write","fix","answer"]),"goal":string(0,64),
            "proposal":object(json!({"text":string(1,4000),"actions":actions}), &["text"])}), &["action"]),false),
    ]
}
// ---- end floating islands host ----
/// The tools that write Heimdall's own state in one synchronous step sequence: the plan and islands files, the bug
/// inbox and its receipt, the wake store. Agent off lets one already running finish (`DeliveryGate::crucial_write`;
/// "any tool call that is writing state, such as a plan edit"). Every other tool stops at once:
/// reads, speech and display tools, and T3 calls, whose network steps Off cancels as before.
pub fn writes_state(name: &str) -> bool {
    matches!(name, "plan_edit" | "islands_put" | "islands_view" | "islands_remove" | "bug_note" | "schedule_wake" | "cancel_wake" | "taste_note")
}
/// `island_notice.detail`: exactly one of text, table, svg or image (by `type`).
fn detail() -> Value {
    let cell = string(0, 200);
    json!({"oneOf":[
        object(json!({"type":choice(&["text"]),"text":string(1,DETAIL_TEXT_CHARS)}), &["type","text"]),
        object(json!({"type":choice(&["table"]),"headers":{"type":"array","minItems":1,"maxItems":TABLE_COLUMNS,"items":string(1,64)},"rows":{"type":"array","minItems":1,"maxItems":TABLE_ROWS,"items":{"type":"array","minItems":1,"maxItems":TABLE_COLUMNS,"items":cell}}}), &["type","headers","rows"]),
        object(json!({"type":choice(&["svg"]),"svg":string(1,DETAIL_SVG_CHARS)}), &["type","svg"]),
        object(json!({"type":choice(&["image"]),"archive_id":string(1,512)}), &["type","archive_id"]),
    ]})
}
pub fn tools() -> Value {
    let id = string(1, 512);
    let time = json!({"type":"string","format":"date-time","maxLength":64});
    let tag = object(
        json!({"key":choice(&["speaker","source","app","language","command_kind","agent","listening","window_title"]),"value":string(1,256)}),
        &["key", "value"],
    );
    let mut list = json!([
        tool("speak","Talk to User while you work: an update, a question, something he should know now. He reads it on the island; with Speaking on he also hears it in Heimdall's own voice. Your final reply reaches him the same way when your turn ends, so never repeat it here. The only way to make sound: never use say, other TTS or audio files, and never search files for a voice. Returns at once; speech stops when User talks. While User talks with other people (a call), Heimdall holds speech unless he addressed you by name, shows it silently on the island, and the result says so.",object(json!({"text":string(1,4096),"language":choice(&["en","pl"])}), &["text"]),false),
        tool("screen_frames","Get one current screen preview or up to four archived frames by time or chunk ID.",object(json!({"at":string(1,64),"chunk_id":id,"display_id":string(1,32),"count":integer(1,4,1),"quality":choice(&["preview","full"])}), &[]),true),
        tool("timeline_search",TIMELINE_SEARCH,object(json!({"output":choice(&["lines","full","frames","clip"]),"query":string(1,2048),"start":time,"end":time,"kinds":{"type":"array","maxItems":5,"items":choice(&["text","speech_audio","other_audio","moment","screen"])},"tags":{"type":"array","maxItems":16,"items":tag},"limit":integer(1,50,10),"id":id,"cursor":id,"vector":{"oneOf":[object(json!({"query":string(1,8192)}), &["query"]),object(json!({"model_id":string(1,256),"model_version":string(1,256),"values":{"type":"array","minItems":1,"maxItems":32768,"items":{"type":"number"}}}), &["model_id","model_version","values"])]}}), &[]),true),
        tool("archive_clip",ARCHIVE_CLIP,object(json!({"start":time,"end":time,"display_id":string(1,32),"width":{"type":"integer","minimum":64,"maximum":4096},"fps":{"type":"integer","minimum":1,"maximum":60},"path":string(1,4096)}), &["start","end"]),false),
        // island-ui: content for the pull-down (a title and its detail) and choice questions.
        tool("island_notice",ISLAND_NOTICE,object(json!({"text":string(1,NOTICE_CHARS),"notice_id":id,"detail":detail()}), &["text","detail"]),false),
        // One object with two forms, never both: question + options (legacy), or questions (one to four). The
        // handler (`actions::parse_ask`) enforces which, so the schema stays a single flat object for every provider.
        tool("ask",ASK,object(json!({"question":string(1,QUESTION_CHARS),"options":{"type":"array","minItems":2,"maxItems":4,"items":string(1,OPTION_CHARS)},"questions":{"type":"array","minItems":1,"maxItems":QUESTIONS_MAX,"items":ask_question()}}), &[]),false),
        tool("ask_close",ASK_CLOSE,object(json!({"request_id":id,"answer":string(1,200)}), &["request_id","answer"]),false),
        // Fast reading (rsvp-reader).
        tool("fast_read","Show User a longer explanation or recap to read word by word in the island (he clicks to start, sets the speed, can pause and replay). Write for one pass at about 240 words per minute: plain spoken English, the answer first, sentences of 8 to 16 words with the verb early, at most one number per sentence, the noun repeated instead of a distant 'it'; no lists, tables, parentheses, code, paths or markdown. Not for tables or anything he must keep exactly. Returns pending; 'read at N wpm' or 'dismissed' arrives later as a notice.",object(json!({"text":string(1,20000),"title":string(1,40)}), &["text"]),false),
        // Pointing and drawing mode (draw-overlay).
        tool("point",&format!("{POINT_GUIDE}{POINT_TAIL}"),object(json!({"spec":string(1,SPEC_CHARS)}), &["spec"]),false),
        tool("look",LOOK,object(json!({"app":string(1,128)}), &[]),true),
        tool("drawing_mode",DRAWING_MODE,object(json!({"open":{"type":"boolean"}}), &["open"]),false),
        tool("reload_me","Schedule supervised Pi/MCP reload after this turn settles. Preserves the conversation and returns a durable receipt.",object(json!({"reason":string(1,500)}), &[]),false),
        tool("delegate_task",DELEGATE_TASK,object(json!({"task":string(1,65536),"title":string(1,160),"target":object(json!({"model":string(1,256),"providerInstanceId":string(1,128),"options":{"type":"object"}}), &["model"]),"path":string(1,4096),"isolation":choice(&["shared","worktree"]),"branch":string(1,256)}), &["task","title","target"]),false),
        tool("task_status",TASK_STATUS,object(json!({"taskId":id}), &[]),true),
        tool("task_cancel",TASK_CANCEL,object(json!({"taskId":id,"reason":string(1,500)}), &["taskId"]),false),
        tool("t3_thread_list",T3_THREAD_LIST,object(json!({"query":string(1,200),"limit":{"type":"integer","minimum":1,"maximum":50}}), &[]),true),
        tool("t3_thread_read",T3_THREAD_READ,object(json!({"threadId":id,"threadIds":{"type":"array","minItems":1,"maxItems":5,"items":id},"last":{"type":"integer","minimum":1,"maximum":20},"view":choice(&["messages","activity"]),"itemId":id,"textOffset":{"type":"integer","minimum":0,"maximum":1_000_000_000u64},"details":{"type":"boolean"}}), &[]),true),
        tool("t3_thread_send",T3_THREAD_SEND,object(json!({"threadId":id,"message":string(1,65536),"mode":choice(&["auto","steer","queue","restart"]),"confirm":id}), &["threadId","message"]),false),
        // The rest of T3's orchestrator surface through its live catalog (t3/orchestrator.rs).
        tool("t3_tools",T3_TOOLS,object(json!({"query":string(1,200),"tool":string(1,128)}), &[]),true),
        tool("t3_call",T3_CALL,object(json!({"tool":string(1,128),"args":{"type":"object"},"confirm":id}), &["tool"]),false)
    ]);
    // Bug notes: appended last, so the other tools keep their order. Personal edition only.
    #[cfg(not(feature = "public"))]
    list.as_array_mut().unwrap().push(bug_note_tool());
    // Scheduled wakes: appended after bug notes.
    list.as_array_mut().unwrap().extend(wake_tools());
    // Plan refresher: appended after the wake tools.
    list.as_array_mut().unwrap().extend(plan_refresher_tools());
    // Floating islands (night 1022): appended after the wakes.
    list.as_array_mut().unwrap().extend(islands_tools());
    // Quiet speaker naming (23 Oct): appended last.
    list.as_array_mut().unwrap().extend(voice_tools());
    // Taste ledger (24 Oct): appended last. Personal edition only; hidden in the public alpha.
    if !cfg!(feature = "public") {
        list.as_array_mut().unwrap().extend(taste_tools());
    }
    // Personalise (26 Oct): appended last.
    list.as_array_mut().unwrap().extend(identity_tools());
    list
}
fn validate(schema: &Value, value: &Value, field: &str) -> Result<()> {
    if let Some(alternatives) = schema["oneOf"].as_array() {
        let matching = alternatives.iter().filter(|s| validate(s, value, field).is_ok()).count();
        if matching != 1 {
            // A tagged alternative (`type`) says what is wrong inside it, not just "wrong type".
            if let Some(tagged) = alternatives.iter().find(|s| {
                value.get("type").is_some_and(|t| s["properties"]["type"]["enum"].as_array().is_some_and(|e| e.contains(t)))
            }) {
                validate(tagged, value, field)?;
            }
            bail!("{field}: wrong type; follow tools/list");
        }
        return Ok(());
    }
    if let Some(values) = schema["enum"].as_array() {
        ensure!(
            values.contains(value),
            "{field}: unsupported value; follow tools/list"
        );
    }
    match schema["type"].as_str() {
        Some("object") => {
            let map = value
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("{field}: expected object"))?;
            // An object without declared properties (t3_call.args) is checked by its own live schema later.
            let Some(props) = schema["properties"].as_object() else { return Ok(()) };
            for key in schema["required"].as_array().unwrap() {
                ensure!(
                    map.contains_key(key.as_str().unwrap()),
                    "{field}.{}: required",
                    key.as_str().unwrap()
                );
            }
            for (key, v) in map {
                let s = props
                    .get(key)
                    .ok_or_else(|| anyhow::anyhow!("{field}.{key}: unknown field; remove it"))?;
                validate(s, v, &format!("{field}.{key}"))?;
            }
        }
        Some("string") => {
            let text = value
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("{field}: expected string"))?;
            let n = text.chars().count();
            ensure!(
                n >= schema["minLength"].as_u64().unwrap_or(0) as usize
                    && n <= schema["maxLength"].as_u64().unwrap_or(u64::MAX) as usize,
                "{field}: string length outside bounds; follow tools/list"
            );
            ensure!(
                text.is_empty() || !text.trim().is_empty(),
                "{field}: blank text; supply text"
            );
            if schema["format"] == "date-time" {
                wall_ns(text)?;
            }
        }
        Some("integer") => {
            let n = value
                .as_u64()
                .ok_or_else(|| anyhow::anyhow!("{field}: expected nonnegative integer"))?;
            ensure!(
                n >= schema["minimum"].as_u64().unwrap_or(0)
                    && n <= schema["maximum"].as_u64().unwrap_or(u64::MAX),
                "{field}: integer outside bounds; follow tools/list"
            );
        }
        Some("number") => {
            let n = value.as_f64().filter(|n| n.is_finite()).ok_or_else(|| anyhow::anyhow!("{field}: expected finite number"))?;
            // Bounds when the schema has them (the settings tool's value: seconds such as 2.5, never negative).
            ensure!(schema["minimum"].as_f64().is_none_or(|m| n >= m) && schema["maximum"].as_f64().is_none_or(|m| n <= m), "{field}: number outside bounds; follow tools/list");
        }
        Some("boolean") => ensure!(value.is_boolean(), "{field}: expected boolean"),
        Some("array") => {
            let rows = value
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("{field}: expected array"))?;
            ensure!(
                rows.len() >= schema["minItems"].as_u64().unwrap_or(0) as usize && rows.len() <= schema["maxItems"].as_u64().unwrap_or(u64::MAX) as usize,
                "{field}: too many items"
            );
            for row in rows {
                validate(&schema["items"], row, field)?;
            }
        }
        _ => bail!("invalid server schema"),
    }
    Ok(())
}
pub fn wall_ns(text: &str) -> Result<u64> {
    let dt = chrono::DateTime::parse_from_rfc3339(text)
        .map_err(|_| anyhow::anyhow!("time: use RFC3339 with an explicit offset"))?;
    let ns = dt
        .timestamp_nanos_opt()
        .filter(|n| *n >= 0)
        .ok_or_else(|| anyhow::anyhow!("time outside supported range"))?;
    Ok(ns as u64)
}
pub fn utc(ns: u64) -> String {
    chrono::DateTime::from_timestamp((ns / 1_000_000_000) as i64, (ns % 1_000_000_000) as u32)
        .unwrap()
        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}
pub fn validate_tool(name: &str, args: &Value) -> Result<()> {
    let list = tools();
    let schema = list
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == name)
        .ok_or_else(|| anyhow::anyhow!("unknown tool"))?;
    // `island_notice.kind` (status icons, retired 19 Oct) is accepted and ignored: a session that learned it from
    // earlier turns must not fail its notice over it. It is no longer advertised.
    let without_kind;
    let args = if name == "island_notice" && args.get("kind").is_some() {
        let mut a = args.clone();
        a.as_object_mut().map(|o| o.remove("kind"));
        without_kind = a;
        &without_kind
    } else {
        args
    };
    validate(&schema["inputSchema"], args, "arguments")?;
    match name {
        "screen_frames" => {
            ensure!(
                args.get("at").is_none() || args.get("chunk_id").is_none(),
                "at excludes chunk_id; use one selector"
            );
            if let Some(at) = args["at"].as_str() {
                if at != "now" {
                    wall_ns(at)?;
                }
            }
            if args.get("chunk_id").is_none() && args["at"].as_str().unwrap_or("now") == "now" {
                ensure!(
                    args["count"].as_u64().unwrap_or(1) == 1,
                    "current capture requires count=1"
                );
            }
            if let Some(d) = args["display_id"].as_str() {
                d.parse::<u32>()
                    .map_err(|_| anyhow::anyhow!("display_id: use a numeric display ID"))?;
            }
        }
        "timeline_search" => {
            if args.get("cursor").is_some() {
                ensure!(
                    args.as_object().unwrap().len() == 1,
                    "cursor excludes all other fields; send cursor alone"
                );
            } else if args.get("id").is_some() {
                ensure!(
                    args.as_object().unwrap().len() == 1,
                    "id excludes filters; send id alone"
                );
            } else {
                ensure!(
                    args.get("vector").is_some() || args.get("query").is_some()
                        || args.get("start").is_some()
                        || args.get("end").is_some()
                        || args["tags"].as_array().is_some_and(|a| !a.is_empty()),
                    "provide query, time boundary or tags"
                );
            }
            if let (Some(a), Some(b)) = (args["start"].as_str(), args["end"].as_str()) {
                ensure!(
                    wall_ns(a)? <= wall_ns(b)?,
                    "end precedes start; use end at or after start"
                );
            }
        }
        "settings" => {
            let action = args["action"].as_str().unwrap_or("");
            ensure!(action == "list" || args.get("key").is_some(), "{action} needs key; call list for the keys");
            ensure!(action == "set" || (args.get("value").is_none() && args.get("owner_asked").is_none()), "value and owner_asked go with set");
            ensure!(action != "set" || args.get("value").is_some(), "set needs value");
            ensure!(action != "list" || args.as_object().is_some_and(|o| o.len() == 1), "list takes nothing else");
            let itemised = matches!(args["key"].as_str(), Some("extra_microphone" | "app_audio" | "background_task"));
            ensure!(action != "set" || !itemised || args.get("item").is_some(), "this key needs item: the microphone, app or task");
        }
        "personalise" => {
            ensure!(args.get("icon").is_none() || args.get("icon_svg").is_none(), "icon excludes icon_svg; pass one");
            if let Some(icon) = args["icon"].as_str() {
                let path = std::path::Path::new(icon);
                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
                ensure!(
                    if icon.contains('/') { path.is_absolute() && matches!(ext.as_str(), "png" | "pdf" | "svg") }
                    else { icon.len() <= 64 && icon.chars().all(|c| c.is_ascii_alphanumeric() || c == '.') },
                    "icon: \"helmet\", an SF Symbol name, or an absolute .png, .pdf or .svg path"
                );
            }
            if let Some(svg) = args["icon_svg"].as_str() {
                let head = svg.trim_start();
                ensure!(head.starts_with("<svg") || head.starts_with("<?xml"), "icon_svg: SVG source starting with <svg");
            }
            ensure!(args.get("voice").is_none() || args.get("speaking_model").is_some(), "voice needs speaking_model: the model it belongs to");
        }
        "archive_clip" => {
            let (a, b) = (wall_ns(args["start"].as_str().unwrap())?, wall_ns(args["end"].as_str().unwrap())?);
            ensure!(b > a, "end precedes start; use end after start");
            ensure!(b - a <= CLIP_MAX_NS, "a clip is at most two hours; split the window");
            if let Some(d) = args["display_id"].as_str() {
                d.parse::<u32>().map_err(|_| anyhow::anyhow!("display_id: use a numeric display ID"))?;
            }
            if let Some(path) = args["path"].as_str() {
                let path = std::path::Path::new(path);
                ensure!(path.is_absolute(), "path: use an absolute path");
                ensure!(
                    path.extension().and_then(|e| e.to_str()).is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "mp4" | "mov" | "m4v")),
                    "path: end it in .mp4 or .mov"
                );
            }
        }
        "island_notice" => {
            if let Some(detail) = args.get("detail").filter(|d| d["type"] == "table") {
                let columns = detail["headers"].as_array().unwrap().len();
                ensure!(
                    detail["rows"].as_array().unwrap().iter().all(|r| r.as_array().unwrap().len() <= columns),
                    "detail.rows: a row has more cells than headers; one cell per header"
                );
            }
            if let Some(svg) = args["detail"]["svg"].as_str() {
                ensure!(svg.trim_start().starts_with('<') && svg.contains("<svg"), "detail.svg: send one <svg> document");
            }
        }
        // Both forms, one card: the same checks the handler runs when the user's card is built.
        "ask" => {
            crate::actions::parse_ask(args)?;
        }
        "delegate_task" => {
            if let Some(path) = args["path"].as_str() {
                ensure!(std::path::Path::new(path).is_absolute(), "path: use an absolute project path");
            }
            ensure!(
                args.get("branch").is_none() || args["isolation"] == "worktree",
                "branch requires isolation worktree"
            );
            for (key, value) in args["target"]["options"].as_object().into_iter().flatten() {
                ensure!(
                    value.is_boolean() || value.as_str().is_some_and(|v| !v.trim().is_empty() && v.len() <= 256),
                    "target.options.{key}: use a string or true/false"
                );
            }
        }
        "schedule_wake" => {
            ensure!(args.get("at").is_some() != args.get("in_minutes").is_some(), "give at or in_minutes, not both");
            if let Some(at) = args["at"].as_str() {
                crate::wakes::When::parse_at(at)?;
            }
        }
        "t3_thread_read" => {
            ensure!(
                args.get("threadId").is_some() != args.get("threadIds").is_some(),
                "pass threadId or threadIds"
            );
            ensure!(
                args.get("itemId").is_none() || args.get("threadId").is_some(),
                "itemId reads one thread: pass threadId"
            );
            ensure!(
                args.get("textOffset").is_none() || args.get("itemId").is_some(),
                "textOffset continues an item: pass itemId"
            );
        }
        _ => {}
    }
    Ok(())
}
