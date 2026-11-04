//! Precision gate for the decider's ack line (acks are "too aggressive and not very precise"; he
//! said to stop overusing Opus and heard "Stopping now." as Heimdall stopping). Decider v3 picks a line from the user's
//! words; these deterministic rules then check the line against the same words, so a line claiming an action plays
//! only when he clearly asked for that action:
//! - Stop ("Stopping now.") needs a stop or cancel order: "stop", "cancel", "abort", "kill", "halt", "pause",
//!   "forget about it", "never mind". "Stop using X" is a preference, not a stop.
//! - Changing course needs a correction ("instead", "actually", "switch", "forget that"…); a worker line needs workers
//!   or agents named; a look line needs something to look at, and never when he says he will look himself.
//! - Doing lines ("On it.", "Will do.", "Right away."…) need a request to act (an order, "can you", "let's"…), never
//!   a plain question or a remark.
//! - Checking and thinking lines ("Let me check.", "Let me think.") need a question or a request.
//! - When the line does not fit: a directive or a question gets the neutral "Got it."; anything else gets nothing.
//! - Nothing at all for greetings and "are you there" (the reply itself comes at once), "don't talk to me", and
//!   turns of three words or fewer ("Okay, cool.", "Great.").
use crate::acks;

/// What the gate makes of the decider's line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Gated {
    /// The decider's line fits his words.
    Keep,
    /// It claims more than he asked: the neutral line (`NEUTRAL`) instead.
    Neutral(&'static str),
    /// No ack this turn; the reason is journalled.
    Skip(&'static str),
}
/// The neutral acknowledgement: it claims nothing.
pub const NEUTRAL: &str = "en_got_it";

/// What a line claims.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Claim {
    Stop,
    Course,
    Worker,
    Look,
    Do,
    Check,
    Neutral,
}
fn claim(id: &str) -> Claim {
    let base = acks::resolve(id).map_or(id, |a| a.variant_of.as_deref().unwrap_or(&a.id));
    match base.strip_prefix("en_").unwrap_or(base) {
        "stopping_now" => Claim::Stop,
        "got_it_changing_course" => Claim::Course,
        "ill_get_a_worker_on_it" => Claim::Worker,
        "having_a_look" | "ill_have_a_look" | "let_me_take_a_look" => Claim::Look,
        "got_it" | "noted" | "duly_noted" | "understood" => Claim::Neutral,
        "checking" | "let_me_check" | "one_sec" | "give_me_a_moment" | "looking_into_it" | "reading_it" | "looking_it_up"
        | "let_me_find_out" | "let_me_think_short" | "working_through_it" => Claim::Check,
        _ => Claim::Do,
    }
}

/// Lower-case words, apostrophes kept ("don't").
fn words(text: &str) -> Vec<String> {
    text.replace('’', "'")
        .to_lowercase()
        .split(|c: char| !(c.is_alphanumeric() || c == '\''))
        .map(|w| w.trim_matches('\''))
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect()
}
/// Words with his lead-ins ("okay, by the way, just …") dropped.
fn without_lead(mut w: Vec<String>) -> Vec<String> {
    const LEAD: &[&str] = &[
        "okay", "ok", "so", "and", "but", "oh", "uh", "um", "hmm", "yeah", "yes", "hey", "heimdall", "also", "then",
        "now", "well", "alright", "right", "listen", "actually", "plus", "dude", "just", "like", "maybe",
    ];
    loop {
        if w.first().is_some_and(|f| LEAD.contains(&f.as_str())) {
            w.remove(0);
        } else if w.len() >= 3 && w[0] == "by" && w[1] == "the" && w[2] == "way" {
            w.drain(..3);
        } else if w.len() >= 2 && w[0] == "you" && w[1] == "know" {
            w.drain(..2);
        } else {
            return w;
        }
    }
}
/// Clauses (split at sentence ends and commas), each as words without lead-ins.
fn clauses(text: &str) -> Vec<Vec<String>> {
    text.replace('’', "'").split(['.', '!', '?', ';', ',', '\n']).map(|s| without_lead(words(s))).filter(|w| !w.is_empty()).collect()
}
/// Whether a sentence asks something. A trailing tag ("…, you know?", "…, right?", "okay?") does not.
fn asks(sentence: &str) -> bool {
    let w = without_lead(words(sentence));
    let Some(first) = w.first() else { return false };
    if ASK.contains(&first.as_str()) {
        return w.len() >= 3 || sentence.trim_end().ends_with('?');
    }
    if !sentence.trim_end().ends_with('?') {
        return false;
    }
    let n = w.len();
    let tag = (n >= 2 && w[n - 2] == "you" && w[n - 1] == "know")
        || ["right", "okay", "ok", "yeah", "no", "huh"].contains(&w[n - 1].as_str())
        || (n >= 2 && w[n - 2] == "got" && w[n - 1] == "it");
    !tag
}
fn has(words: &[String], phrase: &str) -> bool {
    let p: Vec<&str> = phrase.split(' ').collect();
    words.windows(p.len()).any(|w| w.iter().zip(&p).all(|(a, b)| a == b))
}
fn any(words: &[String], phrases: &[&str]) -> bool {
    phrases.iter().any(|p| has(words, p))
}

