//! The canonical skills folder (docs/skills.md): every skill loads in every harness, the sets loaded together never
//! share a name, Heimdall's index names every task skill, and the public edition's sets carry nothing of the user's.
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../skills")
}

struct Skill {
    set: String,
    dir: String,
    name: String,
    description: String,
    text: String,
}

fn skills() -> Vec<Skill> {
    let mut all = vec![];
    for set in std::fs::read_dir(root()).unwrap().flatten().filter(|e| e.path().is_dir()) {
        for skill in std::fs::read_dir(set.path()).unwrap().flatten().filter(|e| e.path().is_dir()) {
            let text = std::fs::read_to_string(skill.path().join("SKILL.md"))
                .unwrap_or_else(|_| panic!("{} has no SKILL.md", skill.path().display()));
            let front = text.strip_prefix("---\n").and_then(|t| t.split_once("\n---\n")).map(|(f, _)| f)
                .unwrap_or_else(|| panic!("{}: frontmatter", skill.path().display()));
            let field = |key: &str| front.lines().find_map(|l| l.strip_prefix(&format!("{key}: "))).unwrap_or("").to_owned();
            all.push(Skill {
                set: set.file_name().to_string_lossy().into(),
                dir: skill.file_name().to_string_lossy().into(),
                name: field("name"),
                description: field("description"),
                text,
            });
        }
    }
    assert!(all.len() >= 5, "skills folder looks empty");
    all
}

/// Pi, Claude Code, Codex, OpenCode and Grok all drop a skill whose frontmatter fails (Pi said nothing when a
/// description held ": ", 26 Oct): plain YAML scalars, the Agent Skills name rules, the folder's name.
#[test]
fn every_skill_has_frontmatter_every_harness_loads() {
    for s in skills() {
        let at = format!("{}/{}", s.set, s.dir);
        assert_eq!(s.name, s.dir, "{at}: name must equal the folder name");
        assert!(!s.name.is_empty() && s.name.len() <= 64 && s.name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            && !s.name.starts_with('-') && !s.name.ends_with('-') && !s.name.contains("--"), "{at}: name {:?}", s.name);
        assert!(!s.description.is_empty() && s.description.len() <= 1024, "{at}: description length {}", s.description.len());
        assert!(!s.description.contains(": ") && !s.description.contains(" #") && !s.description.starts_with(['"', '\'', '[', '{', '&', '*', '!', '|', '>', '%', '@', '`']),
            "{at}: the description must be a plain YAML scalar: {:?}", s.description);
        assert!(s.description.contains("Use "), "{at}: the description says when to use it");
    }
}

/// Each harness gets its edition's set plus `shared` (Pi), or `workers` plus `shared` (the T3 harnesses).
#[test]
fn sets_loaded_together_never_share_a_name() {
    let all = skills();
    for set in ["heimdall", "public", "workers"] {
        let mut names: Vec<_> = all.iter().filter(|s| s.set == set || s.set == "shared").map(|s| s.name.as_str()).collect();
        let n = names.len();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), n, "{set} + shared repeat a name");
    }
    for s in &all {
        assert!(["heimdall", "public", "workers", "shared"].contains(&s.set.as_str()), "unknown set {}", s.set);
    }
}

/// The personal sets (`heimdall`, `workers`) are not in the public source export; tests check them when present.
fn personal_sets_present() -> bool {
    root().join("heimdall").is_dir()
}

/// Heimdall must know its tasks' skills. Its index names every task skill in the repository.
#[test]
fn heimdalls_index_names_every_task_skill() {
    let all = skills();
    let index = |set: &str| all.iter().find(|s| s.set == set && s.name == "task-skills").unwrap_or_else(|| panic!("{set}/task-skills")).text.clone();
    let public = index("public");
    if personal_sets_present() {
        let personal = index("heimdall");
        for s in all.iter().filter(|s| s.set == "workers" || s.set == "shared") {
            assert!(personal.contains(&format!("- `{}`", s.name)), "heimdall/task-skills misses `{}`", s.name);
        }
        // The video transcript the user named (14:59) is a tool tasks have without a download.
        assert!(personal.contains("`youtube_transcript`"));
    }
    for s in all.iter().filter(|s| s.set == "shared") {
        assert!(public.contains(&format!("- `{}`", s.name)), "public/task-skills misses `{}`", s.name);
    }
}

/// The update-skills skill is the whole sync (no commands, no MCP): it names each harness's folder.
#[test]
fn update_skills_names_every_harness_folder_and_the_reload() {
    let all = skills();
    let s = &all.iter().find(|s| s.name == "update-skills" && s.set == "shared").expect("shared/update-skills").text;
    for part in ["~/Library/Application Support/Heimdall/agent/skills/", "~/.claude/skills/", "~/.agents/skills/", "OpenCode", "Grok CLI",
        "`reload_me`", "rsync -a --delete", "task-skills/SKILL.md", "cargo test -p agent --test prompt"] {
        assert!(s.contains(part), "update-skills misses {part:?}");
    }
    assert!(s.contains("No command, server or background job syncs them"));
}

