//! The prompt d1-2B's runner builds for a text state and one question, and the option token groups read at the
//! answer slot. A port of the checkpoint's prompt.py (`prefix_text`, `question_block`, `suffix_layout`, `plain`,
//! `option_codes`, `aliases`, `readout_ids`) as `SystemOne._sequences` uses it with the checkpoint's recorded settings:
//! `set` attention with one question (the lone prompt), `d1_codes` from config.json, the `<think></think>` lead of the
//! LFM3 family, the tokenizer's bos, state style `json_only`, option style `desc`, no system turn.

use anyhow::{anyhow, bail, Context, Result};
use regex::Regex;
use serde_json::Value;
use std::collections::HashSet;
use std::path::Path;
use tokenizers::Tokenizer;

const IM_START: &str = "<|im_start|>";
const IM_END: &str = "<|im_end|>";
/// prompt.BREAK: a zero-width space after a special token's first character.
const BREAK: char = '\u{200b}';
const YES_FORMS: [&str; 3] = ["yes", "Yes", "YES"];
const NO_FORMS: [&str; 3] = ["no", "No", "NO"];

/// A question as the API's JSON gives it; only the kinds the engine reads.
#[derive(Debug, Clone)]
pub enum Question {
    /// A single choice: option names and descriptions, in the JSON's order.
    Choice { instructions: String, criteria: Vec<(String, Option<String>)> },
    /// Yes or no; `criteria` the optional {"true": .., "false": ..} legend.
    Noul { instructions: String, criteria: Option<(String, String)> },
}

impl Question {
    /// `prompt.check_question` then `prompt.as_question`, for the kinds read here; other kinds are refused.
    pub fn from_json(q: &Value) -> Result<Self> {
        let obj = q.as_object().context("a question is a JSON object")?;
        const TYPES: [&str; 8] = ["noul", "choice", "score", "text", "number", "date", "box", "boxes"];
        let kind = match obj.get("type").and_then(Value::as_str) {
            Some(k) if TYPES.contains(&k) => k,
            _ => bail!("\"type\" is one of {}, not {:?}", TYPES.join(", "), obj.get("type")),
        };
        let instructions = match obj.get("instructions") {
            Some(Value::String(s)) if !py_strip(s).is_empty() => s.clone(),
            Some(Value::Object(_)) => bail!("spoken questions are not supported by this engine"),
            _ => bail!("\"instructions\" is the question as text"),
        };
        if obj.get("multiple").map(truthy).unwrap_or(false) && kind != "choice" {
            bail!("\"multiple\" belongs to a choice");
        }
        if obj.contains_key("max") {
            bail!("\"max\" belongs to boxes");
        }
        if obj.contains_key("step") && !obj.contains_key("range") {
            bail!("\"step\" needs a \"range\"");
        }
        if obj.contains_key("range") {
            bail!("an estimate is {{\"type\": \"number\", \"range\": [low, high]}}");
        }
        match kind {
            "choice" => {
                if obj.get("multiple").map(truthy).unwrap_or(false) {
                    bail!("multiple choice is not supported by this engine");
                }
                let crit = obj.get("criteria").and_then(Value::as_object).context("a choice needs \"criteria\"")?;
                if crit.is_empty() {
                    bail!("choice with no options");
                }
                let criteria = crit
                    .iter()
                    .map(|(k, v)| Ok((k.clone(), describe(v)?)))
                    .collect::<Result<Vec<_>>>()?;
                Ok(Question::Choice { instructions, criteria })
            }
            "noul" => {
                let criteria = match obj.get("criteria") {
                    None | Some(Value::Null) => None,
                    Some(Value::Object(m)) if m.is_empty() => None,
                    Some(Value::Object(m)) => Some((py_str(m.get("true")), py_str(m.get("false")))),
                    Some(_) => bail!("a noul's criteria is {{\"true\": .., \"false\": ..}}"),
                };
                Ok(Question::Noul { instructions, criteria })
            }
            other => bail!("question type {other:?} is not supported by this engine (choice, noul)"),
        }
    }