/// Verbs that open an order ("clean this up", "continue the work").
const ORDER: &[&str] = &[
    "open", "close", "build", "fix", "run", "install", "reinstall", "download", "delete", "remove", "add", "send", "start",
    "restart", "continue", "clean", "change", "move", "rename", "test", "check", "use", "switch", "schedule", "save",
    "show", "give", "write", "prepare", "make", "find", "search", "look", "research", "compare", "create", "update", "kill",
    "stop", "cancel", "abort", "pause", "put", "set", "turn", "try", "ask", "tell", "read", "play", "merge", "commit", "go",
    "do", "get", "copy", "take", "help", "explain", "list", "figure", "note", "remember", "keep", "drop", "record",
    "review", "measure", "retry", "launch", "navigate", "spawn", "deploy", "push", "pull", "finish", "draw", "generate",
];
/// Asking for something anywhere in the turn.
const REQUEST: &[&str] = &[
    "can you", "could you", "would you", "will you", "can we", "could we", "please", "let us", "i want you",
    "i need you", "i'd like you", "i would like", "go ahead", "how about we", "shall we", "you need to",
];
/// Telling him how things should be (a preference or rule, nothing to do now): the neutral line fits.
const DIRECTIVE: &[&str] = &[
    "should", "shouldn't", "need", "want", "don't", "do not", "never", "always", "stop", "chill", "from now on",
    "make sure", "go easy", "take it easy", "keep in mind", "calm down", "we're not", "we are not", "no more", "instead", "i told you",
];
const ASK: &[&str] = &[
    "what", "why", "how", "when", "where", "who", "which", "whats", "what's", "is", "are", "do", "does", "did", "can",
    "could", "should", "would", "will", "was", "were", "have", "has",
];

/// The turn, read once.
struct Turn {
    all: Vec<String>,
    question: bool,
    request: bool,
    stop: bool,
}
impl Turn {
    fn new(text: &str) -> Self {
        let all = words(text);
        let question = text.replace('’', "'").split_inclusive(['.', '!', '?', ';', '\n']).any(asks);
        // "Look, I'll…" and "Do you…" are not orders.
        const NOT_AFTER: &[&str] = &["you", "i", "i'll", "i'm", "we", "we're", "it's"];
        // A lone "Look," before his own words calls attention; it orders nothing. "Go easy on…", "keep in mind…"
        // are preferences in an order's shape.
        const EASY: &[&str] = &["go easy", "take it easy", "keep in mind", "chill out", "calm down", "make sure"];
        let clauses = clauses(text);
        let order = clauses.iter().any(|s| {
            ORDER.contains(&s[0].as_str())
                && s.get(1).map_or(s[0] != "look", |w| !NOT_AFTER.contains(&w.as_str()))
                && !any(s, EASY)
        });
        // A stop order opens its clause ("stop it", "okay, cancel the download"), never "don't stop" or "a pause".
        const STOP: &[&str] = &["stop", "cancel", "abort", "kill", "halt", "pause"];
        let stop = clauses.iter().any(|s| STOP.contains(&s[0].as_str()) && s.get(1).is_none_or(|w| !w.ends_with("ing")));
        // "Let's see" thinks aloud; it asks for nothing.
        let lets = all.windows(2).any(|w| w[0] == "let's" && w[1] != "see") || all.last().is_some_and(|w| w == "let's");
        let request = order || lets || any(&all, REQUEST);
        Turn { all, question, request, stop }
    }
    fn directive(&self) -> bool {
        any(&self.all, DIRECTIVE)
    }
}

