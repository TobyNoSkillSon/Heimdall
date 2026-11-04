//! Compiled grammars and the token masks they give the engine (server/constraints.py: ConstraintFactory,
//! TokenConstraint). The semantics of Python's `LLMatcher` binding (llguidance python_ext/src/llmatcher.rs, 1.8.0) are
//! kept here where they differ from the Rust `Matcher`: a stopped or failed matcher masks to EOS, a stopped one takes
//! EOS, and draft rows stop at the first token the grammar refuses.
use super::lru::LruCache;
use super::ApiError;
use crate::chat::format::ChatFormat;
use crate::engine::MaskProvider;
use llguidance::api::{GrammarInit, ParserLimits, TopLevelGrammar};
use llguidance::earley::SlicedBiasComputer;
use llguidance::toktrie::{InferenceCapabilities, SimpleVob, TokEnv};
use llguidance::{Logger, Matcher, ParserFactory};
use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// The output grammar refused a generated token or allows none next. The server answers 400 `constraint_error`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ConstraintError(pub String);

fn constraint_error(message: impl Into<String>) -> anyhow::Error {
    ConstraintError(message.into()).into()
}

/// A compiler panic carries a backtrace rather than a reason, and the lines after the first echo the grammar with
/// every schema it holds.
fn grammar_api_error(error: &str) -> ApiError {
    if error.starts_with("panic") {
        return ApiError::bad_request("tool or output schema is too large to compile");
    }
    ApiError::bad_request(format!("unsupported output schema: {}", first_line(error)))
}

/// `text.splitlines()[0]`: the text up to Python's first line boundary ("" for text that starts with one).
pub(crate) fn first_line(text: &str) -> &str {
    let boundary = |c: char| matches!(c, '\n' | '\r' | '\u{b}' | '\u{c}' | '\u{1c}' | '\u{1d}' | '\u{1e}' | '\u{85}' | '\u{2028}' | '\u{2029}');
    text.find(boundary).map_or(text, |end| &text[..end])
}

/// What the grammar cache holds, for the server's status.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct GrammarCacheStats {
    pub entries: usize,
    pub capacity: usize,
    pub source_bytes: usize,
    pub source_budget_bytes: usize,
    pub hits: u64,
    pub misses: u64,
}

/// A compilation another request is already running, shared by identical misses.
#[derive(Default)]
struct Pending {
    result: Mutex<Option<Result<Matcher, ApiError>>>,
    done: Condvar,
}

struct Cache {
    matchers: LruCache<String, Matcher>,
    pending: HashMap<String, Arc<Pending>>,
    hits: u64,
    misses: u64,
}

/// Compiles grammars against one model's tokenizer and hands out a fresh constraint per request, keeping compiled
/// matchers by grammar source.
pub struct ConstraintFactory {
    chat_format: ChatFormat,
    factory: ParserFactory,
    cache: Mutex<Cache>,
}

pub type Prefixes<'a> = &'a dyn Fn() -> Vec<(Vec<u32>, String)>;

impl ConstraintFactory {
    pub const CACHE_SIZE: usize = 32;
    pub const CACHE_SOURCE_BYTES: usize = 8 * 1024 * 1024;

    /// Builds llguidance's view of `tokenizer` padded to the model's vocabulary, the engine's stop tokens as its EOS.
    /// toktrie_hf_tokenizers links its own `tokenizers`, so the tokenizer crosses as JSON, as Python's binding does.
    pub fn new(tokenizer: &tokenizers::Tokenizer, chat_format: ChatFormat) -> anyhow::Result<Self> {
        let mut plain = tokenizer.clone();
        plain.with_padding(None);
        plain.with_truncation(None).map_err(|e| anyhow::anyhow!("{e}"))?;
        let json = plain.to_string(false).map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut bytes = toktrie_hf_tokenizers::ByteTokenizer::from_json_bytes(json.as_bytes())?;
        let stops = chat_format.stop_ids();
        let size = bytes.tokrx_info().vocab_size;
        anyhow::ensure!(stops.iter().all(|&t| t < size), "stop tokens lie outside the tokenizer's vocabulary");
        bytes.set_eos_tokens(&stops);
        let tok_env = bytes.into_tok_env(Some(chat_format.vocabulary as usize))?;
        let mut factory = ParserFactory::new(&tok_env, InferenceCapabilities::default(), &SlicedBiasComputer::json_slices())?;
        factory.quiet();
        let matchers = LruCache::new(Self::CACHE_SOURCE_BYTES, Some(Self::CACHE_SIZE));
        Ok(ConstraintFactory { chat_format, factory, cache: Mutex::new(Cache { matchers, pending: HashMap::new(), hits: 0, misses: 0 }) })
    }

