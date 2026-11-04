//! The help texts, word for word the Swift CLI's (main.swift at 9c9332a).

/// `islands put --help`.
pub const PUT: &str = r#"islands put <id> [options]   Create or update one island in one call (merges; a place User chose is kept).
Words follow docs/island-style.md; put refuses text that breaks the checkable rules.
  --title T                  1-3 words naming the thing: "Qwen-Image", "iPad sale". Never the id. Needed for a new island.
  --state S                  running (working in the background) | waiting (needs User) | done | failed
  --stage WORDS              1-2 words, the present activity: "Downloading", "Final tests", "Your answer"
  --summary TEXT             one plain line: what it is and what is next
  --step "TEXT[=done|now|failed]"   a sequence, once per step in order (replaces the list; --step none clears it)
  --item "TEXT[=running|waiting|discussed|done]"   a plain list, once per item (replaces it; --item none clears it).
                             With a group, the list shows Running / Waiting on you / Discussed headers and ticks.
  --kv "Key=Value"           a few key values (replaces them; --kv none clears them)
  --progress 0..1|none       only a live measured value (a ring and %)
  --image PATH               a picture shown when it is opened
  --open URL|PATH            a button that opens the page or file in the default browser or app
  --icon SF.Symbol  --value V (a result's headline)  --preset task|waiting|result|list|note|viewer|todo
  --parts JSON  --json JSON|-  --expanded | --collapsed  --from-notch
Where:
  --side left|right|top      dock to that edge's stack (top: the row under the notch); default right, plan lists left
  --order N                  place in its stack (lower first)
  --at X,Y                   place freely: its top-left corner in screen points from the display's top-left (y down)
  --auto                     back to its default dock
  Without these, put keeps the place User chose (an island he threw keeps its edge and height).
Example: islands put img --title "Qwen-Image" --state running --stage Downloading --icon photo \
           --summary "20 GB of weights, then convert and install" --step "Download=now" --step "Convert" --step "Install""#;

/// `islands plan --help`.
pub const PLAN: &str = r#"islands plan                              The plan lists (left dock): Goals, Scheduled, Settled, numbered.
islands plan [goals|scheduled|settled] list
islands plan LIST add "TEXT" [--due TIME] [--when W] [--detail T] [--proposal TEXT] [--item-id ID] [--goal GOAL-ID]
                                          Add an item (on Goals a question that needs User: --goal puts it under that
                                          goal, else it is a card at the top; --proposal gives it Yes / No / Later).
                                          Each item gets a stable id (a slug of its text).
islands plan LIST done|undo|rm N|ID|"TEXT"...   Tick off / back on / remove (N from the listing, an id, or a unique part).
islands plan LIST move N|ID|"TEXT" [--to goals|scheduled|settled] [--due TIME] [--when W]
islands plan LIST prune                   Remove ticked items.
LIST defaults to goals. Goals: a few big goals a week or a month ahead with their tasks (written by the Goals job;
listed here) and the questions that need User now; Scheduled = what has to happen at a time, with a due time
(--due "2026-10-24 10:00" | "tomorrow 10:00" | "today 22:00" | tonight | +2h | +30m | +3d): the app counts down to it,
and when it is due it stays in Scheduled, at the top, as due now; Settled = done or agreed."#;

/// The rest of `islands --help`, after the two above.
pub const REST: &str = r#"islands view <path> [--id ID] [--title T] [--action "Label=action"]...   Show any file large in the middle of the screen
                 (markdown, text/code, image, PDF, audio, video, HTML, else QuickLook); --action adds proposed-action buttons on top.
islands open|close <id>...              Open it / back to its pill (aliases expand|collapse, pin|unpin).
islands hide|show <id>...               Take it off screen / bring it back, keeping its file and place.
islands hide-all|show-all               Every island off screen / back where it was; no island file changes. Under Heimdall,
                                        its ⌃⌘, and Show Islands own this and set it again at launch.
islands move <id> X Y | <id> --side left|right|top | <id> --auto
                                        X,Y: top-left corner in screen points from the display's top-left.
islands rm <id>... | --all              Remove islands (never a plan list: edit those with islands plan).
islands clear                           Remove every island but the plan lists, and every pending response.
islands list [--json]                   Islands with where they are now: dock or placed, top-left x,y in screen points, size.
islands state [<id>] [--json]           What is on screen: open, side, x, y (top-left, screen points), width, height.
islands quit                            Quit the app (islands vanish; files stay unless cleared).
islands responses [--consume] [--json]  Show clicks User made (and delete them with --consume).
islands dir                             Print the data folder."#;

/// `islands --help`.
pub fn all() -> String {
    format!("{PUT}\n{PLAN}\n{REST}")
}