/// The gate: whether the decider's line `id` fits the user's words `text`.
pub fn gate(id: &str, text: &str) -> Gated {
    let t = Turn::new(text);
    if any(&t.all, &["don't talk to me", "do not talk to me", "be quiet", "shut up", "stop talking"]) {
        return Gated::Skip("asked_for_quiet");
    }
    let greeting = any(&t.all, &["you there", "are you there", "you okay", "are you okay", "how are you", "you listening",
        "you're listening", "you are listening"])
        || (t.all.len() <= 4 && t.all.first().is_some_and(|w| ["hello", "hi", "hey"].contains(&w.as_str())));
    if greeting && !t.request && t.all.len() <= 10 {
        return Gated::Skip("greeting");
    }
    if t.all.len() <= 3 && !t.request {
        return Gated::Skip("too_short");
    }
    // He drops it ("never mind", "let's leave it"): nothing but a stop line or the neutral one fits.
    let dropped = any(&t.all, &["never mind", "nevermind", "forget it", "forget about it", "forget about the", "leave it"]);
    if dropped && !matches!(claim(id), Claim::Stop | Claim::Neutral) {
        return Gated::Neutral(NEUTRAL);
    }
    let fits = match claim(id) {
        Claim::Neutral => true,
        Claim::Stop => t.stop || any(&t.all, &["forget about it", "forget it", "forget about that", "never mind", "nevermind"]),
        Claim::Course => {
            any(&t.all, &["instead", "actually", "rather", "switch", "change", "forget", "scratch that", "not that", "no no"])
        }
        Claim::Worker => any(&t.all, &["worker", "workers", "agent", "agents", "subagent", "subagents", "delegate", "spawn"]),
        Claim::Look => {
            let himself = any(&t.all, &["myself", "i'll look", "i will look", "i'll check", "i will check", "i'm gonna take",
                "i am gonna take", "i'm going to take", "i am going to take", "i'm gonna look", "i'm going to look"]);
            let something = any(&t.all, &["look", "see", "screen", "this", "check", "show", "link", "image", "screenshot",
                "attached", "dropped"]);
            !himself && something && (t.request || t.question)
        }
        // An order or a request to act; a plain question is not one ("we can use it, right?").
        Claim::Do => t.request,
        Claim::Check => t.request || t.question,
    };
    if fits {
        Gated::Keep
    } else if t.request || t.question || t.directive() {
        Gated::Neutral(NEUTRAL)
    } else {
        Gated::Skip("no_request")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wakes from 22-26 Oct where the decider's line did not fit his words (paraphrased), and the gate's answer.
    #[test]
    fn mismatched_lines_from_real_wakes_are_neutral_or_silent() {
        let cases: &[(&str, &str, Gated)] = &[
            // The 26 Oct 15:14 case: a preference about Opus heard as Heimdall stopping.
            ("en_okay_stopping", "By the way, take it easy with Opus, okay? Not everything needs Opus.", Gated::Neutral(NEUTRAL)),
            ("en_okay_stopping", "Look, I'll record more Jarvis lines, okay?", Gated::Skip("no_request")),
            ("en_okay_stopping", "I closed the website already.", Gated::Skip("no_request")),
            ("en_will_do", "No, that one is useless, we stay with Parakeet. We can use it, right?", Gated::Skip("no_request")),
            ("en_let_me_think", "What do you mean, all the experts? Okay, okay, never mind.", Gated::Neutral(NEUTRAL)),
            ("en_let_me_think", "Only two options changed, you know. So let's see.", Gated::Skip("no_request")),
            ("en_let_me_think", "It just does all of that, it's really needed, you know?", Gated::Skip("no_request")),
            ("en_let_me_think", "If you won't do it, I'll switch you to another model.", Gated::Skip("no_request")),
            ("en_pulling_it_up", "Arsenal takes almost 500 gigabytes, all weights and snapshots. What is that?", Gated::Neutral(NEUTRAL)),
            ("en_pulling_it_up", "Would a higher resolution Blu-ray also give better sound quality?", Gated::Neutral(NEUTRAL)),
            ("en_okay", "I deleted the weights and the row stayed, the download looks so nice, I love it.", Gated::Skip("no_request")),
            ("en_okay", "Okay, cool.", Gated::Skip("too_short")),
            ("en_sure_thing", "Sure, that's quite.", Gated::Skip("too_short")),
            ("en_might_take_a_while", "No, no, much less. Two hundred is way too slow. Two stable clips is fine.", Gated::Skip("no_request")),
            ("en_let_me_take_a_look", "Okay, I'll check it myself. I'm going to take a look myself.", Gated::Skip("no_request")),
            ("en_okay_checking", "It is already free of that model.", Gated::Skip("no_request")),
            ("en_checking", "I think it was the singleton one.", Gated::Skip("no_request")),
            ("en_looking_it_up", "And I don't like how the order looks, it's a bit weird.", Gated::Neutral(NEUTRAL)),
            ("en_looking_into_it", "Model scope. I didn't finish talking, by the way.", Gated::Skip("no_request")),
            ("en_let_me_think", "Aww, that is so nice.", Gated::Skip("no_request")),
            ("en_let_me_think", "Hello.", Gated::Skip("greeting")),
            ("en_let_me_think", "Hi Heimdall, you there?", Gated::Skip("greeting")),
            ("en_let_me_think", "Hey Heimdall, how are you doing?", Gated::Skip("greeting")),
            ("en_let_me_think", "Okay, I'll be back in fifteen minutes, so don't talk to me for now.", Gated::Skip("asked_for_quiet")),
            ("en_let_me_think", "That is so incredibly loud.", Gated::Skip("no_request")),
            ("en_let_me_think", "Great.", Gated::Skip("too_short")),
            ("en_let_me_think", "Okay, we are back, that was a network problem.", Gated::Skip("no_request")),
            // Bug note 26 Oct 00:38: self-talk woke the agent and "Let me think." played each time.
            ("en_let_me_think", "So, the defaults.", Gated::Skip("too_short")),
            ("en_let_me_think", "Oh, you are listening. I forgot you were there.", Gated::Skip("greeting")),
            ("en_let_me_think", "And I will trim it by hand.", Gated::Skip("no_request")),
        ];
        for (id, text, want) in cases {
            assert_eq!(&gate(id, text), want, "{id}: {text}");
        }
    }

    /// Wakes where the line fit: the gate keeps them.
    #[test]
    fn fitting_lines_from_real_wakes_stay() {
        let cases: &[(&str, &str)] = &[
            ("en_okay_stopping", "Okay, you know what, forget about it. That job."),
            ("en_stopping_now", "Stop it now."),
            ("en_stopping_now", "Cancel the download, please."),
            ("en_will_do", "Clean this up and then ask me that question again."),
            ("en_will_do", "Let's download it and test it."),
            ("en_will_do", "Yeah, by the way, continue the tiny model work, we have space now."),
            ("en_on_it", "Can you open the website for me at the login page?"),
            ("en_got_it", "Okay, remember that for the release."),
            ("en_let_me_check", "Can you check my mailbox for that gym subscription?"),
            ("en_let_me_think", "How exactly do we build the setup wizard, on the islands or separately?"),
            ("en_let_me_take_a_look", "By the way, look at the screen, the find dialog still has that bug."),
            ("en_got_it_changing_course", "No, actually do the other one instead."),
            ("en_ill_get_a_worker_on_it", "Send a worker to look online for the best voice."),
            ("en_noted", "The quotas refresh at about two tomorrow."),
            ("en_let_me_think", "Speaking of that, just look at this and tell me which one has the best audio."),
            ("en_let_me_check", "Your weekly usage dropped by a fifth, so um, try to figure out where it went."),
        ];
        for (id, text) in cases {
            assert_eq!(gate(id, text), Gated::Keep, "{id}: {text}");
        }
    }

    #[test]
    fn stop_needs_an_order_not_a_preference() {
        assert_eq!(gate("en_stopping_now", "Stop using Opus for everything."), Gated::Neutral(NEUTRAL));
        assert_eq!(gate("en_stopping_now__urgent", "Kill that process now."), Gated::Keep);
        assert_eq!(gate("en_stopping_now", "Never mind, leave it."), Gated::Keep);
        assert_eq!(gate("en_stopping_now", "Okay, cancel the download."), Gated::Keep);
        // Review 26 Oct: "stop" said but not ordered.
        for text in ["Don't stop the download.", "The download didn't stop.", "There's a pause in the audio."] {
            assert_ne!(gate("en_stopping_now", text), Gated::Keep, "{text}");
        }
    }

    /// Review 26 Oct: a preference in an order's shape claims no action.
    #[test]
    fn preferences_are_not_requests() {
        for text in ["You should never use Opus for this.", "Go easy on Opus, not everything needs it.", "Make sure it stays small."] {
            assert_eq!(gate("en_on_it", text), Gated::Neutral(NEUTRAL), "{text}");
        }
        assert_eq!(gate("en_on_it", "Use Haiku for the drawing job."), Gated::Keep);
        assert_eq!(gate("en_stopping_now", "Hey, stop the download."), Gated::Keep, "an order after a greeting");
    }

    /// Not in the suite: the gate over a set of real wakes kept outside the repo (the user's words stay out of git).
    /// `ACK_GATE_EVAL=<json [[line id, words], …]> cargo test -p voicebank ack_gate -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn eval_on_a_set_of_real_wakes() {
        let path = std::env::var("ACK_GATE_EVAL").expect("ACK_GATE_EVAL");
        let set: Vec<(String, String)> = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        for (id, text) in set {
            println!("GATE\t{}", serde_json::json!(format!("{:?}", gate(&id, &text))));
        }
    }

    /// Every current line has a claim class, and the neutral line exists.
    #[test]
    fn the_neutral_line_is_a_current_ack() {
        assert!(acks::by_id(NEUTRAL).is_some_and(|a| !a.retired()));
        assert_eq!(claim("en_okay"), Claim::Do, "a retired id is judged by the line that plays");
        assert_eq!(claim("en_let_me_think"), Claim::Check);
        assert_eq!(claim("en_stopping_now__urgent"), Claim::Stop);
    }
}