    pub fn tok_env(&self) -> &TokEnv {
        self.factory.tok_env()
    }

    pub fn chat_format(&self) -> &ChatFormat {
        &self.chat_format
    }

    /// A constraint of `grammar` for one request. `prefixes`, called when the grammar is compiled, gives token runs
    /// the output must be able to begin with and the error for a grammar that cannot: a grammar can compile and still
    /// exceed the parser's limits where generation reaches a construct, after the whole prompt was processed.
    /// `timeout` bounds the wait for an identical compilation another request is running.
    pub fn create(&self, grammar: &str, timeout: Option<Duration>, prefixes: Option<Prefixes>) -> Result<TokenConstraint, ApiError> {
        let matcher = self.matcher(grammar, timeout, prefixes)?;
        Ok(TokenConstraint::new(matcher, self.factory.tok_env().clone(), &self.chat_format))
    }

    fn matcher(&self, grammar: &str, timeout: Option<Duration>, prefixes: Option<Prefixes>) -> Result<Matcher, ApiError> {
        let pending = {
            let mut cache = self.cache.lock().unwrap();
            if let Some(matcher) = cache.matchers.get(grammar).cloned() {
                cache.hits += 1;
                return Ok(matcher);
            }
            match cache.pending.get(grammar) {
                Some(pending) => Err(pending.clone()),
                None => {
                    let pending = Arc::new(Pending::default());
                    cache.pending.insert(grammar.to_string(), pending.clone());
                    Ok(pending)
                }
            }
        };
        let pending = match pending {
            Ok(owned) => owned,
            Err(shared) => return self.wait(grammar, &shared, timeout),
        };
        // A panic must still settle the shared compilation, or identical requests would wait on it forever.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.compile(grammar, prefixes)))
            .unwrap_or_else(|_| Err(grammar_api_error("panic")));
        let mut cache = self.cache.lock().unwrap();
        if let Ok(matcher) = &result {
            cache.misses += 1;
            // Oversized grammars stay usable without displacing the cache; llguidance bounds compiler complexity.
            cache.matchers.put(grammar.to_string(), matcher.clone(), grammar.len());
        }
        cache.pending.remove(grammar);
        *pending.result.lock().unwrap() = Some(result.clone());
        pending.done.notify_all();
        result
    }

    fn wait(&self, grammar: &str, pending: &Pending, timeout: Option<Duration>) -> Result<Matcher, ApiError> {
        let timed_out = || ApiError { status: 504, message: "request timed out".into(), code: "request_timeout" };
        let mut result = pending.result.lock().unwrap();
        match timeout {
            None => {
                while result.is_none() {
                    result = pending.done.wait(result).unwrap();
                }
            }
            Some(timeout) => {
                let (guard, waited) = pending.done.wait_timeout_while(result, timeout, |r| r.is_none()).unwrap();
                result = guard;
                if waited.timed_out() && result.is_none() {
                    return Err(timed_out());
                }
            }
        }
        let matcher = result.clone().expect("a finished compilation")?;
        drop(result);
        let mut cache = self.cache.lock().unwrap();
        // The shared matcher, if kept, is now the most recently used.
        cache.matchers.get(grammar);
        cache.hits += 1;
        Ok(matcher)
    }

