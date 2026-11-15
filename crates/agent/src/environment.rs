//! The private layer of Heimdall's prompt (docs/skills.md, "Two layers"). The general prompt (`harness::PROMPT`) is
//! the same text for every user and ships in the app; this is what one install has and what its user asked for, and it
//! lives only in that user's data folder, `<data>/agent/private/`:
//!
//! - `environment.md`: a block Heimdall rewrites at every Pi start from the Mac and its settings (`generated`), then
//!   the user's own setup notes and standing rules, which only the user and their Heimdall edit (`refresh` keeps them
//!   byte for byte). It is appended to the general prompt, for Pi and the relay alike (`layer`).
//! - `task-rules.md`: the user's own rules for the tasks Heimdall starts, put under the general task preamble
//!   (`task_rules`).
//! - `skills/`: the user's own skills, loaded before the general ones (`crate::skills::private`).
//!
//! Nothing here is in the repository's shipping paths or the app bundle, so a release cannot carry it. Every fact in
//! the block is read from a file's existence, `sysctl` or settings.json: no Keychain read, no network, no permission
//! check, nothing that can raise a prompt.
use std::path::{Path, PathBuf};

use serde_json::Value;

/// The private folder of the profile whose data folder is `data`.
pub fn dir(data: &Path) -> PathBuf {
    data.join("agent").join("private")
}
/// The environment and rules file appended to the general prompt.
pub fn file(data: &Path) -> PathBuf {
    dir(data).join("environment.md")
}
/// The user's own skills, loaded before the general ones.
pub fn skills_dir(data: &Path) -> PathBuf {
    dir(data).join("skills")
}
/// The user's own rules for tasks, or "" when there are none.
pub fn task_rules(data: &Path) -> String {
    std::fs::read_to_string(dir(data).join("task-rules.md")).map(|t| t.trim().to_owned()).unwrap_or_default()
}

/// The generated block's delimiters. Everything between them is rewritten at each start.
pub const START: &str = "<!-- heimdall:environment:start -->";
pub const END: &str = "<!-- heimdall:environment:end -->";
/// The layer's heading, as the general prompt names it.
pub const HEADING: &str = "# This install";
/// The heading the user's own part starts with in a new file.
pub const RULES_HEADING: &str = "## Setup notes and standing rules";
/// A soft cap on the whole layer, in words: `check` reports a longer one. Standing rules that outgrow it belong in a
/// private skill, which is read only when its topic comes up.
pub const WORD_CAP: usize = 2000;

/// What the generator reads besides settings.json. A test fills it by hand; `Probe::read` reads the real Mac.
#[derive(Clone, Debug, Default)]
pub struct Probe {
    /// "Apple M5 Max"
    pub chip: Option<String>,
    pub memory_gb: Option<u64>,
    /// "27.0"
    pub macos: Option<String>,
    /// The user's own home folder (`$HOME`): where T3 Code and per-user apps are looked for.
    pub user_home: PathBuf,
    /// Heimdall's home folder, the agent's working folder (docs/home.md).
    pub heimdall_home: PathBuf,
    /// Folders searched for command-line tools.
    pub path: Vec<PathBuf>,
    /// Folders searched for apps.
    pub apps: Vec<PathBuf>,
    pub version: String,
    pub public: bool,
    /// The installed app (`…/Heimdall.app`), when this process runs from a bundle: where its command-line tools are.
    pub app: Option<PathBuf>,
}

fn command_line(program: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(program).args(args).stdin(std::process::Stdio::null()).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (out.status.success() && !text.is_empty()).then_some(text)
}