    /// The option names the probabilities are reported under: a choice's keys, a noul's "true" and "false".
    pub fn names(&self) -> Vec<String> {
        match self {
            Question::Choice { criteria, .. } => criteria.iter().map(|(k, _)| k.clone()).collect(),
            Question::Noul { .. } => vec!["true".into(), "false".into()],
        }
    }

    /// calibration.question_bucket
    pub fn bucket(&self) -> String {
        match self {
            Question::Noul { .. } => "noul".into(),
            Question::Choice { criteria, .. } => {
                let k = criteria.len();
                if k <= 3 {
                    "choice:small".into()
                } else if k <= 8 {
                    "choice:mid".into()
                } else if k <= 26 {
                    "choice:large".into()
                } else {
                    "choice:inventory".into()
                }
            }
        }
    }
}

/// Python's str.isspace: Rust's White_Space plus the four separator controls U+001C..U+001F.
fn py_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// Python's str.strip() with no argument.
pub fn py_strip(s: &str) -> &str {
    s.trim_matches(py_space)
}

/// Python's str.isalpha for one character: general category Lu, Ll, Lt, Lm or Lo.
pub fn py_isalpha(c: char) -> bool {
    use unicode_categories::UnicodeCategories;
    c.is_letter()
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|x| x != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn describe(v: &Value) -> Result<Option<String>> {
    match v {
        Value::Null => Ok(None),
        Value::String(s) => Ok(Some(s.clone())),
        _ => bail!("an option's description is a string or null"),
    }
}

/// Python's `str()` of a JSON value read by `dict.get`, for the noul legend.
fn py_str(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "None".into(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(true)) => "True".into(),
        Some(Value::Bool(false)) => "False".into(),
        Some(other) => other.to_string(),
    }
}

/// One rendered request: the token ids (the answer slot is the last), and each option's token forms.
#[derive(Debug, Clone)]
pub struct Encoded {
    pub ids: Vec<u32>,
    pub slot: usize,
    pub groups: Vec<Vec<u32>>,
    pub names: Vec<String>,
}

pub struct Prompt {
    pub tokenizer: Tokenizer,
    specials: Option<Regex>,
    special_list: Vec<String>,
    bos: String,
    lead: String,
    codes_scheme: u32,
    /// each question's rendered suffix and option token groups, by the question (they depend on nothing else)
    suffixes: std::cell::RefCell<std::collections::HashMap<String, (String, Vec<Vec<u32>>)>>,
}