    fn compile(&self, grammar: &str, prefixes: Option<Prefixes>) -> Result<Matcher, ApiError> {
        if let Some(error) = super::grammar::grammar_error(grammar, Some(self.factory.tok_env().clone())) {
            return Err(grammar_api_error(&error));
        }
        let parsed = TopLevelGrammar::from_lark_or_grammar_list(grammar).map_err(|e| grammar_api_error(&e.to_string()))?;
        let parser = self.factory.create_parser_from_init_ext(
            GrammarInit::Serialized(parsed),
            Logger::new(0, 0),
            InferenceCapabilities::default(),
            ParserLimits::default(),
        );
        let matcher = Matcher::new(parser);
        if let Some(error) = matcher.get_error() {
            return Err(grammar_api_error(&error));
        }
        for (tokens, message) in prefixes.map(|p| p()).unwrap_or_default() {
            if matcher.clone().consume_tokens(&tokens).is_err() {
                return Err(ApiError::bad_request(message));
            }
        }
        Ok(matcher)
    }

    pub fn stats(&self) -> GrammarCacheStats {
        let cache = self.cache.lock().unwrap();
        GrammarCacheStats {
            entries: cache.matchers.len(),
            capacity: Self::CACHE_SIZE,
            source_bytes: cache.matchers.bytes,
            source_budget_bytes: Self::CACHE_SOURCE_BYTES,
            hits: cache.hits,
            misses: cache.misses,
        }
    }
}

struct MaskState {
    matcher: Matcher,
    /// MAX_ROWS rows, all allowed at first (allocate_token_bitmask fills -1) and reused between mask requests as
    /// Python's preallocated bitmask is: a draft row the grammar stops before keeps what an earlier call wrote there.
    bitmask: Vec<u32>,
}

/// One request's grammar as the engine sees it.
pub struct TokenConstraint {
    tok_env: TokEnv,
    vocabulary: u32,
    words: usize,
    eos_tokens: Vec<u32>,
    /// Generated batches the grammar has not consumed yet. The reader thread commits them; the mask thread consumes
    /// them before the next mask, so reading the engine's stream never waits for the grammar.
    committed: Mutex<Vec<Vec<u32>>>,
    state: Mutex<MaskState>,
}

impl TokenConstraint {
    /// A mask request simulates at most the engine's target verify rows (the pending anchor and seven drafts) and
    /// takes a mask before and after each.
    pub const MAX_ROWS: usize = 9;

    fn new(matcher: Matcher, tok_env: TokEnv, chat_format: &ChatFormat) -> Self {
        let words = chat_format.vocabulary.div_ceil(32) as usize;
        TokenConstraint {
            tok_env,
            vocabulary: chat_format.vocabulary,
            words,
            eos_tokens: chat_format.stop_ids(),
            committed: Mutex::new(Vec::new()),
            state: Mutex::new(MaskState { matcher, bitmask: vec![u32::MAX; Self::MAX_ROWS * words] }),
        }
    }

    /// The words of one mask row.
    pub fn words_per_mask(&self) -> usize {
        self.words
    }

    fn consume_committed(&self, state: &mut MaskState) -> anyhow::Result<()> {
        let batches = std::mem::take(&mut *self.committed.lock().unwrap());
        let matcher = &mut state.matcher;
        for tokens in batches {
            // llguidance's bulk call refuses EOS after a NoExtension stop; its single-token one takes it.
            let stopped_eos = tokens.len() == 1
                && self.eos_tokens.contains(&tokens[0])
                && !matcher.is_error()
                && matcher.is_stopped()
                && matcher.is_accepting().unwrap_or(false);
            let valid = if stopped_eos { consume_token(matcher, &self.tok_env, tokens[0]) } else { matcher.consume_tokens(&tokens).is_ok() };
            if !valid {
                // The lines after the first dump the parser state, output included.
                let error = matcher.get_error().unwrap_or_default();
                return Err(constraint_error(if error.is_empty() { "invalid token" } else { first_line(&error) }));
            }
        }
        Ok(())
    }
}