impl Probe {
    /// The real Mac: three quick local commands and the environment, nothing else.
    pub fn read() -> Self {
        let user_home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        let mut path: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
        for extra in ["/opt/homebrew/bin", "/usr/local/bin"] {
            path.push(extra.into());
        }
        for extra in [".local/bin", ".claude/local", ".cargo/bin", ".bun/bin", ".npm-global/bin", ".opencode/bin", ".grok/bin"] {
            path.push(user_home.join(extra));
        }
        Self {
            chip: command_line("/usr/sbin/sysctl", &["-n", "machdep.cpu.brand_string"]),
            memory_gb: command_line("/usr/sbin/sysctl", &["-n", "hw.memsize"]).and_then(|b| b.parse::<u64>().ok()).map(|b| b >> 30),
            macos: command_line("/usr/bin/sw_vers", &["-productVersion"]),
            apps: vec!["/Applications".into(), user_home.join("Applications")],
            heimdall_home: heimdall_home::home_dir(),
            user_home,
            path,
            version: env!("CARGO_PKG_VERSION").into(),
            public: cfg!(feature = "public"),
            app: std::env::current_exe().ok().filter(|exe| crate::runtime::in_app_bundle(exe))
                .and_then(|exe| exe.parent().and_then(Path::parent).and_then(Path::parent).map(Path::to_path_buf)),
        }
    }
    fn command(&self, name: &str) -> Option<PathBuf> {
        self.path.iter().map(|d| d.join(name)).find(|p| p.is_file())
    }
    fn has_app(&self, name: &str) -> bool {
        self.apps.iter().any(|d| d.join(format!("{name}.app")).exists())
    }
}

/// "a, b and c".
fn list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}
fn names(value: &Value) -> Vec<String> {
    value.as_array().map(|a| a.iter().filter_map(|v| v["name"].as_str().map(str::to_owned)).collect()).unwrap_or_default()
}
fn model(value: &Value) -> Option<String> {
    let (provider, id) = (value["provider"].as_str()?, value["id"].as_str()?);
    (!provider.is_empty() && !id.is_empty()).then(|| format!("`{id}` through `{provider}`"))
}