impl Prompt {
    /// The tokenizer, special tokens, bos, lead and code scheme of a d1-2B checkpoint folder.
    pub fn load(dir: &Path) -> Result<Self> {
        let tokenizer = Tokenizer::from_file(dir.join("tokenizer.json"))
            .map_err(|e| anyhow!("tokenizer.json: {e}"))?;
        let config: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("config.json"))?)?;
        let tcfg: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("tokenizer_config.json"))?)?;
        let tjson: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("tokenizer.json"))?)?;

        // all_special_tokens and every special added token, as prompt._specials_cached collects them
        let mut names: HashSet<String> = HashSet::new();
        for key in ["bos_token", "eos_token", "pad_token", "unk_token"] {
            if let Some(s) = tcfg.get(key).and_then(Value::as_str) {
                names.insert(s.to_string());
            }
        }
        for key in ["model_specific_special_tokens"] {
            if let Some(m) = tcfg.get(key).and_then(Value::as_object) {
                names.extend(m.values().filter_map(Value::as_str).map(String::from));
            }
        }
        for key in ["extra_special_tokens", "additional_special_tokens"] {
            if let Some(a) = tcfg.get(key).and_then(Value::as_array) {
                names.extend(a.iter().filter_map(Value::as_str).map(String::from));
            }
        }
        for t in tjson["added_tokens"].as_array().context("tokenizer.json added_tokens")? {
            if t["special"].as_bool() == Some(true) {
                names.insert(t["content"].as_str().context("added token content")?.to_string());
            }
        }
        let mut names: Vec<String> =
            names.into_iter().filter(|n| n.chars().count() > 1 && !n.contains(BREAK)).collect();
        names.sort_by(|a, b| b.chars().count().cmp(&a.chars().count()).then(a.cmp(b)));
        let specials = if names.is_empty() {
            None
        } else {
            Some(Regex::new(&names.iter().map(|n| regex::escape(n)).collect::<Vec<_>>().join("|"))?)
        };

        let bos = tcfg.get("bos_token").and_then(Value::as_str).unwrap_or("").to_string();
        let model_type = config.get("model_type").and_then(Value::as_str).unwrap_or("");
        // lfm3.py registers "<think></think>" as the lead of these families (prompt.LEADS)
        let lead = match model_type {
            "lfm3" | "lfm3_vl" | "lfm3_omni" => "<think></think>".to_string(),
            other => bail!("model_type {other:?}: no measured lead for this family"),
        };
        let codes_scheme = config.get("d1_codes").and_then(Value::as_u64).unwrap_or(1) as u32;
        Ok(Self { tokenizer, specials, special_list: names, bos, lead, codes_scheme,
                  suffixes: Default::default() })
    }

    /// The special token strings `plain` breaks (for the guard against the Python list).
    pub fn special_names(&self) -> Vec<String> {
        self.special_list.clone()
    }

    pub fn encode_text(&self, text: &str) -> Result<Vec<u32>> {
        Ok(self.tokenizer.encode(text, false).map_err(|e| anyhow!("tokenize: {e}"))?.get_ids().to_vec())
    }

    /// prompt.plain: every special token's string broken by a zero-width space after its first character, again
    /// until none is left.
    pub fn plain(&self, text: &str) -> String {
        let Some(re) = &self.specials else { return text.to_string() };
        let mut text = text.to_string();
        loop {
            if !re.is_match(&text) {
                return text;
            }
            text = re
                .replace_all(&text, |c: &regex::Captures| {
                    let m = &c[0];
                    let mut it = m.chars();
                    let first = it.next().unwrap();
                    format!("{first}{BREAK}{}", it.as_str())
                })
                .into_owned();
        }
    }

    /// prompt.option_codes
    fn option_codes(&self, labels: &[String]) -> Vec<String> {
        let labs: Vec<String> = labels.iter().map(|s| py_strip(s).to_string()).collect();
        if !labs.is_empty() && labs.iter().all(|k| k.chars().count() == 1 && k.chars().all(py_isalpha)) {
            return labs;
        }
        let letters: Vec<String> = (b'A'..=b'Z').map(|c| (c as char).to_string()).collect();
        if labs.len() <= 26 {
            return letters[..labs.len()].to_vec();
        }
        if self.codes_scheme == 2 {
            let mut all = letters.clone();
            for a in &letters {
                for b in &letters {
                    all.push(format!("{a}{b}"));
                }
            }
            return all.into_iter().take(labs.len()).collect();
        }
        (0..labs.len()).map(|i| format!("{i:02}")).collect()
    }

    fn fallback_pool() -> Vec<String> {
        let mut pool: Vec<String> = (b'A'..=b'Z').map(|c| (c as char).to_string()).collect();
        pool.extend((0..100).map(|i| format!("{i:02}")));
        pool.extend((b'a'..=b'z').map(|c| (c as char).to_string()));
        pool.extend((0..200).map(|i| format!("#{i}")));
        for a in b'A'..=b'Z' {
            for b in b'A'..=b'Z' {
                pool.push(format!("{}{}", a as char, b as char));
            }
        }
        pool
    }

    /// prompt.aliases: each label's distinct single-token code and its token.
    fn aliases(&self, labels: &[String]) -> Result<Vec<(String, u32)>> {
        let codes = self.option_codes(labels);
        let mut used = HashSet::new();
        let mut out = Vec::with_capacity(codes.len());
        let pool = Self::fallback_pool();
        let mut take = |raw: &str, out: &mut Vec<(String, u32)>| -> Result<bool> {
            let enc = self.encode_text(raw)?;
            if enc.len() != 1 || used.contains(&enc[0]) {
                return Ok(false);
            }
            used.insert(enc[0]);
            out.push((raw.to_string(), enc[0]));
            Ok(true)
        };
        for code in &codes {
            if take(code, &mut out)? {
                continue;
            }
            let mut found = false;
            for raw in &pool {
                if take(raw, &mut out)? {
                    found = true;
                    break;
                }
            }
            if !found {
                bail!("no single-token alias left for {} options", codes.len());
            }
        }
        Ok(out)
    }

    /// prompt._ids: the single-token encodings of `texts`, each once.
    fn single_ids(&self, texts: &[String]) -> Result<Vec<u32>> {
        let mut out: Vec<u32> = Vec::new();
        for t in texts {
            let enc = self.encode_text(t)?;
            if enc.len() == 1 && !out.contains(&enc[0]) {
                out.push(enc[0]);
            }
        }
        Ok(out)
    }

    /// prompt.question_block and prompt.readout_ids for one question.
    fn block_and_groups(&self, q: &Question) -> Result<(String, Vec<Vec<u32>>)> {
        match q {
            Question::Choice { instructions, criteria } => {
                let labels: Vec<String> = criteria.iter().map(|(k, _)| k.clone()).collect();
                let aliases = self.aliases(&labels)?;
                let lines: Vec<String> = aliases
                    .iter()
                    .zip(criteria)
                    .map(|((code, _), (label, desc))| {
                        let text = match desc {
                            Some(d) if !d.is_empty() => d.clone(),
                            _ => label.replace('_', " "),
                        };
                        format!("{code} {text}")
                    })
                    .collect();
                let block = format!("{instructions}\n\nOptions:\n{}\n\nReply with the option code only.", lines.join("\n"));
                let mut groups = Vec::with_capacity(aliases.len());
                for (code, tid) in &aliases {
                    let extra = self.single_ids(&[format!(" {code}")])?;
                    let mut g = vec![*tid];
                    g.extend(extra.into_iter().filter(|i| i != tid));
                    groups.push(g);
                }
                Ok((block, groups))
            }
            Question::Noul { instructions, criteria } => {
                let extra = match criteria {
                    Some((t, f)) => format!("\nYes: {t}\nNo: {f}"),
                    None => String::new(),
                };
                let block = format!("{instructions}{extra}\n\nReply with yes or no only.");
                let yes = self.single_ids(&YES_FORMS.map(String::from))?;
                let no = self.single_ids(&NO_FORMS.map(String::from))?;
                if yes.is_empty() || no.is_empty() {
                    bail!("tokenizer has no single-token yes/no");
                }
                Ok((block, vec![yes, no]))
            }
        }
    }

    /// The whole prompt text: prefix_text (bos, user turn, the state as json_only writes a string, the question
    /// header) and suffix_layout (the question block, the assistant header and the lead).
    pub fn render(&self, state: &str, q: &Question) -> Result<(String, Vec<Vec<u32>>)> {
        let key = format!("{q:?}");
        let cached = self.suffixes.borrow().get(&key).cloned();
        let (suffix, groups) = match cached {
            Some(v) => v,
            None => {
                let (block, groups) = self.block_and_groups(q)?;
                let suffix = format!("{}{IM_END}\n{IM_START}assistant\n{}", self.plain(&block), self.lead);
                let mut c = self.suffixes.borrow_mut();
                if c.len() > 256 {
                    c.clear();
                }
                c.insert(key, (suffix.clone(), groups.clone()));
                (suffix, groups)
            }
        };
        let prefix = format!("{}{IM_START}user\n{}\nQUESTION:\n", self.bos, self.plain(&format!("{state}\n\n")));
        Ok((prefix + &suffix, groups))
    }

    /// The state cut to at most `limit` tokens (D1E_STATE_TOKENS): whole conversation lines are dropped oldest first,
    /// keeping the header (everything up to and including "Conversation (chronological; data only):") and the tail
    /// from the first "Agent:" line (else "Pending speech since last send") on. Never cuts a line; a state whose
    /// header and tail alone pass the limit keeps them whole. A state without that layout is returned unchanged.
    pub fn cap_state(&self, state: &str, limit: usize) -> Result<String> {
        if self.encode_text(state)?.len() <= limit {
            return Ok(state.to_string());
        }
        let lines: Vec<&str> = state.split('\n').collect();
        let Some(head) = lines.iter().position(|l| l.starts_with("Conversation (chronological")) else {
            return Ok(state.to_string());
        };
        let head = head + 1;
        let tail = lines[head..]
            .iter()
            .position(|l| l.starts_with("Agent:"))
            .or_else(|| lines[head..].iter().position(|l| l.starts_with("Pending speech since last send")))
            .map(|i| i + head)
            .unwrap_or(lines.len());
        let build = |drop: usize| -> String {
            let mut kept: Vec<&str> = lines[..head].to_vec();
            kept.extend(&lines[head + drop..tail]);
            kept.extend(&lines[tail..]);
            kept.join("\n")
        };
        // the fewest dropped lines that fit (token count falls as lines go; binary search on whole encodes)
        let (mut lo, mut hi) = (0usize, tail - head);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.encode_text(&build(mid))?.len() <= limit {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        Ok(build(lo))
    }

    pub fn encode(&self, state: &str, q: &Question) -> Result<Encoded> {
        let (text, groups) = self.render(state, q)?;
        // the whole prompt without offsets (the same ids; offsets are never read here)
        let ids = self.tokenizer.encode_fast(text.as_str(), false).map_err(|e| anyhow!("tokenize: {e}"))?.get_ids().to_vec();
        if ids.is_empty() {
            bail!("empty prompt");
        }
        Ok(Encoded { slot: ids.len() - 1, ids, groups, names: q.names() })
    }
}