/// `LLMatcher.consume_token`: a stopped matcher takes any EOS.
fn consume_token(matcher: &mut Matcher, env: &TokEnv, token: u32) -> bool {
    if matcher.is_stopped() && env.tok_trie().eos_tokens().contains(&token) {
        return true;
    }
    matcher.consume_token(token).is_ok()
}

/// `LLMatcher.compute_mask_or_eos` into `row`: EOS alone where the matcher is stopped or fails.
fn fill_row(matcher: &mut Matcher, env: &TokEnv, row: &mut [u32]) {
    let eos = || env.tok_trie().eos_token_set();
    let mask: SimpleVob = if matcher.is_stopped() { eos() } else { matcher.compute_mask().unwrap_or_else(|_| eos()) };
    row.copy_from_slice(&mask.as_slice()[..row.len()]);
}

/// `LLMatcher.validate_tokens`: how many of `tokens` the grammar takes in a row; on failure 1 where the first is
/// the primary EOS, else 0.
fn validate_tokens(matcher: &mut Matcher, tokens: &[u32], primary_eos: u32) -> usize {
    matcher.validate_tokens(tokens).unwrap_or_else(|_| usize::from(tokens.first() == Some(&primary_eos)))
}

impl MaskProvider for TokenConstraint {
    fn commit(&self, tokens: &[u32]) -> anyhow::Result<()> {
        if tokens.iter().any(|&t| t >= self.vocabulary) {
            return Err(constraint_error("generated token is out of range"));
        }
        self.committed.lock().unwrap().push(tokens.to_vec());
        Ok(())
    }

    fn masks(&self, simulation: &[u32], words_per_mask: u32) -> anyhow::Result<Vec<u32>> {
        let mut state = self.state.lock().unwrap();
        self.consume_committed(&mut state)?;
        if simulation.len() >= Self::MAX_ROWS {
            return Err(constraint_error("too many simulation tokens"));
        }
        if words_per_mask as usize != self.words {
            return Err(constraint_error(format!("mask rows of {words_per_mask} words; the vocabulary takes {}", self.words)));
        }
        let words = self.words;
        let in_range = simulation.iter().position(|&t| t >= self.vocabulary).unwrap_or(simulation.len());
        let MaskState { matcher, bitmask } = &mut *state;
        let primary_eos = self.tok_env.tok_trie().eos_token();
        let valid_count = validate_tokens(matcher, &simulation[..in_range], primary_eos);
        let valid = &simulation[..valid_count];
        // The mask before each valid token and after the last, consuming each in turn, then rolled back.
        let mut advanced = 0;
        for index in 0..=valid.len() {
            fill_row(matcher, &self.tok_env, &mut bitmask[index * words..(index + 1) * words]);
            if index == valid.len() || matcher.is_stopped() {
                break;
            }
            match matcher.try_consume_tokens(&valid[index..index + 1]) {
                Ok(consumed) if consumed > 0 => advanced += 1,
                _ => break,
            }
        }
        if advanced > 0 {
            let _ = matcher.rollback(advanced);
        }
        let rows = simulation.len() + 1;
        let valid_rows = valid_count + 1;
        for row in valid_rows..rows {
            bitmask.copy_within((valid_rows - 1) * words..valid_rows * words, row * words);
        }
        if bitmask[..valid_rows * words].chunks(words).any(|row| row.iter().all(|&w| w == 0)) {
            return Err(constraint_error("output grammar has no valid token"));
        }
        Ok(bitmask[..rows * words].to_vec())
    }

    fn finish(&self) -> anyhow::Result<()> {
        let mut state = self.state.lock().unwrap();
        self.consume_committed(&mut state)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn first_line_splits_as_python() {
        use super::first_line;
        assert_eq!(first_line("\nParser state"), "");
        assert_eq!(first_line("bad token\r\nstate"), "bad token");
        assert_eq!(first_line("a\u{2028}b"), "a");
        assert_eq!(first_line("one line"), "one line");
    }
}
