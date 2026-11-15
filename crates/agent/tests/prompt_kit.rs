//! An evaluation kit: Heimdall's whole prompt as one profile's agent gets it, in files, so a fresh agent can be given
//! exactly that and asked what it would do (the proof runs of docs/skills.md, "Two layers"; any later prompt
//! evaluation). Nothing here starts Pi or touches a real profile.
//!
//!   HEIMDALL_PROMPT_KIT=<dir> cargo test -p agent --test prompt_kit -- --ignored
//!
//! `<dir>/Heimdall.app/Contents/Resources/` may hold `docs/` and `emergency.md` (the prompt then names them there);
//! `<dir>/data/` is the profile: put a `settings.json` and an `agent/private/` folder in it first to evaluate one
//! user's private layer. `HEIMDALL_PROMPT_KIT_USER` sets the user's name; `HEIMDALL_PROMPT_KIT_MAC=real` reads this
//! Mac instead of the made-up one. Written: `system-prompt.md`, `task-preamble.md`, `tools.json`.
use std::path::{Path, PathBuf};

/// A made-up Mac a stranger might have: enough to delegate (T3 Code and Claude Code found), nothing else.
fn made_up_mac(kit: &Path, data: &Path) -> agent::environment::Probe {
    let home = kit.join("home");
    std::fs::create_dir_all(home.join(".t3")).unwrap();
    std::fs::create_dir_all(home.join("bin")).unwrap();
    for tool in ["claude", "gh", "brew"] {
        std::fs::write(home.join("bin").join(tool), "").unwrap();
    }
    agent::environment::Probe {
        chip: Some("Apple M3".into()),
        memory_gb: Some(16),
        macos: Some("26.1".into()),
        heimdall_home: home.clone(),
        path: vec![home.join("bin")],
        apps: vec![home.join("Applications")],
        user_home: home,
        version: env!("CARGO_PKG_VERSION").into(),
        public: cfg!(feature = "public"),
        app: Some(kit.join("Heimdall.app")),
    }
    .tap(|_| std::fs::create_dir_all(data).unwrap())
}
trait Tap: Sized {
    fn tap(self, f: impl FnOnce(&Self)) -> Self {
        f(&self);
        self
    }
}
impl<T> Tap for T {}

#[test]
#[ignore = "writes an evaluation kit: HEIMDALL_PROMPT_KIT=<dir>"]
fn write_the_prompt_kit() {
    let kit = PathBuf::from(std::env::var("HEIMDALL_PROMPT_KIT").expect("HEIMDALL_PROMPT_KIT=<dir>"));
    let data = kit.join("data");
    let exe = kit.join("Heimdall.app/Contents/MacOS/heimdall");
    let user = std::env::var("HEIMDALL_PROMPT_KIT_USER").unwrap_or_else(|_| agent::identity::PUBLIC_USER.into());
    let names = agent::identity::Names { assistant: agent::identity::DEFAULT_ASSISTANT.into(), user };
    agent::identity::set(names.clone());
    let probe = if std::env::var("HEIMDALL_PROMPT_KIT_MAC").as_deref() == Ok("real") {
        std::fs::create_dir_all(&data).unwrap();
        agent::environment::Probe::read()
    } else {
        made_up_mac(&kit, &data)
    };
    // The profile's skills, as a start seeds them, and the two layers of the prompt.
    let agent_dir = data.join("agent/config");
    let [own, general] = agent::skills::folders(&agent_dir);
    let checkout = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../skills");
    agent::skills::seed(&checkout, &general).unwrap();
    let log = agent::tool_log::path(&data);
    let prompt = agent::harness::fill_prompt(agent::harness::PROMPT, &exe, Some(&log), Some(&data));
    let layer = agent::environment::refresh(&data, &probe).unwrap();
    let layer = agent::harness::fill_paths(&agent::identity::tokens(&layer, &names), &exe, Some(&log), Some(&data));
    let index = agent::relay_prompt::skills_block(&own, &general, &checkout).unwrap_or_default();
    let text = format!(
        "{}\n\n{}\n\nThe following skills provide specialized instructions for specific tasks.\nRead a skill's file when the task matches its description.\n\n{index}\n",
        prompt.trim_end(),
        layer.trim_end()
    );
    std::fs::write(kit.join("system-prompt.md"), &text).unwrap();
    std::fs::write(kit.join("task-preamble.md"), agent::harness::task_preamble(&data, &names)).unwrap();
    let tools = agent::identity::tokens(&serde_json::to_string_pretty(&agent::mcp_contract::tools()).unwrap(), &names);
    std::fs::write(kit.join("tools.json"), tools).unwrap();
    for placeholder in [agent::harness::PRIVATE_PLACEHOLDER, agent::harness::DOCS_PLACEHOLDER, agent::harness::MEMORY_PLACEHOLDER, agent::harness::CONNECTORS_PLACEHOLDER] {
        assert!(!text.contains(placeholder), "{placeholder} was left unfilled");
    }
    println!("{} words in {}", text.split_whitespace().count(), kit.join("system-prompt.md").display());
}