/// The generated block, without its markers: one line per fact, each read from `probe`, `settings` (settings.json; a
/// missing key leaves its line out or says "not set", never a guess) and the folders under `data`. Present things and
/// absent things are both said, so the agent knows what it cannot use here.
pub fn generated(probe: &Probe, settings: &Value, data: &Path) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut mac: Vec<String> = Vec::new();
    mac.extend(probe.chip.clone());
    mac.extend(probe.memory_gb.map(|g| format!("{g} GB of memory")));
    mac.extend(probe.macos.as_ref().map(|v| format!("macOS {v}")));
    if !mac.is_empty() {
        lines.push(format!("Mac: {}.", mac.join(", ")));
    }
    let app = probe.app.as_ref().map(|a| format!(", installed at `{}` (its command-line tools are in `Contents/MacOS/`)", a.display())).unwrap_or_default();
    lines.push(format!(
        "Heimdall: the {} edition, version {}{app}. Home folder (your working folder) `{}`; Heimdall's data folder `{}`.",
        if probe.public { "public" } else { "personal" },
        probe.version,
        probe.heimdall_home.display(),
        data.display()
    ));
    // The agent's own model: the typed value from settings.json, or the build's default when the file has none.
    let chosen = match &settings["agent_model"] {
        // A value equal to the build's default is not written to the file.
        Value::Null => Ok(crate::provider::AgentModel::default()),
        saved => serde_json::from_value::<crate::provider::AgentModel>(saved.clone()),
    };
    let own = match chosen {
        Ok(crate::provider::AgentModel::Api { provider, id }) => format!("`{id}` through `{provider}`"),
        Ok(crate::provider::AgentModel::Local) => "the local engine on this Mac".into(),
        Ok(crate::provider::AgentModel::Unchosen) => "none chosen yet (the user picks one in Setup or the menu)".into(),
        Err(_) => "not readable from settings (`settings` get `agent_model` says)".into(),
    };
    if settings["agent_relay"].as_bool() == Some(true) {
        let relay = settings["agent_relay_model"].as_str().map(|m| format!(" on `{m}`")).unwrap_or_default();
        let account = settings["agent_relay_account"].as_str().map(|a| format!(", provider instance `{a}`")).unwrap_or_default();
        lines.push(format!("Your model: you run as a T3 Code thread (relay mode){relay}{account}. Without the relay, your model is {own}."));
    } else {
        lines.push(format!("Your model: {own}."));
    }
    let mut providers: Vec<String> = settings["api_models"].as_array().map(|a| a.iter().filter_map(|m| m["provider"].as_str().map(str::to_owned)).collect()).unwrap_or_default();
    providers.sort();
    providers.dedup();
    if !providers.is_empty() {
        lines.push(format!("Providers with models added: {}.", list(&providers.iter().map(|p| format!("`{p}`")).collect::<Vec<_>>())));
    }
    let own_models = settings["task_models"].as_array().is_some_and(|a| !a.is_empty());
    match model(&settings["background_model"]) {
        Some(m) => lines.push(format!("Background jobs (suggestions, plan lists, goals, voice naming) run on {m}, unless a job has its own model or is off.")),
        None if probe.public && !own_models => lines.push("Background jobs (suggestions, plan lists, goals, voice naming): no model chosen, so none of them run.".into()),
        None => {}
    }
    if settings["engine_on"].as_bool() == Some(true) {
        let engine = settings["engine_model"].as_str().map(|m| format!(" (`{m}`)")).unwrap_or_default();
        lines.push(format!("The local engine is on{engine}."));
    }
    if let Some(mic) = settings["microphone"]["name"].as_str() {
        let extra = names(&settings["extra_microphones"]);
        let also = if extra.is_empty() { String::new() } else { format!("; also listening on {}", list(&extra)) };
        lines.push(format!("Microphone at start: {mic}{also}."));
    }
    let apps = names(&settings["recorded_apps"]);
    lines.push(if apps.is_empty() {
        "App audio: no app is recorded, so you hear only the microphone.".into()
    } else {
        format!("App audio recorded: {}. Lines from call apps among them (Discord, Slack, Zoom, Teams, FaceTime) reach your transcript; the others go only to the archive.", list(&apps))
    });
    lines.push(if settings["computer_control"].as_bool() == Some(true) {
        "Control Apps was on at start: the `computer` tool can act in other apps while Accessibility is granted.".into()
    } else {
        "Control Apps was off at start: the `computer` tool answers COMPUTER_OFF until the user turns it on in Heimdall's menu.".into()
    });
    // What work can be handed to. T3 Code keeps its files in `~/.t3` once it has run; the public edition also needs
    // the user's Connect in Setup (`t3_connect_allowed`), which the personal edition does not ask for.
    let connected = !probe.public || settings["t3_connect_allowed"].as_bool() == Some(true);
    lines.push(match (probe.user_home.join(".t3").is_dir(), connected) {
        (true, true) => "T3 Code has run on this Mac and is connected: `delegate_task` and the T3 tools work while it is open.".into(),
        (true, false) => "T3 Code has run on this Mac but is not connected to you: `delegate_task` and the T3 tools answer T3_UNAVAILABLE until the user connects it in Heimdall's Setup.".into(),
        (false, _) => "T3 Code: no sign of it on this Mac. `delegate_task` and the T3 tools answer T3_UNAVAILABLE until the user installs it, opens it and connects it in Heimdall's Setup; until then you can only do what your own tools do.".into(),
    });
    let clis = [("claude", "Claude Code"), ("codex", "Codex"), ("opencode", "OpenCode"), ("grok", "Grok"), ("gh", "GitHub CLI (`gh`)"), ("brew", "Homebrew")];
    let found: Vec<String> = clis.iter().filter_map(|(bin, name)| probe.command(bin).map(|p| format!("{name} at `{}`", p.display()))).collect();
    let missing: Vec<String> = clis.iter().filter(|(bin, _)| probe.command(bin).is_none()).map(|(_, name)| name.to_string()).collect();
    if !found.is_empty() {
        lines.push(format!("Command-line tools found: {}.", list(&found)));
    }
    if !missing.is_empty() {
        lines.push(format!("Not found in the usual folders (ask before assuming, a tool may live elsewhere): {}.", list(&missing)));
    }
    let arsenal = match settings["arsenal_mcp"].as_str() {
        Some(path) if Path::new(path).exists() => format!("Arsenal (local models) is registered as the `arsenal` MCP server (`{path}`)"),
        Some(path) => format!("Arsenal (local models) was set in Setup but is missing at `{path}`"),
        None if probe.has_app("Arsenal") => "Arsenal.app (local models) is in an Applications folder (not registered as an MCP server of yours)".into(),
        None => "no Arsenal.app (local models) in the Applications folders".into(),
    };
    let vella = if probe.has_app("Vella") { "Vella.app (dictation) is in an Applications folder" } else { "no Vella.app (dictation) in the Applications folders" };
    lines.push(format!("Apps that work with you: {vella}; {arsenal}."));
    // The phone bridge is compiled only into the personal edition; whether a phone is paired is the bridge's to say.
    if !probe.public {
        lines.push("Phone: this build has the phone bridge (the phone skills say what a paired phone can do).".into());
    }
    let own: Vec<String> = crate::skill_index::scan(&[&skills_dir(data)]).into_iter().map(|s| format!("`{}`", s.name)).collect();
    lines.push(if own.is_empty() {
        format!("Your user's own skills: none yet (they go in `{}`).", skills_dir(data).display())
    } else {
        format!("Your user's own skills, in `{}`: {}.", skills_dir(data).display(), list(&own))
    });
    lines.iter().map(|l| format!("- {l}")).collect::<Vec<_>>().join("\n")
}

