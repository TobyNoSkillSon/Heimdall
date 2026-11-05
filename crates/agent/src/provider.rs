//! API inference providers: the catalog, added-model metadata, the agent's selection and credentials.
//! Keys never enter settings, logs, Pi's files, Pi's environment or its arguments: the installed app keeps them in the
//! login Keychain and Pi's key command reads them per request; lab binaries use an environment variable or memory.
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub const OPENCODE_GO: &str = "opencode-go";
pub const ANTHROPIC: &str = "anthropic";
/// Custom provider, OpenAI-compatible: any server that speaks OpenAI chat completions at a URL the user names (base
/// URL, optional key, its `/models`). Local servers and hosted fast inference alike.
pub const CUSTOM: &str = "openai-compatible";
/// Custom provider, Anthropic-compatible: any server that speaks the Anthropic Messages API at a URL the user names
/// (base URL without `/v1`, optional key, its `/v1/models`).
pub const CUSTOM_ANTHROPIC: &str = "anthropic-compatible";
/// Retired providers that saved settings and the Keychain may still name. OpenRouter is added as a custom provider
/// now; Own Engine (any OpenAI-compatible URL, key optional) is the OpenAI-compatible custom provider.
pub const RETIRED_OPENROUTER: &str = "openrouter";
pub const RETIRED_OWN_ENGINE: &str = "own-engine";
/// Why the agent does not start while no model is chosen (`AgentModel::Unchosen`).
pub const NO_MODEL_CHOSEN: &str = "Add a provider";
/// [providers] Sign in with ChatGPT (`chatgpt`): Pi's `openai` provider on api.openai.com, run on the ChatGPT plan.
pub const CHATGPT: &str = "openai";
pub const HAIKU: &str = "claude-haiku-5-5";
pub const OPUS: &str = "claude-opus-5-5";
pub const SONNET: &str = "claude-sonnet-5-5";

/// How a provider takes its key on the model list (Pi sends inference keys itself).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyHeader {
    Bearer,
    /// Anthropic: `x-api-key` with `anthropic-version`.
    XApiKey,
}

/// A provider Heimdall can authenticate with. The menu and picker are data-driven.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProviderInfo {
    /// Pi's provider id (Pi's built-in provider supplies the request formats and headers).
    pub id: &'static str,
    pub name: &'static str,
    /// `crates/ui/assets/providers/<icon>.png`.
    pub icon: &'static str,
    /// Lists the models the account can use; with a key it also verifies the key (401 when rejected).
    pub models_url: &'static str,
    pub key_header: KeyHeader,
    /// Whether the model list answers without a key (OpenCode's does; Anthropic's needs one).
    pub public_models: bool,
    /// Where the key lives: sign-in, subscription and API keys.
    pub console_url: &'static str,
    /// The provider's browser sign-in (OpenCode's device login). Without one, Connect opens `console_url` and the
    /// key panel.
    pub sign_in: bool,
    /// The environment handoff to Pi (and the only key source for lab binaries).
    pub key_env: &'static str,
    pub keychain_service: &'static str,
    /// A public catalog with endpoint family and limits for models neither Pi's catalog nor the model list describes.
    pub metadata_url: Option<&'static str>,
    /// The endpoints for models Pi's pinned catalog lacks, per request format.
    pub anthropic_base: Option<&'static str>,
    pub openai_base: Option<&'static str>,
    /// What a 401/403 at connect time means, after "<name>: ".
    pub rejected: &'static str,
    /// Selected as the agent's model when the provider is connected (on Anthropic: "we definitely will
    /// use it").
    pub connect_default: Option<&'static str>,
    /// Anthropic's one-hour prompt cache: Pi asks for it, spend counts its writes at 2× input, and the extension
    /// compacts a long history whose cache has expired before the next message.
    pub hour_cache: bool,
    /// Keys not scoped to a workspace (Anthropic's `sk-ant-usr-…`) must name one on every request: the organisation's
    /// workspace list, the header that names it, and the environment variable lab binaries take it from.
    pub workspaces: Option<Workspaces>,
    /// The endpoint is the user's: its base URL is kept beside the key, the model list is `<base>/models`, and every added
    /// model carries the base for Pi (`ApiModel::base_url`).
    pub custom_url: bool,
    /// Whether Connect needs a key (a local engine usually takes none).
    pub key_required: bool,
    /// [providers] A plan the user signs in to (ChatGPT, OpenCode Go): listed first.
    pub subscription: bool,
}

/// Anthropic's workspace selection for keys that are not scoped to one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Workspaces {
    pub list_url: &'static str,
    pub header: &'static str,
    pub env: &'static str,
}

/// The Keychain service of a provider's items: "Heimdall API provider: <name>". The public edition has its own
/// names ("Heimdall Alpha API provider: <name>"), so it can never read the personal app's keys on the same Mac. The
/// personal names never change: existing items keep working.
macro_rules! keychain_service {
    ($name:literal) => {
        if cfg!(feature = "public") { concat!("Heimdall Alpha API provider: ", $name) } else { concat!("Heimdall API provider: ", $name) }
    };
}

pub const PROVIDERS: &[ProviderInfo] = &[
    // [providers] Sign in with ChatGPT (chatgpt.rs): OpenAI's token sharing for locally hosted apps, the plan's usage.
    ProviderInfo {
        id: CHATGPT,
        name: "ChatGPT",
        icon: "codex",
        // OpenAI's list for this flow (`{"models":[{slug, display_name, visibility}]}`, chatgpt::listing).
        models_url: "https://api.openai.com/v1/models",
        key_header: KeyHeader::Bearer,
        public_models: false,
        console_url: crate::chatgpt::USAGE_URL,
        sign_in: true,
        // Lab binaries: a signed-in access token (the installed app keeps the session in the Keychain).
        key_env: "HEIMDALL_CHATGPT_ACCESS_TOKEN",
        keychain_service: keychain_service!("ChatGPT"),
        metadata_url: None,
        anthropic_base: None,
        openai_base: Some(crate::chatgpt::API_BASE),
        rejected: "sign in again",
        connect_default: Some(crate::chatgpt::DEFAULT_MODEL),
        hour_cache: false,
        workspaces: None,
        custom_url: false,
        key_required: false,
        subscription: true,
    },
    ProviderInfo {
        id: OPENCODE_GO,
        name: "OpenCode Go",
        icon: "opencode",
        models_url: "https://opencode.ai/inference/go/v1/models",
        key_header: KeyHeader::Bearer,
        public_models: true,
        console_url: "https://opencode.ai/auth",
        sign_in: true,
        key_env: "HEIMDALL_OPENCODE_API_KEY",
        keychain_service: keychain_service!("OpenCode Go"),
        metadata_url: Some("https://models.dev/api.json"),
        anthropic_base: Some("https://opencode.ai/inference/go/anthropic"),
        openai_base: Some("https://opencode.ai/inference/go/openai/v1"),
        rejected: "access rejected / no subscription",
        // The first model a sign-in gives an agent with none (Setup's OpenCode Go tile): Claude Haiku 5.5, the personal
        // edition's default agent model on this provider (fast, cheap per request).
        connect_default: Some(HAIKU),
        hour_cache: false,
        workspaces: None,
        custom_url: false,
        key_required: true,
        subscription: true,
    },
    ProviderInfo {
        id: ANTHROPIC,
        name: "Anthropic",
        icon: "anthropic",
        // One page holds every model (the list paginates at 20 by default).
        models_url: "https://api.anthropic.com/v1/models?limit=1000",
        key_header: KeyHeader::XApiKey,
        public_models: false,
        console_url: "https://platform.claude.com/settings/keys",
        sign_in: false,
        key_env: "HEIMDALL_ANTHROPIC_API_KEY",
        keychain_service: keychain_service!("Anthropic"),
        metadata_url: None,
        anthropic_base: Some("https://api.anthropic.com"),
        openai_base: None,
        rejected: "key rejected",
        connect_default: Some(OPUS),
        hour_cache: true,
        workspaces: Some(Workspaces {
            list_url: "https://api.anthropic.com/v1/organizations/workspaces?include_default=true",
            header: "anthropic-workspace-id",
            env: "HEIMDALL_ANTHROPIC_WORKSPACE_ID",
        }),
        custom_url: false,
        key_required: true,
        subscription: false,
    },
    ProviderInfo {
        id: CUSTOM,
        name: "OpenAI-Compatible",
        icon: "custom",
        models_url: "",
        key_header: KeyHeader::Bearer,
        public_models: false,
        console_url: "",
        sign_in: false,
        key_env: "HEIMDALL_CUSTOM_API_KEY",
        keychain_service: keychain_service!("OpenAI-Compatible"),
        metadata_url: None,
        anthropic_base: None,
        openai_base: None,
        rejected: "key rejected",
        connect_default: None,
        hour_cache: false,
        workspaces: None,
        custom_url: true,
        key_required: false,
        subscription: false,
    },
    ProviderInfo {
        id: CUSTOM_ANTHROPIC,
        name: "Anthropic-Compatible",
        icon: "custom",
        models_url: "",
        key_header: KeyHeader::XApiKey,
        public_models: false,
        console_url: "",
        sign_in: false,
        key_env: "HEIMDALL_CUSTOM_ANTHROPIC_API_KEY",
        keychain_service: keychain_service!("Anthropic-Compatible"),
        metadata_url: None,
        anthropic_base: None,
        openai_base: None,
        rejected: "key rejected",
        connect_default: None,
        hour_cache: false,
        workspaces: None,
        custom_url: true,
        key_required: false,
        subscription: false,
    },
];