/// The public edition ships `public/` and `shared/` (scripts/bundle.sh): nothing of the user's in them.
#[test]
fn public_and_shared_skills_carry_nothing_of_owners() {
    for s in skills().iter().filter(|s| s.set == "public" || s.set == "shared") {
        for private in ["/Users/", "Vault", "Arsenal", "night pass", "Opus", "Haiku", "Discord", "GO INSTALL", "Allegro", "Polish", "shelf"] {
            if s.set == "public" && ["models", "speed-and-models"].contains(&s.name.as_str()) && ["Opus", "Haiku"].contains(&private) { continue; }
            // The voice tip names Arsenal for extracting and cleaning a sample, where it is on the Mac.
            if s.set == "public" && s.name == "onboarding" && private == "Arsenal" { continue; }
            assert!(!s.text.contains(private), "{}/{} carries {private:?}", s.set, s.name);
        }
    }
}

/// Routing belongs to editable skills in both editions; a model name alone is not private information.
#[test]
fn routing_editions_match_and_keep_access_precision_and_update_guards() {
    let all = skills();
    let personal = all.iter().find(|s| s.set == "heimdall" && s.name == "models").map(|s| &s.text);
    assert_eq!(personal.is_some(), personal_sets_present(), "the personal set has a models skill");
    let public = &all.iter().find(|s| s.set == "public" && s.name == "models").unwrap().text;
    // The public edition has no night round (docs/bugs.md), so its routing never names night fixes.
    let squash = |t: &str| t.split_whitespace().collect::<Vec<_>>().join(" ");
    if let Some(personal) = personal {
        let personal_routing = squash(personal.split("\n## Arsenal").next().unwrap()).replace("When User names", "When the user names")
            .replace("Keep night fixes, merges", "Keep merges").replace("Haiku for night fixes, merges", "Haiku for merges");
        assert_eq!(personal_routing, squash(public));
    }
    for text in personal.into_iter().chain([public]) {
        // 27 Oct: delegates on the Anthropic subscription only; no Codex targets or fallback.
        for rule in ["Light work, research", "Claude Haiku 5.5, high effort", "Never start a Codex task, sub-agent, benchmark or review",
            "Visual work, design", "Claude Opus 5.5, medium effort", "Engineering, app code", "never\nfall back to Codex",
            "not measured claims", "covers delegates only", "actual authenticated",
            "no metered API fallback", "Never silently switch to metered APIs", "report that and ask",
            "Include this precision rule explicitly in every local-model brief", "big models keep their own", "no FP32 beyond weights of a few MB", "no converting quantized weights",
            "without rebuilding the app", "Preserve local edits"] {
            assert!(text.contains(rule), "routing misses {rule:?}");
        }
    }
    let public_briefs = &all.iter().find(|s| s.set == "public" && s.name == "writing-briefs").unwrap().text;
    assert!(public_briefs.contains("Read the editable `models` skill") && public_briefs.contains("include its precision rule explicitly in the brief"));
    assert!(!public_briefs.contains("leave the model to T3's default"));
    for prompt in [include_str!("../prompt/heimdall.md"), include_str!("../prompt/public/heimdall.md")] {
        assert!(prompt.contains("`models`") && prompt.contains("before choosing a target") || prompt.contains("read the editable `models` skill"));
        assert!(!prompt.contains("claude-haiku-5-5") && !prompt.contains("gpt-6.1-sol"));
    }
    let update = &all.iter().find(|s| s.set == "shared" && s.name == "update-skills").unwrap().text;
    for rule in ["baseline is unknown", "previously shipped default", "hash/version", "After EVERY app update",
        "release author's prepared changelog", "personalised assistant name", "Every public release",
        "metadata as authority", "not implemented"] {
        assert!(update.contains(rule), "update procedure misses {rule:?}");
    }
}

/// 28 Oct: a complaint that Heimdall is slow loads one skill in both editions: quality first, today's
/// picks as dated judgement checked against current sources, the bench's link a marked TODO.
#[test]
fn speed_and_models_ships_in_both_editions() {
    let all = skills();
    for set in ["heimdall", "public"].into_iter().filter(|set| *set == "public" || personal_sets_present()) {
        let s = &all.iter().find(|s| s.set == set && s.name == "speed-and-models").unwrap_or_else(|| panic!("{set}/speed-and-models")).text;
        for part in ["slow", "fewest steps", "Claude Opus 5.5", "GPT-6.1 Sol", "GPT-6 Luna", "28 Oct 2026", "not a measurement",
            "check current sources", "never memory", "Heimdall bench", "TODO", "only when"] {
            assert!(s.contains(part), "{set}/speed-and-models misses {part:?}");
        }
    }
}

/// The public edition bundles `skills/public/` (scripts/bundle.sh): a new user's tour ships there, built only from
/// what that edition has: its tools, the one bundled voice and the Own Voice Engine menu path (docs/voices.md).
#[test]
fn public_edition_ships_the_onboarding_tour() {
    let all = skills();
    let s = &all.iter().find(|s| s.set == "public" && s.name == "onboarding").expect("public/onboarding").text;
    let tools = agent::mcp_contract::tools().to_string();
    for tool in ["timeline_search", "look", "point", "schedule_wake", "delegate_task", "settings", "islands_view", "ask"] {
        assert!(s.contains(&format!("`{tool}`")) && tools.contains(&format!("\"name\":\"{tool}\"")), "onboarding names {tool}");
    }
    assert!(s.contains("Personalisation ▸ Model ▸ Add Provider ▸ Own Voice Engine…") && s.contains("which sounds better"));
    for gone in ["Drama", "Jarvis", "Supertonic", "ElevenLabs", "bug note", "Friends"] {
        assert!(!s.contains(gone), "onboarding offers {gone}, which the public edition lacks");
    }
}