/// A new file's text around the block: what the file is, and the empty part the user and their Heimdall fill.
fn fresh(block: &str) -> String {
    format!(
        "{HEADING}\n\nWhat this Mac and this user have. Heimdall rewrites the list between the two markers at every \
start from the Mac and its settings: a fact there is as of your last start, and `settings` gives the value now. \
Everything under the second marker is the user's own and is never rewritten.\n\n{START}\n{block}\n{END}\n\n\
{RULES_HEADING}\n\nNothing yet. When the user tells you something lasting about their setup (an account, a \
subscription, an app they use, a project) or gives you a standing rule, write it here in their words with the date, in place of this paragraph.\n"
    )
}

/// `text` with the block between the markers replaced by `block`. A file without both markers, in order, keeps all of
/// its text under a new block: nothing a user wrote is ever dropped.
pub fn with_block(text: &str, block: &str) -> String {
    match (text.find(START), text.find(END)) {
        (Some(start), Some(end)) if start < end => format!("{}{START}\n{block}\n{}", &text[..start], &text[end..]),
        _ if text.trim().is_empty() => fresh(block),
        _ => format!("{HEADING}\n\n{START}\n{block}\n{END}\n\n{}", text.trim_start()),
    }
}

/// Brings `<data>/agent/private/environment.md` up to date and returns it: the block rewritten from `probe` and
/// settings.json, the rest untouched. Creates the folder, the file and the skills folder on a first run. The file is
/// written only when its text changed.
pub fn refresh(data: &Path, probe: &Probe) -> anyhow::Result<String> {
    use anyhow::Context;
    std::fs::create_dir_all(skills_dir(data)).with_context(|| format!("create {}", skills_dir(data).display()))?;
    let settings: Value = std::fs::read(data.join("settings.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);
    let path = file(data);
    let old = std::fs::read_to_string(&path).unwrap_or_default();
    let new = with_block(&old, &generated(probe, &settings, data));
    if new != old {
        crate::runtime::atomic_write(&path, new.as_bytes())?;
    }
    Ok(new)
}

/// The private layer as it goes under the general prompt: the refreshed file. When the file cannot be written (a
/// read-only data folder), the block alone, so the agent still knows its environment.
pub fn layer(data: &Path) -> String {
    let probe = Probe::read();
    match refresh(data, &probe) {
        Ok(text) => text.trim_end().to_owned(),
        Err(e) => {
            eprintln!("The private layer was not written: {e:#}");
            let settings: Value = std::fs::read(data.join("settings.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);
            format!("{HEADING}\n\n{START}\n{}\n{END}", generated(&probe, &settings, data))
        }
    }
}

/// `prompt` (the general prompt, already filled) with the private layer of the profile in `data` under it.
pub fn under(prompt: &str, data: &Path) -> String {
    format!("{}\n\n{}\n", prompt.trim_end(), layer(data))
}

/// What a weekly check reads (`Heimdall --environment check`): one line per finding, empty when the layer is sound.
/// It never changes a file.
pub fn check(data: &Path, probe: &Probe) -> Vec<String> {
    let mut findings = Vec::new();
    let path = file(data);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return vec![format!("{} does not exist yet; it is written at the next start.", path.display())];
    };
    let settings: Value = std::fs::read(data.join("settings.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);
    if with_block(&text, &generated(probe, &settings, data)) != text {
        findings.push("The generated block is older than the Mac's state now; it is rewritten at the next start or reload.".into());
    }
    let words = text.split_whitespace().count();
    if words > WORD_CAP {
        findings.push(format!("The layer is {words} words, over the {WORD_CAP}-word cap: move rules for one topic into a private skill."));
    }
    let own = match text.find(END) {
        Some(end) => &text[end + END.len()..],
        None => text.as_str(),
    };
    // A path the user's notes name that is no longer there is a stale note.
    for token in own.split(|c: char| c == '`' || c.is_whitespace()) {
        let token = token.trim_end_matches(['.', ',', ')', ';', ':']);
        if token.starts_with('/') && token.len() > 3 && !token.contains('<') && !Path::new(token).exists() {
            findings.push(format!("A path in the notes is not on this Mac: {token}"));
        }
    }
    // Private skills: each declares its class, and none shadows a general skill by accident.
    let general = crate::skills::folder(&data.join("agent/config"));
    for skill in crate::skill_index::scan(&[&skills_dir(data)]) {
        if skill.class != Some(crate::skill_index::Class::Private) {
            findings.push(format!("Private skill `{}` does not declare `class: private` under `metadata`.", skill.name));
        }
        if general.join(&skill.name).join("SKILL.md").is_file() {
            findings.push(format!("Private skill `{}` replaces the general skill of that name; additions belong in `my-{}`.", skill.name, skill.name));
        }
    }
    for skill in crate::skill_index::scan(&[&general]) {
        if skill.class == Some(crate::skill_index::Class::Private) {
            findings.push(format!("`{}` in the general skills folder declares `class: private`; move it to {}.", skill.name, skills_dir(data).display()));
        }
    }
    findings
}

/// `Heimdall --environment [show|check|refresh]` for the profile in `data`: the layer as the agent gets it, the check's
/// findings, or the refreshed file's path.
pub fn cli(args: &[String], data: &Path) -> anyhow::Result<String> {
    let probe = Probe::read();
    match args.first().map(String::as_str) {
        None | Some("show") => {
            let settings: Value = std::fs::read(data.join("settings.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);
            Ok(with_block(&std::fs::read_to_string(file(data)).unwrap_or_default(), &generated(&probe, &settings, data)))
        }
        Some("check") => {
            let findings = check(data, &probe);
            Ok(if findings.is_empty() { format!("The private layer in {} is sound.", dir(data).display()) } else { findings.join("\n") })
        }
        Some("refresh") => {
            refresh(data, &probe)?;
            Ok(file(data).display().to_string())
        }
        Some(other) => anyhow::bail!("unknown --environment command {other:?}: show, check or refresh"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn probe(root: &Path) -> Probe {
        Probe {
            chip: Some("Apple M9".into()),
            memory_gb: Some(32),
            macos: Some("27.0".into()),
            user_home: root.join("home"),
            heimdall_home: root.join("home/Notes"),
            path: vec![root.join("bin")],
            apps: vec![root.join("Applications")],
            version: "1.2.3".into(),
            public: true,
            app: None,
        }
    }

    /// The model line of a profile whose settings.json names no model: the build's default.
    const MODEL_DEFAULT: &str = if cfg!(feature = "public") { "Your model: none chosen yet (the user picks one in Setup or the menu)." } else { "Your model: `claude-haiku-5-5` through `opencode-go`." };

    #[test]
    fn the_block_says_what_is_there_and_what_is_not() {
        let d = tempfile::tempdir().unwrap();
        let (root, data) = (d.path(), d.path().join("data"));
        let bare = generated(&probe(root), &Value::Null, &data);
        for line in ["- Mac: Apple M9, 32 GB of memory, macOS 27.0.", "the public edition, version 1.2.3", MODEL_DEFAULT,
            "no model chosen, so none of them run", "no app is recorded", "Control Apps was off at start", "T3 Code: no sign of it on this Mac",
            "Not found in the usual folders (ask before assuming, a tool may live elsewhere): Claude Code, Codex, OpenCode, Grok, GitHub CLI (`gh`) and Homebrew.",
            "no Vella.app (dictation) in the Applications folders; no Arsenal.app (local models) in the Applications folders", "Your user's own skills: none yet"] {
            assert!(bare.contains(line), "missing {line:?} in:\n{bare}");
        }
        assert!(!bare.contains("Phone:"), "the public edition has no phone app to describe");
        // The same Mac with everything set up.
        std::fs::create_dir_all(root.join("home/.t3")).unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::create_dir_all(root.join("Applications/Vella.app")).unwrap();
        for bin in ["claude", "gh"] {
            std::fs::write(root.join("bin").join(bin), "").unwrap();
        }
        let skill = skills_dir(&data).join("my-models");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), "---\nname: my-models\ndescription: Mine. Use always.\nmetadata:\n  class: private\n---\n").unwrap();
        let settings = json!({"agent_model": {"api": {"provider": "anthropic", "id": "claude-x"}}, "agent_relay": true, "agent_relay_model": "claude-y",
            "agent_relay_account": "claudeAgent", "api_models": [{"provider": "openai", "id": "a"}, {"provider": "anthropic", "id": "b"}, {"provider": "openai", "id": "c"}],
            "background_model": {"provider": "anthropic", "id": "small"}, "engine_on": true, "engine_model": "q",
            "microphone": {"name": "Desk Mic"}, "extra_microphones": [{"name": "AirPods"}], "recorded_apps": [{"name": "Zoom"}, {"name": "Safari"}],
            "computer_control": true, "arsenal_mcp": "/A/arsenal"});
        let mut personal = probe(root);
        personal.public = false;
        personal.app = Some("/Applications/Heimdall.app".into());
        std::fs::create_dir_all(&data).unwrap();
        // The public edition waits for the user's Connect in Setup; the personal one does not ask.
        let mut waiting = probe(root);
        waiting.public = true;
        assert!(generated(&waiting, &settings, &data).contains("T3 Code has run on this Mac but is not connected to you"));
        let full = generated(&personal, &settings, &data);
        for line in ["you run as a T3 Code thread (relay mode) on `claude-y`, provider instance `claudeAgent`. Without the relay, your model is `claude-x` through `anthropic`.",
            "Providers with models added: `anthropic` and `openai`.", "run on `small` through `anthropic`, unless a job has its own model or is off", "The local engine is on (`q`).",
            "Microphone at start: Desk Mic; also listening on AirPods.", "App audio recorded: Zoom and Safari. Lines from call apps among them", "Control Apps was on at start",
            "T3 Code has run on this Mac and is connected", &format!("Command-line tools found: Claude Code at `{}` and GitHub CLI (`gh`) at `{}`.", root.join("bin/claude").display(), root.join("bin/gh").display()),
            "Vella.app (dictation) is in an Applications folder; Arsenal (local models) was set in Setup but is missing at `/A/arsenal`.",
            "Phone: this build has the phone bridge", "own skills, in", "`my-models`",
            "the personal edition, version 1.2.3, installed at `/Applications/Heimdall.app` (its command-line tools are in `Contents/MacOS/`). Home folder"] {
            assert!(full.contains(line), "missing {line:?} in:\n{full}");
        }
        assert!(full.lines().all(|l| l.starts_with("- ")));
    }

    #[test]
    fn a_refresh_rewrites_only_the_block_and_keeps_every_word_of_the_users() {
        let d = tempfile::tempdir().unwrap();
        let data = d.path().join("data");
        let first = refresh(&data, &probe(d.path())).unwrap();
        assert!(first.starts_with(HEADING) && first.contains(START) && first.contains(END) && first.contains(RULES_HEADING));
        assert!(skills_dir(&data).is_dir(), "the user's skills folder is there from the first start");
        // The user's Heimdall writes a rule under the block; the Mac changes; the rule stays and the block follows.
        let rule = "\n- Never buy citrus beer (said 3 Nov).\n";
        std::fs::write(file(&data), format!("{first}{rule}")).unwrap();
        let mut later = probe(d.path());
        later.memory_gb = Some(64);
        let second = refresh(&data, &later).unwrap();
        assert!(second.ends_with(rule) && second.contains("64 GB of memory") && !second.contains("32 GB"));
        assert_eq!(second.matches(START).count(), 1);
        // Unchanged state writes nothing.
        let before = std::fs::metadata(file(&data)).unwrap().modified().unwrap();
        assert_eq!(refresh(&data, &later).unwrap(), second);
        assert_eq!(std::fs::metadata(file(&data)).unwrap().modified().unwrap(), before);
        // A hand-made file without markers keeps all of its text under a new block.
        std::fs::write(file(&data), "My rules.\n\n- One.\n").unwrap();
        let adopted = refresh(&data, &later).unwrap();
        assert!(adopted.starts_with(HEADING) && adopted.ends_with("My rules.\n\n- One.\n") && adopted.contains(END));
        // Markers in the wrong order are not a block: nothing between them is thrown away.
        std::fs::write(file(&data), format!("{END}\nkept\n{START}\n")).unwrap();
        assert!(refresh(&data, &later).unwrap().contains("\nkept\n"));
    }

    #[test]
    fn the_layer_goes_under_the_general_prompt_and_task_rules_are_read() {
        let d = tempfile::tempdir().unwrap();
        let data = d.path().join("data");
        assert_eq!(task_rules(&data), "");
        let text = under("# Heimdall\n\nGeneral.\n", &data);
        assert!(text.starts_with("# Heimdall\n\nGeneral.\n\n# This install\n") && text.ends_with('\n') && text.contains(START));
        std::fs::write(dir(&data).join("task-rules.md"), "\n- Tasks use the main browser window.\n\n").unwrap();
        assert_eq!(task_rules(&data), "- Tasks use the main browser window.");
    }

    #[test]
    fn the_check_finds_a_stale_block_dead_paths_long_layers_and_misfiled_skills() {
        let d = tempfile::tempdir().unwrap();
        let data = d.path().join("data");
        let p = probe(d.path());
        assert!(check(&data, &p)[0].contains("does not exist yet"));
        refresh(&data, &p).unwrap();
        assert!(check(&data, &p).is_empty(), "{:?}", check(&data, &p));
        let mut text = std::fs::read_to_string(file(&data)).unwrap();
        text.push_str(&format!("\n- My project is in `{}/gone`.\n{}\n", d.path().display(), "word ".repeat(WORD_CAP)));
        std::fs::write(file(&data), text).unwrap();
        let skill = |root: &Path, name: &str, class: &str| {
            std::fs::create_dir_all(root.join(name)).unwrap();
            std::fs::write(root.join(name).join("SKILL.md"), format!("---\nname: {name}\ndescription: X. Use it.\nmetadata:\n  class: {class}\n---\n")).unwrap();
        };
        let general = crate::skills::folder(&data.join("agent/config"));
        skill(&general, "models", "public");
        skill(&general, "misfiled", "private");
        skill(&skills_dir(&data), "models", "private");
        skill(&skills_dir(&data), "unmarked", "public");
        let mut later = p.clone();
        later.macos = Some("28.0".into());
        let found = check(&data, &later).join("\n");
        for part in ["older than the Mac's state", "over the 2000-word cap", "/gone", "`unmarked` does not declare `class: private`",
            "`models` replaces the general skill", "`misfiled` in the general skills folder declares `class: private`"] {
            assert!(found.contains(part), "missing {part:?} in:\n{found}");
        }
    }
}