/// Own Engine as it was, read only to carry its saved URL and key over to the OpenAI-compatible custom provider
/// (`adopt_retired_endpoint`).
const RETIRED_OWN_ENGINE_INFO: ProviderInfo = ProviderInfo {
    id: RETIRED_OWN_ENGINE,
    name: "Own Engine",
    icon: "custom",
    models_url: "",
    key_header: KeyHeader::Bearer,
    public_models: false,
    console_url: "",
    sign_in: false,
    key_env: "HEIMDALL_OWN_ENGINE_API_KEY",
    keychain_service: keychain_service!("Own Engine"),
    metadata_url: None,
    anthropic_base: None,
    openai_base: None,
    rejected: "key rejected",
    connect_default: None,
    hour_cache: false,
    workspaces: None,
    custom_url: true,
    key_required: false,
    subscription: false,
};
/// The key a keyless local engine is connected with (sent as `Bearer local`, which local servers ignore).
pub const NO_KEY: &str = "local";

pub fn provider(id: &str) -> Option<&'static ProviderInfo> {
    PROVIDERS.iter().find(|p| p.id == id)
}

/// `anthropic-version` for Anthropic's REST endpoints.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// The request format a model is served with, in Pi's `api` names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Api {
    AnthropicMessages,
    OpenaiCompletions,
    OpenaiResponses,
    /// [providers] ChatGPT's Codex backend (Pi's `openai-codex` provider, signed in with a ChatGPT plan).
    OpenaiCodexResponses,
    Google,
    Unknown,
}
impl Api {
    pub fn pi(self) -> Option<&'static str> {
        match self {
            Self::AnthropicMessages => Some("anthropic-messages"),
            Self::OpenaiCompletions => Some("openai-completions"),
            Self::OpenaiResponses => Some("openai-responses"),
            Self::OpenaiCodexResponses => Some("openai-codex-responses"),
            // Pi 1.2.0's OpenCode Go provider implements the three formats above only.
            Self::Google | Self::Unknown => None,
        }
    }
    pub fn from_pi(api: &str) -> Self {
        match api {
            "anthropic-messages" => Self::AnthropicMessages,
            "openai-completions" => Self::OpenaiCompletions,
            "openai-responses" => Self::OpenaiResponses,
            "openai-codex-responses" => Self::OpenaiCodexResponses,
            a if a.starts_with("google") => Self::Google,
            _ => Self::Unknown,
        }
    }
    /// models.dev names the AI SDK package; OpenCode's endpoint table pairs each package with one endpoint.
    fn from_npm(npm: &str) -> Self {
        match npm {
            "@ai-sdk/anthropic" => Self::AnthropicMessages,
            "@ai-sdk/openai" => Self::OpenaiResponses,
            "@ai-sdk/openai-compatible" => Self::OpenaiCompletions,
            "@ai-sdk/google" => Self::Google,
            _ => Self::Unknown,
        }
    }
}

impl ProviderInfo {
    /// The provider's documented endpoint for a model Pi's catalog does not already describe.
    fn base_url(&self, api: Api) -> Option<&'static str> {
        match api {
            Api::AnthropicMessages => self.anthropic_base,
            Api::OpenaiCompletions | Api::OpenaiResponses | Api::OpenaiCodexResponses => self.openai_base,
            Api::Google | Api::Unknown => None,
        }
    }
}

/// The smallest request budget the context slider offers ("minimum probably 128k … anything less
/// than 128k … seems a little bit dumb").
pub const MIN_CONTEXT: u64 = 128_000;
/// The slider's stops are evenly spaced; at most this many intervals between 128K and the model's window.
const MAX_CONTEXT_STEPS: u64 = 8;

/// An added model, with the metadata captured when it was added so Pi can be configured offline.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApiModel {
    pub provider: String,
    pub id: String,
    pub name: String,
    pub api: Api,
    /// The model's own window and maximum output tokens (0 when unknown).
    pub context: u64,
    pub max_output: u64,
    #[serde(default)]
    pub images: bool,
    #[serde(default)]
    pub reasoning: bool,
    /// Per million tokens, in Pi's shape (`input`, `output`, `cacheRead`, `cacheWrite`, optional `tiers`): Pi's
    /// definition for models its catalog lacks, and the spend estimate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<Value>,
    /// The context slider's choice: requests stay inside this many tokens (history compacts before). `None` is the
    /// model's whole window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_budget: Option<u64>,
    /// The thinking selector's choice, in Pi's level names (`thinking_levels`). `None` is `DEFAULT_THINKING`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    /// [providers] The endpoint of a model on the user's own URL (OpenAI-compatible, own engine), captured when added.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// The reasoning efforts the provider's own model list names for this model, least first, in the provider's words
    /// (ChatGPT's `supported_reasoning_levels`: "low" … "max", "ultra"). Empty when the list names none: Pi's catalog
    /// decides then.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub efforts: Vec<String>,
}

/// The thinking levels the menu offers, least first, in Pi's names. Pi's "minimal" (Pi sends it as low to Claude) and
/// "xhigh" are not offered.
pub const THINKING_ORDER: [&str; 5] = ["off", "low", "medium", "high", "max"];
/// Every model's thinking until the user picks another ("think less and just answer right away").
pub const DEFAULT_THINKING: &str = "low";
/// The local engine's levels: Pi maps off to `reasoning_effort: none` and the rest to the same effort names.
pub const LOCAL_THINKING: [&str; 4] = ["off", "low", "medium", "high"];

/// The level the model runs for a choice: the choice when the model takes it, else the nearest level above it, else
/// below (Pi's own clamp, over the offered levels).
pub fn clamp_thinking(levels: &[&'static str], wanted: &str) -> &'static str {
    if let Some(l) = levels.iter().find(|l| **l == wanted) { return l; }
    let at = THINKING_ORDER.iter().position(|l| *l == wanted).unwrap_or(1);
    THINKING_ORDER[at..].iter().chain(THINKING_ORDER[..at].iter().rev()).find_map(|l| levels.iter().find(|x| *x == l).copied()).unwrap_or("off")
}

/// Heimdall compacts the history once it fills this share of the request budget ("auto-compact at
/// about 85% of the context window"; it fired at 75% before: 190,617 of 252,571 tokens on 25 Oct 23:54).
pub const COMPACT_AT_PERCENT: u64 = 85;
/// Pi's own check, which compacts inside a turn (in front of a request), fires at this share of the budget at the
/// latest: a long busy stretch, when idle compaction never finds a quiet moment, cannot run the history to 94-97% of
/// the budget (audit 27 Oct, F5; Codex compacts at 90% of its window by default).
pub const IN_TURN_PERCENT: u64 = 90;

/// Heimdall's request budget for a model and the reserves derived from it, in tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The budget: requests stay inside it (history compacts at `compact_at`, Pi's own check at `threshold`).
    pub context: u64,
    /// The most one answer may take: Pi's `maxTokens`. The model's own output limit when it is known (Pi trims it per
    /// request to what the window leaves, `clampMaxTokensToContext`), else the reserve (audit 27 Oct, F7: it was the
    /// reserve, 16,384, on every API model, against a real 128,000 on Claude 5.5).
    pub response: u64,
    /// The answer's room the compaction threshold keeps free: an eighth of the budget, 2,048 to 16,384, never more than
    /// the model's output limit. It sets only when history compacts, never how long an answer may be.
    pub reserve: u64,
    pub recent: u64,
    /// The window Pi is told. It is the model's real window, so a summary of a history longer than the budget
    /// (a long local conversation switched to an API model) still fits one request; Pi clamps output to this window.
    pub window: u64,
    /// Pi's in-turn threshold for a history carried over from another model that is already past this budget's
    /// (`carrying`); 0 when there is none.
    pub carry: u64,
}
impl Limits {
    /// One formula for local and API models: Heimdall compacts while idle at `compact_at` (85%); Pi's own check,
    /// which compacts inside a turn, waits until the history leaves only the reserve's room, and 90% at most
    /// (`threshold`).
    pub fn for_context(context: u64, max_output: Option<u64>) -> Self {
        let known = max_output.filter(|m| *m > 0);
        let reserve = (context / 8).clamp(2048, 16384).min(known.unwrap_or(u64::MAX));
        Self {
            context,
            response: known.unwrap_or(reserve),
            reserve,
            recent: (context / 4).min(20000),
            window: context,
            carry: 0,
        }
    }
    /// The budget's compaction threshold: past it Pi compacts before its next request, also inside a turn.
    pub fn threshold(self) -> u64 {
        (self.context - self.reserve).min(self.context * IN_TURN_PERCENT / 100)
    }
    /// Where Heimdall compacts while idle (`maintenance`): `COMPACT_AT_PERCENT` of the budget, never past Pi's own.
    pub fn compact_at(self) -> u64 {
        (self.context * COMPACT_AT_PERCENT / 100).min(self.threshold())
    }
    /// The threshold Pi runs with: the budget's, or a carried history's (`carrying`).
    pub fn pi_threshold(self) -> u64 {
        self.threshold().max(self.carry)
    }
    /// Pi's `reserveTokens` for `window`: `window` less the threshold Pi runs with.
    pub fn pi_reserve(self) -> u64 {
        self.window - self.pi_threshold()
    }
    /// A model switch to a smaller budget (audit 27 Oct, F6: Opus or Sol at 234K switched to a 128K budget compacted
    /// in front of the next request, 16 s after the click). When the conversation's `history` (its last request, in
    /// tokens) is past this budget's threshold but still fits the model's real window with the reserve's room, Pi's
    /// own in-turn check moves up to the history plus the budget's usual room, so no request waits on a compaction;
    /// Heimdall's idle compaction (`compact_at`, unchanged) summarises it at the first quiet moment, as every
    /// compaction Heimdall starts. A history that fits the budget, or not even the window, keeps the budget's threshold.
    pub fn carrying(self, history: Option<u64>) -> Self {
        let Some(history) = history else { return self };
        let ceiling = self.window.saturating_sub(self.reserve);
        if history <= self.threshold() || history >= ceiling { return self; }
        Self { carry: (history + (self.context - self.threshold())).min(ceiling), ..self }
    }
}

/// Token counts of one provider response, as Pi reports them.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    /// All cache writes; `cache_write_1h` of them were one-hour writes (Anthropic reports the split).
    pub cache_write: u64,
    pub cache_write_1h: u64,
}
impl Usage {
    /// Pi's `usage` object (an assistant message's, or a compaction result's).
    pub fn from_pi(v: &Value) -> Option<Self> {
        let n = |k: &str| v[k].as_u64().unwrap_or(0);
        let u = Self { input: n("input"), output: n("output"), cache_read: n("cacheRead"), cache_write: n("cacheWrite"), cache_write_1h: n("cacheWrite1h").min(n("cacheWrite")) };
        (u != Self::default()).then_some(u)
    }
    pub fn prompt(&self) -> u64 {
        self.input + self.cache_read + self.cache_write
    }
}