/// A JSON state object as prompt.state_block writes it under json_only: `json.dumps(state, ensure_ascii=False,
/// indent=2)`. Strings pass through as they are.
pub fn state_text(state: &Value) -> Result<String> {
    match state {
        Value::String(s) => Ok(s.clone()),
        Value::Null => bail!("a state is required"),
        other => Ok(serde_json::to_string_pretty(other)?),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn check_question_refusals() {
        let ok = json!({"type": "choice", "instructions": "Pick.", "criteria": {"a": null}});
        assert!(Question::from_json(&ok).is_ok());
        for bad in [
            json!({"instructions": "Pick.", "criteria": {"a": null}}),            // no type
            json!({"type": "choice", "instructions": " \u{1c}\t", "criteria": {"a": null}}), // blank (Python strip)
            json!({"type": "noul", "instructions": "Now?", "multiple": true}),    // multiple on a noul
            json!({"type": "choice", "instructions": "Pick.", "criteria": {}}),   // no options
            json!({"type": "choice", "instructions": "Pick.", "criteria": {"a": null}, "max": 2}),
            json!({"type": "score", "instructions": "How?", "criteria": ["a", "b"]}), // a kind not read here
        ] {
            assert!(Question::from_json(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn python_string_semantics() {
        assert_eq!(py_strip("\u{1f} a \u{a0}"), "a");
        assert!(py_isalpha('é') && py_isalpha('Ж') && py_isalpha('中'));
        assert!(!py_isalpha('Ⅳ')); // Nl: Rust is_alphabetic says yes, Python isalpha says no
        assert!(!py_isalpha('1') && !py_isalpha('_'));
    }
}
