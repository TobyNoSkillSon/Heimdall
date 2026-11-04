# In-conversation suggester

While the user talks with someone, Heimdall sometimes offers one card on the island, built from
the last ten minutes of talk. The card asks; it never acts. Their pick wakes the main agent with the suggestion, the talk
it came from, what the search found and their choice, and the agent starts from there.

## Flow

1. Every finished line of talk joins a window (`crates/app/src/suggester.rs`, `Window`: ten minutes, sixty lines).
2. Decider V5 decides when (29 Oct): after each line's send decision, the user's or a call guest's, it is asked its trained
   suggest question with the same conversation (`decide::suggest`, `questions/suggest-v5.json`), and at P(yes) 0.7 or
   more (`pipeline::suggest::SUGGEST_MIN`) a pass starts. On V5's frozen test split that bar fires on 2.8% of lines
   at 0.72 precision and 0.35 recall against Haiku's labels (derived from `runs/v5/test_v5.json`). It is not asked
   while the user is talking to Heimdall: a line of theirs that woke Heimdall or that it waits on, the 20 s after it, and
   while Heimdall speaks; a card that would land in such an exchange is held. Each answer is a `suggest_decision`
   evidence record. The old phrase rule (`stub_suggest_now`) runs only in the lab replay. Passes need a running agent,
   a call allowed by the pacing, and a background model with a usable credential, on any provider
   (`background_model::endpoint`; until 26 Oct only Anthropic).
3. The pass is one background job (`background_call::ask_tools_blocking`, the background model, Haiku 5.5 by default,
   the one-hour prompt cache, the $0.50 limit a run, at most five tool rounds). Its tools: `search_files` (Spotlight under the user's home, Library caches out) and
   `search_timeline` (the archive, read-only). `submit_suggestion` ends it: silence, or one card of one to three
   questions with two to four options each.
4. `verify` checks the answer in code: options and questions well formed, no wording that proposes contacting anyone,
   and every cited path must come from a search in the same run.
5. A card shows for five minutes (`CARD_MS`). "Not now" dismisses it and wakes nothing. Any other answer, typed
   answers included, is a pick and sends the handoff notice (`suggest-pick:` id, classified as an answer in
   `crates/agent/src/notice.rs`).

## Pacing (`Pacer`)

- One model call a minute; no call while a card is up.
- One card every three minutes; a topic is not offered again within two hours (word overlap, `same_topic`).
- Two dismissals in a row pause suggestions for 15 minutes. A pick clears the count.
- Pacing state is in memory: a restart resets it.

## Feedback and labels

- Every decision is appended to `<data>/suggestions/log.jsonl`: `shown`, then one of `picked`, `dismissed`,
  `ignored` or `feedback`; silent answers are `silent` lines with their reason.
- A typed "bad suggestion" or "irrelevant" (or "not relevant") within five minutes of a card attaches to it, closes an
  open card, and is not sent to the agent.
- `heimdall --suggest-export [--log PATH] [--out PATH]` writes one label row per card for decider training: `suggest_now` true for a
  pick, false for a dismissal or typed feedback, null for an ignored card.

## Replay (lab)

`heimdall --suggest-replay OUT_DIR TIMELINE.jsonl...` runs the stub, the pacing and the job over archived talk. Each
timeline file is one conversation, made with the archive CLI (read-only):
`heimdall-archive timeline --since YYYY-MM-DDTHH:MM --until YYYY-MM-DDTHH:MM --kinds utterance`.
It writes `replay.json` and `replay.html`; nothing is shown or sent. Limits: $2.50 and twelve calls per replay. It always
uses Haiku. Its output holds real talk, so keep it out of the repository.

## Prompts

`crates/app/prompt/suggester.md`. The public edition uses `crates/app/prompt/public/suggester.md`, which names no one
During calls cards stay on screen only, and the handoff tells
the agent not to speak.

## Not built

- No Background Tasks toggle: the job follows the background model choice and nothing else.
- The replay still measures the stub's triggers, not V5's.
- Typed feedback is recognised only as a short phrase; any other reply is an ordinary message.