impl ApiModel {
    /// Claude Haiku 5.5 on OpenCode Go, also used offline before any catalog fetch. Figures from OpenCode's Go page
    /// and Pi 1.2.0's catalog (18 Oct 2026): 1M window, 128K output, $0.10/$0.50 per M up to 100K input tokens.
    pub fn haiku() -> Self {
        Self { provider: OPENCODE_GO.into(), ..Self::anthropic(HAIKU).unwrap() }
    }
    /// The Claude models pre-added for Anthropic, as Pi 1.2.0's catalog and Anthropic's price list (claude-api
    /// reference, 6 Oct 2026) describe them: 1M window, 128K output; per million tokens input/output/cache read,
    /// with the five-minute cache write at 1.25× input.
    pub fn anthropic(id: &str) -> Option<Self> {
        let (name, input, output, read, tier) = match id {
            OPUS => ("Claude Opus 5.5", 4.0, 20.0, 0.20, None),
            SONNET => ("Claude Sonnet 5.5", 2.0, 10.0, 0.20, None),
            // Haiku's price rises fivefold above 100K input tokens per request.
            HAIKU => ("Claude Haiku 5.5", 0.10, 0.50, 0.01, Some(json!([{"inputTokensAbove":100000,"input":0.5,"output":2.5,"cacheRead":0.05,"cacheWrite":0.625}]))),
            _ => return None,
        };
        let mut cost = json!({"input":input,"output":output,"cacheRead":read,"cacheWrite":input * 1.25});
        if let Some(tiers) = tier { cost["tiers"] = tiers; }
        Some(Self {
            provider: ANTHROPIC.into(),
            id: id.into(),
            name: name.into(),
            api: Api::AnthropicMessages,
            context: 1_000_000,
            max_output: 128_000,
            images: true,
            reasoning: true,
            cost: Some(cost),
            context_budget: None,
            thinking: None,
            base_url: None,
            efforts: vec![],
        })
    }
    pub fn key(&self) -> (&str, &str) {
        (&self.provider, &self.id)
    }
    pub fn provider_info(&self) -> Option<&'static ProviderInfo> {
        provider(&self.provider)
    }
    /// Why this model cannot run in Heimdall at all, whatever the credentials.
    pub fn unsupported(&self) -> Option<String> {
        if self.provider_info().is_none() {
            return Some("Provider not supported".into());
        }
        match self.api {
            Api::Google => Some("Google endpoint not supported by Pi".into()),
            Api::Unknown => Some("Endpoint unknown".into()),
            _ if self.context < 8192 => Some("Context limit unknown".into()),
            _ => None,
        }
    }
    /// The context slider's stops: evenly spaced from 128K to the model's window (one stop when the window is smaller),
    /// plus the user's saved budget when it falls between them (a window that grew: Sol's 272K inside 500K stays 272K).
    pub fn context_steps(&self) -> Vec<u64> {
        self.steps(true)
    }
    /// A slider value the user moved to: the nearest of the stops the slider shows, his saved in-between budget included.
    pub fn snap(&self, tokens: u64) -> u64 {
        self.context_steps().into_iter().min_by_key(|s| s.abs_diff(tokens)).unwrap_or(self.context)
    }
    fn steps(&self, with_saved: bool) -> Vec<u64> {
        if self.context <= MIN_CONTEXT {
            return vec![self.context];
        }
        let span = self.context - MIN_CONTEXT;
        let n = ((span as f64 / MIN_CONTEXT as f64).ceil() as u64).clamp(1, MAX_CONTEXT_STEPS);
        let mut steps: Vec<u64> = (0..=n).map(|i| MIN_CONTEXT + span * i / n).collect();
        if !with_saved { return steps; }
        if let Some(b) = self.context_budget.filter(|b| (MIN_CONTEXT..self.context).contains(b) && !steps.contains(b)) {
            steps.push(b);
            steps.sort_unstable();
        }
        steps
    }
    /// The budget requests stay inside: the slider's stop nearest the choice, the model's whole window until the user
    /// moves it (bug note 2026-10-28T19-41-00, the user 19:45: "by default we should set all the models and all the
    /// providers to the maximum level"; it was 500K, the stop nearest it: 501,714 on a 1M window). A saved choice stays.
    pub fn budget(&self) -> u64 {
        let want = self.context_budget.unwrap_or(self.context);
        self.context_steps().into_iter().min_by_key(|s| s.abs_diff(want)).unwrap_or(self.context)
    }
    pub fn limits(&self) -> Limits {
        let context = self.budget();
        let limits = Limits::for_context(context, Some(self.max_output));
        Limits { window: self.context.max(context), ..limits }
    }
    /// What one response cost in US dollars, from the token counts and this model's price (`None` when unknown).
    /// The price tier follows the request's whole prompt. One-hour cache writes cost 2× input, five-minute writes the
    /// model's `cacheWrite`; without the provider's split, `hour_cache` counts every write as a one-hour write.
    pub fn request_cost(&self, u: &Usage, hour_cache: bool) -> Option<f64> {
        let base = self.cost.as_ref()?;
        let tier = base["tiers"].as_array().into_iter().flatten()
            .filter(|t| t["inputTokensAbove"].as_u64().is_some_and(|above| u.prompt() > above))
            .max_by_key(|t| t["inputTokensAbove"].as_u64());
        let price = |k: &str| tier.and_then(|t| t[k].as_f64()).or_else(|| base[k].as_f64()).unwrap_or(0.0);
        let long = if u.cache_write_1h > 0 { u.cache_write_1h } else if hour_cache { u.cache_write } else { 0 };
        let short = u.cache_write - long;
        let total = u.input as f64 * price("input") + u.output as f64 * price("output") + u.cache_read as f64 * price("cacheRead")
            + short as f64 * price("cacheWrite") + long as f64 * 2.0 * price("input");
        Some(total / 1e6)
    }
    /// The thinking levels this model takes, least first (Pi's names). The Claude 5.5 models follow Anthropic's current
    /// rules on every provider: Opus cannot turn thinking off (effort Low to Max); Sonnet and Haiku can. Any other model
    /// takes what Pi's catalog entry (`pi`) maps, as Pi decides it: a level mapped to null is unsupported, Max only when
    /// mapped; a model without reasoning only Off.
    pub fn thinking_levels(&self, pi: Option<&Value>) -> Vec<&'static str> {
        match self.id.as_str() {
            OPUS => return vec!["low", "medium", "high", "max"],
            SONNET | HAIKU => return THINKING_ORDER.to_vec(),
            _ => {}
        }
        // The provider's own list (ChatGPT's `supported_reasoning_levels`, audit 27 Oct F11) names what the account
        // can run: those of the menu's levels, and Off where Pi knows the model takes "none".
        if !self.efforts.is_empty() {
            let off = self.efforts.iter().any(|e| e == "none") || pi.is_some_and(|e| e["thinkingLevelMap"]["off"].is_string());
            let levels: Vec<&'static str> = THINKING_ORDER.iter().copied()
                .filter(|l| if *l == "off" { off } else { self.efforts.iter().any(|e| e == l) }).collect();
            if !levels.is_empty() { return levels; }
        }
        let reasoning = pi.map_or(self.reasoning, |e| e["reasoning"] == true);
        if !reasoning { return vec!["off"]; }
        let map = pi.map(|e| &e["thinkingLevelMap"]);
        let levels: Vec<&'static str> = THINKING_ORDER.iter().copied().filter(|l| {
            let mapped = map.and_then(|m| m.get(*l));
            match *l {
                "max" => mapped.is_some_and(|v| !v.is_null()),
                _ => !mapped.is_some_and(Value::is_null),
            }
        }).collect();
        if levels.is_empty() { vec!["off"] } else { levels }
    }
    /// The level this model runs: the user's choice, else Low, clamped to what the model takes.
    pub fn thinking_level(&self, pi: Option<&Value>) -> &'static str {
        clamp_thinking(&self.thinking_levels(pi), self.thinking.as_deref().unwrap_or(DEFAULT_THINKING))
    }
    /// How Off reaches the provider when Pi cannot send it itself: Pi runs Anthropic's Claude Sonnet and Haiku 5.5 with
    /// managed effort (always adaptive thinking), so the extension rewrites an Off request (`runtime/thinking-policy.mjs`):
    /// Sonnet `between_tools`, Haiku `disabled`.
    pub fn thinking_off(&self) -> Option<&'static str> {
        if self.provider != ANTHROPIC { return None; }
        match self.id.as_str() {
            SONNET => Some("between_tools"),
            HAIKU => Some("disabled"),
            _ => None,
        }
    }
    /// Pi's definition for a model its catalog lacks (catalog models only get their limits overridden).
    pub fn pi_definition(&self) -> Result<Value> {
        self.pi_definition_with(self.limits())
    }
    /// `pi_definition` with the limits this start runs (`Limits::carrying`).
    pub fn pi_definition_with(&self, limits: Limits) -> Result<Value> {
        let api = self.api.pi().context("endpoint not supported by Pi")?;
        let mut def = json!({
            "id": self.id, "name": self.name, "api": api,
            "baseUrl": self.base_url.clone().or_else(|| self.provider_info().and_then(|p| p.base_url(self.api)).map(str::to_owned)),
            "reasoning": self.reasoning,
            "input": if self.images { json!(["text", "image"]) } else { json!(["text"]) },
            "contextWindow": limits.window, "maxTokens": limits.response,
            "cost": self.cost.clone().unwrap_or_else(|| json!({"input":0,"output":0,"cacheRead":0,"cacheWrite":0})),
        });
        if !self.efforts.is_empty() {
            // Pi's level → the provider's effort; a level the list does not name is unsupported (null).
            let has = |e: &str| self.efforts.iter().any(|x| x == e);
            let map: serde_json::Map<String, Value> = ["off", "minimal", "low", "medium", "high", "xhigh", "max"].iter().map(|l| {
                let effort = if *l == "off" { has("none").then_some("none") } else { has(l).then_some(*l) };
                ((*l).to_owned(), effort.map_or(Value::Null, |e| json!(e)))
            }).collect();
            def["thinkingLevelMap"] = Value::Object(map);
        }
        Ok(def)
    }
    /// This added row with the catalog's newer metadata (`fresh`: window, output limit, images, reasoning, efforts,
    /// format, price), keeping what the user chose: the name he saw, the thinking level, the endpoint, and the context
    /// budget, which a refresh never changes (the user decides how much of his plan a request may use): it stays exactly
    /// where it is, as its own stop when it falls between the new ones (Sol's 272K in a 500K window), and only a window
    /// that shrank below it lowers it to the largest stop that fits. Rows are captured when added (`SetApiModelAdded`), so a listing that grows (ChatGPT's Sol and Terra:
    /// 500K, audit 27 Oct F2) reaches them only through this.
    pub fn refreshed(&self, fresh: &ApiModel) -> ApiModel {
        // A catalog that knows less (no window, an unsupported format, Pi's catalog unreadable) never overwrites what
        // the row knows: such a refresh changes nothing, and a missing output limit, price or effort list is kept.
        if fresh.unsupported().is_some() || fresh.is_chatgpt_placeholder() { return self.clone(); }
        let mut row = ApiModel {
            provider: self.provider.clone(), id: self.id.clone(), name: self.name.clone(),
            thinking: self.thinking.clone(), base_url: self.base_url.clone().or_else(|| fresh.base_url.clone()), context_budget: None,
            max_output: if fresh.max_output > 0 { fresh.max_output } else { self.max_output },
            cost: fresh.cost.clone().or_else(|| self.cost.clone()),
            efforts: if fresh.efforts.is_empty() { self.efforts.clone() } else { fresh.efforts.clone() },
            ..fresh.clone()
        };
        if self.context_budget.is_some() {
            let old = self.budget();
            row.context_budget = Some(old);
            let steps = row.context_steps();
            row.context_budget = Some(steps.iter().copied().filter(|s| *s <= old).max().unwrap_or(steps[0]));
        }
        row
    }
    /// The placeholder Heimdall stored for a ChatGPT model Pi's catalog lacked (before 27 Oct: 128K, no output limit,
    /// no images; gpt-6.1-terra in the user's settings).
    pub fn is_chatgpt_placeholder(&self) -> bool {
        self.provider == CHATGPT && self.context <= MIN_CONTEXT && self.max_output == 0
    }
}

/// The family a ChatGPT model belongs to: its slug up to the last dash ("gpt-6.1" for gpt-6.1-terra and gpt-6.1-sol).
pub fn model_family(id: &str) -> &str {
    id.rsplit_once('-').map_or(id, |(family, _)| family)
}

/// Repairs the placeholder rows already in settings (`ApiModel::is_chatgpt_placeholder`) offline, from an added sibling
/// of the same family that is not one (Terra from Sol): its window, output limit, images, reasoning and efforts; the
/// account's list refreshes both once it loads (`ApiModel::refreshed`). Returns whether anything changed.
pub fn repair_placeholders(models: &mut [ApiModel]) -> bool {
    let mut changed = false;
    for i in 0..models.len() {
        if !models[i].is_chatgpt_placeholder() { continue; }
        let family = model_family(&models[i].id).to_owned();
        let Some(sibling) = models.iter().find(|m| m.provider == CHATGPT && !m.is_chatgpt_placeholder() && model_family(&m.id) == family).cloned() else { continue };
        let fresh = ApiModel { cost: None, ..sibling };
        models[i] = models[i].refreshed(&fresh);
        changed = true;
    }
    changed
}

/// The agent's model: the local engine's (whatever it runs), one added API model, or none chosen yet.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentModel {
    Local,
    Api { provider: String, id: String },
    /// Nothing chosen: the public edition's blank start. The agent waits with `NO_MODEL_CHOSEN` instead of starting.
    #[serde(rename = "none")]
    Unchosen,
}
impl Default for AgentModel {
    /// The personal edition starts on Claude Haiku 5.5 on OpenCode Go; the public edition starts with nothing chosen,
    /// and the setup wizard or the menu picks a provider.
    fn default() -> Self {
        if cfg!(feature = "public") { Self::Unchosen } else { Self::Api { provider: OPENCODE_GO.into(), id: HAIKU.into() } }
    }
}
impl AgentModel {
    pub fn is_local(&self) -> bool {
        matches!(self, Self::Local)
    }
    pub fn is_chosen(&self) -> bool {
        !matches!(self, Self::Unchosen)
    }
    pub fn api(&self) -> Option<(&str, &str)> {
        match self {
            Self::Api { provider, id } => Some((provider, id)),
            Self::Local | Self::Unchosen => None,
        }
    }
}

// ---------------------------------------------------------------------------------------------------------
// Credentials.

/// Only the installed, stably signed app touches the Keychain. Every other binary (tests, lab builds,
/// worktrees) has a different signature, so a Keychain query could raise a system prompt.
pub fn is_installed_app() -> bool {
    installed_app()
}
fn installed_app() -> bool {
    std::env::current_exe().is_ok_and(|p| installed_at(&p, std::env::var_os("HOME").map(std::path::PathBuf::from).as_deref()))
}
/// The installed app's bundle: Heimdall.app (personal) or Heimdall Alpha.app (public, `scripts/bundle.sh`).
const APP_BUNDLE: &str = if cfg!(feature = "public") { "Heimdall Alpha.app" } else { "Heimdall.app" };

/// The installed app runs from `~/Applications` (the user's install, the night pass) or `/Applications` (another user's
/// install of the public edition); a build, test or lab copy runs from anywhere else.
fn installed_at(exe: &std::path::Path, home: Option<&std::path::Path>) -> bool {
    let macos = std::path::Path::new(APP_BUNDLE).join("Contents/MacOS");
    exe.starts_with(std::path::Path::new("/Applications").join(&macos))
        || home.is_some_and(|h| exe.starts_with(h.join("Applications").join(&macos)))
}

/// Lab binaries keep connected credentials in memory for the process lifetime only: (provider, account) → bytes.
static LAB_ITEMS: std::sync::Mutex<BTreeMap<(&'static str, String), Vec<u8>>> = std::sync::Mutex::new(BTreeMap::new());

/// Tests that write lab credentials share this one store; each takes the lock first.
#[cfg(test)]
pub(crate) static LAB_ITEMS_TEST: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// errSecItemNotFound.
const ITEM_NOT_FOUND: i32 = -25300;

fn item_read(p: &ProviderInfo, account: &str) -> Option<Vec<u8>> {
    if !installed_app() {
        return LAB_ITEMS.lock().unwrap().get(&(p.id, account.to_owned())).cloned();
    }
    use security_framework::item::{ItemClass, ItemSearchOptions, SearchResult};
    // Items that would need interaction are skipped: a lookup can never show a system prompt.
    let items = ItemSearchOptions::new()
        .class(ItemClass::generic_password())
        .service(p.keychain_service)
        .account(account)
        .load_data(true)
        .skip_authenticated_items(true)
        .search()
        .ok()?;
    items.into_iter().find_map(|i| match i { SearchResult::Data(bytes) => Some(bytes), _ => None })
}

fn item_write(p: &ProviderInfo, account: &str, bytes: &[u8]) -> Result<()> {
    if !installed_app() {
        LAB_ITEMS.lock().unwrap().insert((p.id, account.to_owned()), bytes.to_vec());
        return Ok(());
    }
    // Replace by delete + add: both run without interaction for items this app created.
    item_delete(p, account)?;
    let mut options = security_framework::passwords::PasswordOptions::new_generic_password(p.keychain_service, account);
    options.set_label(p.keychain_service);
    security_framework::passwords::set_generic_password_options(bytes, options)
        .map_err(|_| anyhow::anyhow!("Could not save the sign-in in the Keychain"))
}

fn item_delete(p: &ProviderInfo, account: &str) -> Result<()> {
    if !installed_app() {
        LAB_ITEMS.lock().unwrap().remove(&(p.id, account.to_owned()));
        return Ok(());
    }
    use security_framework::item::{ItemClass, ItemSearchOptions};
    match ItemSearchOptions::new()
        .class(ItemClass::generic_password())
        .service(p.keychain_service)
        .account(account)
        .skip_authenticated_items(true)
        .delete()
    {
        Ok(()) => Ok(()),
        Err(e) if e.code() == ITEM_NOT_FOUND => Ok(()),
        Err(_) => bail!("Could not remove the sign-in from the Keychain"),
    }
}

fn console_account(p: &ProviderInfo) -> String {
    format!("{}-console", p.id)
}

/// A pasted key, trimmed. Never echoed in an error.
pub fn normalize_key(key: &str) -> Result<String> {
    let key = key.trim();
    ensure!(!key.is_empty(), "Paste an API key");
    ensure!(key.len() <= 512 && key.chars().all(|c| c.is_ascii_graphic()), "That is not an API key");
    Ok(key.to_owned())
}

/// Clipboard text worth pre-filling into the paste field: one token shaped like an OpenCode key.
pub fn looks_like_key(text: &str) -> bool {
    let t = text.trim();
    t.starts_with("sk-") && (24..=512).contains(&t.len()) && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyMissing(pub &'static str);
impl std::fmt::Display for KeyMissing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Connect {}", self.0)
    }
}
impl std::error::Error for KeyMissing {}

/// How Heimdall is connected to a provider: a Console sign-in (primary) or a pasted API key (fallback).
#[derive(Clone, Debug, PartialEq)]
pub enum Credential {
    Key(String),
    Console(crate::console::Session),
}

/// The environment's key (lab), else the saved sign-in, else the saved key.
pub fn credential(p: &ProviderInfo) -> Result<Credential> {
    if let Some(key) = std::env::var(p.key_env).ok().and_then(|k| normalize_key(&k).ok()) {
        return Ok(Credential::Key(key));
    }
    if let Some(session) = console_session(p) {
        return Ok(Credential::Console(session));
    }
    api_key(p).map(Credential::Key)
}

pub fn has_credential(p: &ProviderInfo) -> bool {
    credential(p).is_ok() && (!p.custom_url || base_url(p).is_some())
}

/// The pasted API key, if that is how the provider is connected.
pub fn api_key(p: &ProviderInfo) -> Result<String> {
    if let Some(key) = std::env::var(p.key_env).ok().and_then(|k| normalize_key(&k).ok()) {
        return Ok(key);
    }
    item_read(p, p.id)
        .and_then(|b| String::from_utf8(b).ok())
        .and_then(|k| normalize_key(&k).ok())
        .ok_or_else(|| KeyMissing(p.name).into())
}

/// Keeps a verified key with the workspace its requests name (`None`: a workspace-scoped key); it replaces any
/// Console sign-in (one connection per provider).
pub fn save_key(p: &ProviderInfo, key: &str, workspace: Option<&str>) -> Result<()> {
    let key = normalize_key(key)?;
    item_write(p, p.id, key.as_bytes())?;
    save_workspace(p, workspace)?;
    item_delete(p, &console_account(p))
}

/// The Keychain account of the workspace a key's requests name: "<provider id>-workspace" (Anthropic:
/// "anthropic-workspace"), in the provider's service, holding the id as UTF-8 ("wrkspc_…").
pub fn workspace_account(p: &ProviderInfo) -> String {
    format!("{}-workspace", p.id)
}

/// The workspace the saved key's requests name, if it needs one: the environment's (lab, beside an environment
/// key), else the Keychain's.
pub fn workspace_id(p: &ProviderInfo) -> Option<String> {
    let w = p.workspaces?;
    if std::env::var(p.key_env).ok().and_then(|k| normalize_key(&k).ok()).is_some() {
        return std::env::var(w.env).ok().and_then(|id| normalize_workspace(&id));
    }
    item_read(p, &workspace_account(p)).and_then(|b| String::from_utf8(b).ok()).and_then(|id| normalize_workspace(&id))
}

/// Records which workspace the key's requests name (`None` removes it). The id is not secret but lives beside the key.
pub fn save_workspace(p: &ProviderInfo, workspace: Option<&str>) -> Result<()> {
    if p.workspaces.is_none() {
        return Ok(());
    }
    match workspace.and_then(normalize_workspace) {
        Some(id) => item_write(p, &workspace_account(p), id.as_bytes()),
        None => item_delete(p, &workspace_account(p)),
    }
}

fn normalize_workspace(id: &str) -> Option<String> {
    let id = id.trim();
    (!id.is_empty() && id.len() <= 128 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')).then(|| id.to_owned())
}

pub fn console_session(p: &ProviderInfo) -> Option<crate::console::Session> {
    serde_json::from_slice(&item_read(p, &console_account(p))?).ok()
}

/// Keeps a Console sign-in; it replaces any pasted key.
pub fn save_console_session(p: &ProviderInfo, s: &crate::console::Session) -> Result<()> {
    item_write(p, &console_account(p), &serde_json::to_vec(s)?)?;
    item_delete(p, p.id)?;
    save_workspace(p, None)
}

pub fn forget_console_session(p: &ProviderInfo) -> Result<()> {
    item_delete(p, &console_account(p))
}

/// Disconnect: both the sign-in and the key.
pub fn forget_key(p: &ProviderInfo) -> Result<()> {
    item_delete(p, p.id)?;
    if p.custom_url { item_delete(p, &base_account(p))?; }
    save_workspace(p, None)?;
    item_delete(p, &console_account(p))
}

// ---------------------------------------------------------------------------------------------------------
// Network: key check and the model list. Provider bodies and request headers are never echoed (they can
// carry credentials).

/// The answer to "does this key work?".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyCheck {
    /// Accepted; `workspace` is the workspace its requests must name (`None`: the key is scoped to one already).
    Valid { workspace: Option<String> },
    /// 401/403: a wrong key, or a key without the subscription.
    Rejected,
    /// Another refusal, in the provider's own words ("Anthropic: …").
    Refused(String),
    Unreachable,
}

/// A 401/403 from the provider: the key or sign-in is not accepted, or the account has no subscription.
pub fn rejected_message(p: &ProviderInfo) -> String {
    format!("{}: {}", p.name, p.rejected)
}
pub fn unreachable_message(p: &ProviderInfo) -> String {
    format!("{} unreachable", p.name)
}
/// Anthropic's answer when the organisation's prepaid credits (the Max plan's monthly API credits included) are spent.
pub fn credits_message(p: &ProviderInfo) -> String {
    format!("{} credits used up", p.name)
}
/// Words in a provider error that mean the account has no money left, whatever the status code.
pub fn says_credits_exhausted(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    t.contains("credit balance") || t.contains("billing_error") || (t.contains("credit") && (t.contains("exhaust") || t.contains("used up") || t.contains("insufficient")))
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(concat!("heimdall/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(20))
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .expect("HTTP client")
}

/// A provider request with the key in the provider's header and, for a key that needs one, the workspace it names.
fn keyed(p: &ProviderInfo, mut request: reqwest::RequestBuilder, key: Option<&str>, workspace: Option<&str>) -> reqwest::RequestBuilder {
    if p.key_header == KeyHeader::XApiKey {
        request = request.header("anthropic-version", ANTHROPIC_VERSION);
    }
    if let (Some(w), Some(id)) = (p.workspaces, workspace) {
        request = request.header(w.header, id);
    }
    match (key, p.key_header) {
        (Some(key), KeyHeader::Bearer) => request.bearer_auth(key),
        (Some(key), KeyHeader::XApiKey) => request.header("x-api-key", key),
        (None, _) => request,
    }
}

/// The model list request.
fn models_request(p: &ProviderInfo, key: Option<&str>, workspace: Option<&str>, models_url: Option<&str>) -> reqwest::RequestBuilder {
    keyed(p, client().get(models_url.unwrap_or(p.models_url)), key, workspace)
}

/// `url`'s path on the origin of `base` (a test's fake serves every endpoint on one origin).
fn on_origin(base: &str, url: &str) -> String {
    let origin = |u: &str| u.find("://").and_then(|i| u[i + 3..].find('/').map(|j| u[..i + 3 + j].to_owned())).unwrap_or_else(|| u.to_owned());
    format!("{}{}", origin(base), &url[origin(url).len()..])
}

/// The provider's error message from a JSON error body (`{"error":{"message":…}}`), trimmed to one line.
fn error_message(body: &str) -> Option<String> {
    let v: Value = serde_json::from_str(body).ok()?;
    let m = v["error"]["message"].as_str().or_else(|| v["message"].as_str())?.trim();
    let m = m.lines().next().unwrap_or_default();
    (!m.is_empty()).then(|| if m.chars().count() > 240 { format!("{}…", m.chars().take(240).collect::<String>()) } else { m.to_owned() })
}

/// Anthropic's 400 for a key that belongs to no single workspace: the request must name one.
fn needs_workspace(status: u16, body: &str) -> bool {
    status == 400 && body.contains("workspace")
}

/// The organisation's workspace a key's requests should name: the only active one, else the Default one.
async fn pick_workspace(p: &ProviderInfo, w: Workspaces, key: &str, models_url: Option<&str>) -> Result<String, KeyCheck> {
    let url = models_url.map_or_else(|| w.list_url.to_owned(), |m| on_origin(m, w.list_url));
    let response = keyed(p, client().get(url), Some(key), None).send().await.map_err(|_| KeyCheck::Unreachable)?;
    let status = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();
    match status {
        200..=299 => {}
        401 | 403 => return Err(error_message(&body).map_or(KeyCheck::Rejected, |m| KeyCheck::Refused(format!("{}: {m}", p.name)))),
        400..=499 => return Err(KeyCheck::Refused(format!("{}: {}", p.name, error_message(&body).unwrap_or_else(|| format!("workspace list refused ({status})"))))),
        _ => return Err(KeyCheck::Unreachable),
    }
    let v: Value = serde_json::from_str(&body).map_err(|_| KeyCheck::Unreachable)?;
    let active: Vec<&Value> = v["data"].as_array().into_iter().flatten().filter(|w| w["id"].is_string() && w["archived_at"].is_null()).collect();
    let chosen = match active.as_slice() {
        [only] => Some(*only),
        many => many.iter().copied().find(|w| w["is_default"] == true || w["default"] == true)
            .or_else(|| many.iter().copied().find(|w| w["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case("default")))),
    };
    chosen.and_then(|w| w["id"].as_str()).map(str::to_owned).ok_or_else(|| KeyCheck::Refused(match active.len() {
        0 => format!("{}: this key reaches no workspace", p.name),
        n => format!("{}: this key reaches {n} workspaces and none is the default; create the key inside one workspace", p.name),
    }))
}

/// Checks a key against the model list. A key that is not scoped to a workspace (Anthropic answers 400 "must include
/// the anthropic-workspace-id header") gets the organisation's workspace, and the check is repeated naming it; a
/// saved `workspace` that stopped working is looked up again. `models_url` overrides the provider's (tests point it
/// at a local fake; the workspace list is then read from the same origin).
pub async fn check_key(p: &ProviderInfo, key: &str, workspace: Option<&str>, models_url: Option<&str>) -> KeyCheck {
    let mut workspace = workspace.map(str::to_owned);
    let mut looked_up = false;
    loop {
        let Ok(response) = models_request(p, Some(key), workspace.as_deref(), models_url).send().await else { return KeyCheck::Unreachable };
        let status = response.status().as_u16();
        if (200..300).contains(&status) {
            return KeyCheck::Valid { workspace };
        }
        let body = response.text().await.unwrap_or_default();
        if let Some(w) = p.workspaces.filter(|_| !looked_up) {
            // Unscoped key, or a saved workspace the key can no longer use.
            if needs_workspace(status, &body) || (workspace.is_some() && matches!(status, 400 | 403 | 404) && body.contains("workspace")) {
                looked_up = true;
                match pick_workspace(p, w, key, models_url).await {
                    Ok(id) => { workspace = Some(id); continue; }
                    Err(check) => return check,
                }
            }
        }
        return match status {
            401 | 403 => KeyCheck::Rejected,
            400..=499 => KeyCheck::Refused(format!("{}: {}", p.name, error_message(&body).unwrap_or_else(|| format!("request refused ({status})")))),
            _ => KeyCheck::Unreachable,
        };
    }
}

/// The models the provider serves now, in its order: each entry has at least `id`; Anthropic's also carry
/// `display_name`, `max_input_tokens`, `max_tokens` and `capabilities`.
pub async fn live_models(p: &ProviderInfo, key: Option<&str>, workspace: Option<&str>, models_url: Option<&str>) -> Result<Vec<Value>> {
    if key.is_none() && !p.public_models {
        bail!("{}", KeyMissing(p.name));
    }
    let response = models_request(p, key, workspace, models_url).send().await.map_err(|_| anyhow::anyhow!("{}", unreachable_message(p)))?;
    match response.status().as_u16() {
        200..=299 => {}
        401 | 403 => bail!("{}", rejected_message(p)),
        status @ 400..=499 => {
            let body = response.text().await.unwrap_or_default();
            bail!("{}: {}", p.name, error_message(&body).unwrap_or_else(|| format!("model list refused ({status})")))
        }
        _ => bail!("{}", unreachable_message(p)),
    }
    let body: Value = response.json().await.map_err(|_| anyhow::anyhow!("{} model list unreadable", p.name))?;
    let models: Vec<Value> = if p.id == CHATGPT { crate::chatgpt::listing(&body) } else {
        body["data"].as_array().into_iter().flatten().filter(|m| m["id"].is_string()).cloned().collect()
    };
    ensure!(!models.is_empty(), "{} lists no models", p.name);
    Ok(models)
}

/// models.dev's entry for this provider, fetched only when Pi's catalog and the model list leave some model undescribed.
pub async fn fetch_metadata(p: &ProviderInfo, metadata_url: Option<&str>) -> Result<Value> {
    let url = metadata_url.or(p.metadata_url).context("no public model catalog")?;
    let response = client()
        .get(url)
        .timeout(std::time::Duration::from_secs(45))
        .send()
        .await
        .context("model catalog unreachable")?;
    ensure!(response.status().is_success(), "model catalog unavailable");
    let mut all: Value = response.json().await.context("model catalog unreadable")?;
    Ok(all[p.id].take())
}

/// Pi's pinned catalog for this provider, read from the installed runtime (`None` before installation).
pub fn pi_catalog(runtime_root: &std::path::Path, p: &ProviderInfo) -> Option<Value> {
    let path = runtime_root.join(format!("node_modules/@earendil-works/pi-ai/dist/providers/data/{}.json", p.id));
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Pi's catalog entry for `id`, if Pi 1.2.0 knows it (a chat model).
pub fn pi_entry<'a>(catalog: &'a Value, id: &str) -> Option<&'a Value> {
    catalog.as_object()?.values().filter_map(|byapi| byapi.get(format!("chat:{id}"))).find(|m| m["type"] == "chat")
}

fn from_pi(p: &ProviderInfo, m: &Value) -> ApiModel {
    let cost = m["cost"].is_object().then(|| {
        let mut c = json!({});
        for k in ["input", "output", "cacheRead", "cacheWrite", "tiers"] {
            if !m["cost"][k].is_null() { c[k] = m["cost"][k].clone(); }
        }
        c
    });
    ApiModel {
        provider: p.id.into(),
        id: m["id"].as_str().unwrap_or_default().into(),
        name: m["name"].as_str().unwrap_or_else(|| m["id"].as_str().unwrap_or_default()).into(),
        api: Api::from_pi(m["api"].as_str().unwrap_or_default()),
        context: m["contextWindow"].as_u64().unwrap_or(0),
        max_output: m["maxTokens"].as_u64().unwrap_or(0),
        images: m["input"].as_array().is_some_and(|a| a.iter().any(|i| i == "image")),
        reasoning: m["reasoning"] == true,
        cost,
        context_budget: None,
        thinking: None,
        base_url: None,
        efforts: vec![],
    }
}

/// An Anthropic model Pi's catalog lacks, from the model list's own metadata. Thinking stays off in Pi's request
/// (Pi would send the old fixed-budget form, which current Claude models reject); such models think adaptively by
/// default. Its price is unknown, so it adds nothing to the spend estimate.
fn from_listing(p: &ProviderInfo, m: &Value) -> Option<ApiModel> {
    let context = m["max_input_tokens"].as_u64()?;
    let id = m["id"].as_str()?;
    Some(ApiModel {
        provider: p.id.into(),
        id: id.into(),
        name: m["display_name"].as_str().unwrap_or(id).into(),
        api: Api::AnthropicMessages,
        context,
        max_output: m["max_tokens"].as_u64().unwrap_or(0),
        images: m["capabilities"]["image_input"]["supported"] == true,
        reasoning: false,
        cost: None,
        context_budget: None,
        thinking: None,
        base_url: None,
        efforts: vec![],
    })
}

fn from_models_dev(p: &ProviderInfo, id: &str, m: &Value, provider_npm: Option<&str>) -> ApiModel {
    let npm = m["provider"]["npm"].as_str().or(provider_npm).unwrap_or_default();
    let price = |c: &Value| json!({
        "input": c["input"].as_f64().unwrap_or(0.0), "output": c["output"].as_f64().unwrap_or(0.0),
        "cacheRead": c["cache_read"].as_f64().unwrap_or(0.0), "cacheWrite": c["cache_write"].as_f64().unwrap_or(0.0),
    });
    let tiers: Vec<(u64, Value)> = m["cost"]["tiers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|t| t["tier"]["type"] == "context")
        .filter_map(|t| Some((t["tier"]["size"].as_u64()?, t.clone())))
        .collect();
    let mut cost = price(&m["cost"]);
    if !tiers.is_empty() {
        cost["tiers"] = tiers.iter().map(|(size, t)| { let mut c = price(t); c["inputTokensAbove"] = json!(size); c }).collect();
    }
    ApiModel {
        provider: p.id.into(),
        id: id.into(),
        name: m["name"].as_str().unwrap_or(id).into(),
        api: Api::from_npm(npm),
        context: m["limit"]["context"].as_u64().unwrap_or(0),
        max_output: m["limit"]["output"].as_u64().unwrap_or(0),
        images: m["modalities"]["input"].as_array().is_some_and(|a| a.iter().any(|i| i == "image")),
        reasoning: m["reasoning"] == true,
        cost: m["cost"].is_object().then_some(cost),
        context_budget: None,
        thinking: None,
        base_url: None,
        efforts: vec![],
    }
}

/// One row per listed model: Pi's catalog first (it decides the request format Pi uses), then the listing's own
/// metadata (Anthropic), then the public catalog (models.dev for OpenCode), else an unsupported row named by its id. The
/// ChatGPT sign-in's rows take their window, images and efforts from the account's list over Pi's (`from_chatgpt_listing`).
pub fn merge_catalog(p: &ProviderInfo, listed: &[Value], pi: Option<&Value>, models_dev: Option<&Value>) -> Vec<ApiModel> {
    merge_catalog_at(p, listed, pi, models_dev, None)
}

/// `merge_catalog` for a provider on the user's own URL: every row it describes from the listing carries `base`.
pub fn merge_catalog_at(p: &ProviderInfo, listed: &[Value], pi: Option<&Value>, models_dev: Option<&Value>, base: Option<&str>) -> Vec<ApiModel> {
    let npm = models_dev.and_then(|d| d["npm"].as_str());
    listed
        .iter()
        .filter_map(|m| m["id"].as_str().map(|id| (id, m)))
        .map(|(id, m)| {
            let mut row = catalog_row(p, id, m, pi, models_dev, npm, base);
            // A custom provider's every row goes to its own URL.
            if p.custom_url && row.base_url.is_none() { row.base_url = base.map(str::to_owned); }
            row
        })
        .collect()
}

/// One listed model's row, from the first source that describes it (`merge_catalog_at`).
#[allow(clippy::too_many_arguments)]
fn catalog_row(p: &ProviderInfo, id: &str, m: &Value, pi: Option<&Value>, models_dev: Option<&Value>, npm: Option<&str>, base: Option<&str>) -> ApiModel {
    if p.id == CHATGPT {
        return from_chatgpt_listing(p, id, m, pi);
    }
    if let Some(e) = pi.and_then(|c| pi_entry(c, id)) {
        let mut row = from_pi(p, e);
        // The account's own window when the listing states one (it can be smaller than Pi's figure).
        if let Some(window) = m["max_input_tokens"].as_u64().filter(|w| *w >= 8192) { row.context = window; }
        return row;
    }
    // Anthropic's listing shape; an OpenAI-compatible server stating a window in the same field is still OpenAI.
    if let Some(row) = from_listing(p, m).filter(|_| !(p.custom_url && p.key_header == KeyHeader::Bearer)) {
        return row;
    }
    if let Some(e) = models_dev.and_then(|d| d["models"].get(id)) {
        return from_models_dev(p, id, e, npm);
    }
    if p.custom_url {
        return from_openai_listing(p, id, m, base);
    }
    ApiModel { provider: p.id.into(), id: id.into(), name: id.into(), api: Api::Unknown, context: 0, max_output: 0, images: false, reasoning: false, cost: None, context_budget: None, thinking: None, base_url: None, efforts: vec![] }
}

/// [providers] A model on the ChatGPT sign-in (audit 27 Oct, F2). The account's own list (`chatgpt::listing`) decides
/// what it states: the window (`context_window`, the account's default and the slider's top: 500,000 for Sol and Terra
/// against Pi 1.2.0's 272,000; `max_context_window`, 872,000, is not offered: untested), images (`input_modalities`)
/// and the reasoning efforts. Pi's catalog gives the rest (format, output limit, price); a model Pi lacks (Terra) takes
/// them from a sibling of its family Pi knows (Sol), else OpenAI's Responses API with reasoning on (Pi then sends the
/// system prompt as a developer message; the route rejects system items) and no stated output limit.
fn from_chatgpt_listing(p: &ProviderInfo, id: &str, m: &Value, pi: Option<&Value>) -> ApiModel {
    let own = pi.and_then(|c| pi_entry(c, id));
    let sibling = own.or_else(|| pi.and_then(|c| family_entry(c, id)));
    let listed_name = m["display_name"].as_str().unwrap_or(id);
    let mut row = match sibling {
        Some(e) => {
            let row = from_pi(p, e);
            // A sibling's price and name are not this model's.
            if own.is_some() { row } else { ApiModel { name: listed_name.into(), cost: None, ..row } }
        }
        None => ApiModel { provider: p.id.into(), id: id.into(), name: listed_name.into(), api: Api::OpenaiResponses, context: MIN_CONTEXT, max_output: 0,
            images: false, reasoning: true, cost: None, context_budget: None, thinking: None, base_url: None, efforts: vec![] },
    };
    row.id = id.into();
    if let Some(window) = m["context_window"].as_u64().filter(|w| *w >= 8192) { row.context = window; }
    if let Some(input) = m["input_modalities"].as_array() { row.images = input.iter().any(|i| i == "image"); }
    row.efforts = m["supported_reasoning_levels"].as_array().into_iter().flatten()
        .filter_map(|l| l.as_str().or_else(|| l["effort"].as_str()).map(str::to_owned)).collect();
    row
}

/// Pi's chat entry for another model of `id`'s family (`model_family`: a versioned prefix such as "gpt-6.1", never a
/// bare name) on OpenAI's Responses format, which the ChatGPT route serves; the first by id. The exact id is Pi's own
/// entry (`pi_entry`), tried before this.
fn family_entry<'a>(catalog: &'a Value, id: &str) -> Option<&'a Value> {
    let family = model_family(id);
    if family == id || !family.bytes().any(|b| b.is_ascii_digit()) { return None; }
    let family = format!("{family}-");
    let mut found: Vec<&Value> = catalog.get("openai-responses")?.as_object()?.values()
        .filter(|m| m["type"] == "chat" && m["api"] == "openai-responses" && m["id"].as_str().is_some_and(|i| i != id && i.starts_with(&family)))
        .collect();
    found.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    found.into_iter().next()
}

/// [providers] A model from a custom provider's `/models` entry (OpenRouter, Groq, vLLM, LM Studio, Ollama, llama.cpp):
/// the window from whichever field the server states (OpenRouter `context_length`, vLLM `max_model_len`, others
/// `context_window`/`max_context_length`), else Pi's own default of 128K; OpenRouter's per-token prices and
/// capabilities when present. Served as OpenAI Chat Completions, or the Messages API on an Anthropic-compatible one.
fn from_openai_listing(p: &ProviderInfo, id: &str, m: &Value, base: Option<&str>) -> ApiModel {
    let window = ["context_length", "max_model_len", "context_window", "max_context_length"].iter().find_map(|k| m[*k].as_u64())
        .or_else(|| m["top_provider"]["context_length"].as_u64())
        .filter(|w| *w >= 8192)
        .unwrap_or(MIN_CONTEXT);
    let max_output = m["top_provider"]["max_completion_tokens"].as_u64().or_else(|| m["max_output_tokens"].as_u64()).unwrap_or(0);
    let per_m = |k: &str| m["pricing"][k].as_str().and_then(|v| v.parse::<f64>().ok()).or_else(|| m["pricing"][k].as_f64()).map(|v| (v * 1e6 * 1e4).round() / 1e4);
    let cost = match (per_m("prompt"), per_m("completion")) {
        (Some(input), Some(output)) => Some(json!({"input": input, "output": output, "cacheRead": per_m("input_cache_read").unwrap_or(0.0), "cacheWrite": per_m("input_cache_write").unwrap_or(0.0)})),
        _ => None,
    };
    let params = |k: &str| m["supported_parameters"].as_array().is_some_and(|a| a.iter().any(|v| v == k));
    ApiModel {
        provider: p.id.into(),
        id: id.into(),
        name: m["name"].as_str().filter(|n| !n.is_empty()).unwrap_or(id).into(),
        api: if p.key_header == KeyHeader::XApiKey { Api::AnthropicMessages } else { Api::OpenaiCompletions },
        context: window,
        max_output,
        images: m["architecture"]["input_modalities"].as_array().is_some_and(|a| a.iter().any(|v| v == "image")),
        reasoning: params("reasoning") || params("include_reasoning"),
        cost,
        context_budget: None,
        thinking: None,
        base_url: base.map(str::to_owned),
        efforts: vec![],
    }
}

// [providers] Endpoints on the user's own URL: kept beside the key, like Anthropic's workspace.

/// The Keychain account of the base URL a custom provider's requests go to: "<provider id>-base".
fn base_account(p: &ProviderInfo) -> String {
    format!("{}-base", p.id)
}

/// A base URL as the user typed it, trimmed to "http(s)://host[:port]/path" without a trailing slash or `/models`.
pub fn normalize_base_url(url: &str) -> Result<String> {
    let url = url.trim().trim_end_matches('/');
    let url = url.strip_suffix("/models").unwrap_or(url).trim_end_matches('/');
    ensure!(!url.is_empty(), "Enter the server's URL");
    let parsed = url::Url::parse(url).map_err(|_| anyhow::anyhow!("That is not a URL"))?;
    ensure!(matches!(parsed.scheme(), "http" | "https") && parsed.host_str().is_some() && url.len() <= 512, "That is not an http(s) URL");
    Ok(url.to_owned())
}

/// `normalize_base_url` for a provider: an Anthropic-compatible base is the server's root, without `/v1` (the Messages
/// API is `<base>/v1/messages`, as on api.anthropic.com), so a typed `/v1` or `/v1/messages` is dropped.
pub fn normalize_base_url_for(p: &ProviderInfo, url: &str) -> Result<String> {
    let url = normalize_base_url(url)?;
    if p.key_header != KeyHeader::XApiKey { return Ok(url); }
    let root = url.strip_suffix("/messages").unwrap_or(&url);
    let root = root.strip_suffix("/v1").unwrap_or(root);
    normalize_base_url(root)
}

/// A custom provider's model list at `base`: `<base>/models` (OpenAI), `<base>/v1/models` (Anthropic).
pub fn models_url_at(p: &ProviderInfo, base: &str) -> String {
    if p.key_header == KeyHeader::XApiKey { format!("{base}/v1/models") } else { format!("{base}/models") }
}

/// The saved base URL of a custom provider (`None` until connected, and for every other provider).
pub fn base_url(p: &ProviderInfo) -> Option<String> {
    if !p.custom_url { return None; }
    item_read(p, &base_account(p)).and_then(|b| String::from_utf8(b).ok()).and_then(|u| normalize_base_url_for(p, &u).ok())
}

/// Connects a custom provider: its base URL and key (a keyless engine keeps `NO_KEY`).
pub fn save_endpoint(p: &ProviderInfo, url: &str, key: Option<&str>) -> Result<()> {
    ensure!(p.custom_url, "{} has a fixed endpoint", p.name);
    let url = normalize_base_url_for(p, url)?;
    let key = match key.map(str::trim).filter(|k| !k.is_empty()) {
        Some(k) => normalize_key(k)?,
        None if !p.key_required => NO_KEY.to_owned(),
        None => bail!("Paste an API key"),
    };
    item_write(p, &base_account(p), url.as_bytes())?;
    item_write(p, p.id, key.as_bytes())
}

/// The model list's URL for a provider: the saved base's `/models` for a custom provider, else the catalog's.
pub fn models_url_for(p: &ProviderInfo) -> Option<String> {
    if p.custom_url { base_url(p).map(|b| models_url_at(p, &b)) } else { Some(p.models_url.to_owned()) }
}

/// Own Engine's saved URL and key move to the OpenAI-compatible custom provider once, when that one has none yet
/// (Own Engine was the same wire under another name). The old items stay where they are. Returns whether it moved.
pub fn adopt_retired_endpoint() -> Result<bool> {
    let custom = provider(CUSTOM).context("custom provider")?;
    if base_url(custom).is_some() { return Ok(false); }
    let Some(url) = base_url(&RETIRED_OWN_ENGINE_INFO) else { return Ok(false) };
    let key = item_read(&RETIRED_OWN_ENGINE_INFO, RETIRED_OWN_ENGINE).and_then(|b| String::from_utf8(b).ok());
    save_endpoint(custom, &url, key.as_deref())?;
    Ok(true)
}

/// Whether some listed model needs the public catalog (neither Pi nor the listing describes it).
pub fn needs_metadata(listed: &[Value], pi: Option<&Value>) -> bool {
    listed.iter().any(|m| {
        let id = m["id"].as_str().unwrap_or_default();
        pi.and_then(|c| pi_entry(c, id)).is_none() && m["max_input_tokens"].as_u64().is_none()
    })
}

/// The picker's list: live entries, then metadata.
pub async fn fetch_catalog(
    p: &ProviderInfo,
    key: Option<&str>,
    workspace: Option<&str>,
    pi: Option<&Value>,
    urls: (Option<&str>, Option<&str>),
) -> Result<Vec<ApiModel>> {
    let listed = live_models(p, key, workspace, urls.0).await?;
    // A models.dev outage only costs metadata for models nothing else describes; they show as unavailable.
    let models_dev = if needs_metadata(&listed, pi) && (urls.1.is_some() || p.metadata_url.is_some()) { fetch_metadata(p, urls.1).await.ok() } else { None };
    Ok(merge_catalog(p, &listed, pi, models_dev.as_ref()))
}

/// Pi's raw provider error, in words, for the menu and the agent window. `None` leaves the text as is.
/// `provider_id` is the failed assistant message's own `provider` field.
pub fn explain_error(provider_id: &str, raw: &str) -> Option<String> {
    let p = provider(provider_id)?;
    // [providers] OpenAI's plan-usage codes first: they come with 403/429/503 statuses that mean something else elsewhere.
    let status = raw.trim_start().get(..3).and_then(|s| s.parse::<u16>().ok());
    if p.id == CHATGPT {
        if let Some(why) = crate::chatgpt::explain(raw) { return Some(why); }
        // An admission 403 without a plan code is a policy or region check, not a sign-in problem (OpenAI's errors page).
        let status = status.or_else(|| raw.split_once('(').and_then(|(_, r)| r.get(..3)).and_then(|s| s.parse().ok()));
        if status == Some(403) { return Some("ChatGPT: request not allowed for this account or region".into()); }
        if status == Some(503) { return Some("ChatGPT: unavailable · try again shortly".into()); }
    }
    if says_credits_exhausted(raw) && p.id == ANTHROPIC {
        return Some(credits_message(p));
    }
    match status {
        Some(401) => Some(rejected_message(p)),
        Some(403) if p.id == ANTHROPIC => Some(format!("{}: key not allowed to use this model", p.name)),
        Some(403) => Some(rejected_message(p)),
        Some(413) => Some(format!("{}: request too large for this model", p.name)),
        Some(429) if p.id == ANTHROPIC => Some(format!("{}: rate limit reached", p.name)),
        Some(429) => Some(format!("{}: usage limit reached", p.name)),
        Some(529) => Some(format!("{} overloaded · try again shortly", p.name)),
        Some(404) => Some(format!("{}: endpoint not found for this model", p.name)),
        _ if raw.contains("overloaded_error") => Some(format!("{} overloaded · try again shortly", p.name)),
        _ if raw.contains("Invalid credential") || raw.contains("invalid x-api-key") => Some(rejected_message(p)),
        // Pi's per-request token command failed: offline with an expired token, or the session ended. The menu
        // tells which (the app re-checks the sign-in when it sees this).
        _ if raw.contains("Failed to resolve API key") && raw.contains("--provider-token") => Some(format!("{}: sign-in could not be refreshed", p.name)),
        _ => None,
    }
}

#[cfg(test)]
#[path = "provider_tests/tests.rs"]
mod tests;

#[cfg(test)]
mod installed_tests {
    use std::path::Path;
    /// The user's install keeps its Keychain access; another user's install (either Applications folder) gets it too,
    /// and build, test and lab copies never do (they would raise a Keychain prompt).
    #[test]
    fn the_installed_app_is_either_applications_folder_and_nothing_else() {
        let owner = Path::new("/Users/user");
        // The edition's own bundle name (Heimdall.app, or Heimdall Alpha.app in the public edition).
        let app = |p: &str| p.replace("Heimdall.app", super::APP_BUNDLE);
        let at = |p: &str, home: Option<&Path>| super::installed_at(Path::new(&app(p)), home);
        assert!(at("/Users/user/Applications/Heimdall.app/Contents/MacOS/Heimdall", Some(owner)));
        assert!(at("/Applications/Heimdall.app/Contents/MacOS/Heimdall", Some(Path::new("/Users/ana"))));
        assert!(at("/Users/ana/Applications/Heimdall.app/Contents/MacOS/Heimdall", Some(Path::new("/Users/ana"))));
        for other in ["/Users/user/Code/Heimdall/dist/Heimdall.app/Contents/MacOS/Heimdall",
                      "/Users/user/Code/Heimdall/target/release/heimdall",
                      "/Users/ana/Applications/Heimdall.app/Contents/MacOS/Heimdall"] {
            assert!(!at(other, Some(owner)), "{other}");
        }
        assert!(!at("/Users/ana/Applications/Heimdall.app/Contents/MacOS/Heimdall", None));
        // The other edition's bundle is never this one's.
        let other = if super::APP_BUNDLE == "Heimdall.app" { "Heimdall Alpha.app" } else { "Heimdall.app" };
        assert!(!super::installed_at(&Path::new("/Applications").join(other).join("Contents/MacOS/Heimdall"), Some(owner)));
    }
}
