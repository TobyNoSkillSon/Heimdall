//! The general skills (docs/skills.md): every skill loads in every harness, the sets loaded together never share a
//! name, Heimdall's index names every task skill, and nothing of one person's is in them. One user's own skills live
//! in their data folder and are checked there (`heimdall --environment check`), never here.
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

/// Pi gets `public` plus `shared`; the T3 harnesses get `shared`. A general skill never takes the `my-` prefix, which
/// marks one user's additions to a general skill.
#[test]
fn sets_loaded_together_never_share_a_name() {
    let all = skills();
    let mut names: Vec<_> = all.iter().map(|s| s.name.as_str()).collect();
    let n = names.len();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), n, "public + shared repeat a name");
    for s in &all {
        assert!(["public", "shared"].contains(&s.set.as_str()), "unknown set {}", s.set);
        assert!(!s.name.starts_with("my-"), "{}/{}: `my-` names a user's own additions, which live in their private folder", s.set, s.name);
    }
}

/// Heimdall must know its tasks' skills. Its index names every task skill in the repository.
#[test]
fn heimdalls_index_names_every_task_skill() {
    let all = skills();
    let public = &all.iter().find(|s| s.set == "public" && s.name == "task-skills").expect("public/task-skills").text;
    for s in all.iter().filter(|s| s.set == "shared") {
        assert!(public.contains(&format!("- `{}`", s.name)), "public/task-skills misses `{}`", s.name);
    }
    // The owner's task skills and the video transcript tool the user named (14:59) are listed in his `my-task-skills`
    // (private_layer.rs).
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
        for private in ["Toby", "/Users/", "Vault", "Arsenal", "night pass", "Opus", "Haiku", "Discord", "GO INSTALL", "Allegro", "Polish", "shelf"] {
            // The speed skill's dated picks are the developer's opinion, said as such (scripts/check_public_split.py).
            if s.set == "public" && s.name == "speed-and-models" && ["Opus", "Haiku"].contains(&private) { continue; }
            // The voice tip names Arsenal for extracting and cleaning a sample, where it is on the Mac.
            if s.set == "public" && s.name == "onboarding" && private == "Arsenal" { continue; }
            // The call apps whose lines reach the agent are a fact of the app (`context_policy::CALL_BUNDLES`).
            if s.set == "public" && s.name == "about-heimdall" && private == "Discord" { continue; }
            // The bug-report skill names the repo on purpose (decision 18, 27 Oct; the public prompt did until 14 Nov);
            // nothing else may.
            let text = if s.name == "report-bug" { s.text.replace("`TobyNoSkillSon/Heimdall`", "") } else { s.text.clone() };
            assert!(!text.contains(private), "{}/{} carries {private:?}", s.set, s.name);
        }
    }
}

/// Routing belongs to editable skills in both editions; a model name alone is not private information.
#[test]
fn routing_editions_match_and_keep_access_precision_and_update_guards() {
    let all = skills();
    // One general `models` skill since 14 Nov: the personal copy's only differences (night fixes, the owner's name) are
    // the owner's `my-models` (private_layer.rs), so there is no second copy to compare.
    let public = &all.iter().find(|s| s.set == "public" && s.name == "models").unwrap().text;
    // The public edition has no night round (docs/personal/bugs.md), so its routing never names night fixes.
    assert!(!public.contains("night fixes"), "the general routing names night fixes");
    // Squashed: a pinned phrase may wrap differently in the one general file.
    let squash = |t: &str| t.split_whitespace().collect::<Vec<_>>().join(" ");
    for text in [&squash(public)] {
        // 14 Nov (the user): "the skills and instructions and system prompts are the generalized ones, they are not mine".
        // The general routing states the mechanism: the user's own choice first, then the providers they have, by
        // the kind of work. Which provider and which model is one user's own (`my-models`; private_layer.rs).
        for rule in ["Light work, research", "its lighter, cheaper model, at high effort", "Nothing here prefers a provider",
            "Visual work, design", "its strongest model, at medium effort", "Engineering, app code",
            "The user's own routing, in a `my-models` skill when there is one. It wins over everything below",
            "ask once, with one card, and write the answer into `my-models`",
            "not measured claims", "This covers delegates only", "actual authenticated",
            "never move to metered API billing they did not choose", "Never silently switch to metered APIs", "report that and ask",
            "One heavy GPU job at a time; check free memory before a model load",
            "without rebuilding the app", "Preserve local edits"] {
            assert!(text.contains(&squash(rule)), "routing misses {rule:?}");
        }
        assert!(text.contains("A model or provider the user names for this task. Use it"), "a named model wins");
        assert!(text.contains("Keep merges, integration, installs") && text.contains("never the light model for those"), "merges and installs stay on the strongest model");
        // No provider is prescribed or forbidden and no model is named (scripts/check_public_split.py holds the rule
        // for every general file; this is the skill it was written for).
        for own in ["Anthropic", "Opus", "Haiku", "Sonnet", "Fable", "claude-", "gpt-", "Never start a Codex", "fall back to Codex"] {
            assert!(!text.contains(own), "the general routing carries one user's choice: {own:?}");
        }
    }
    let public_briefs = &all.iter().find(|s| s.set == "public" && s.name == "writing-briefs").unwrap().text;
    assert!(public_briefs.contains("Read the editable `models` skill") && public_briefs.contains("include its precision rule explicitly in the brief"));
    assert!(!public_briefs.contains("leave the model to T3's default"));
    for prompt in [include_str!("../prompt/heimdall.md")] {
        assert!(prompt.contains("`models`") && prompt.contains("before choosing a target") || prompt.contains("read the editable `models` skill"));
        assert!(!prompt.contains("claude-haiku-5-5") && !prompt.contains("gpt-6.1-sol") && !prompt.contains("Opus") && !prompt.contains("Codex"));
    }
    let update = &all.iter().find(|s| s.set == "shared" && s.name == "update-skills").unwrap().text;
    for rule in ["baseline is unknown", "previously shipped default", "hash/version", "After EVERY app update",
        "release author's prepared changelog", "personalised assistant name", "Every public release",
        "metadata as authority", "not implemented"] {
        assert!(update.contains(rule), "update procedure misses {rule:?}");
    }
}

/// 28 Oct (the user 16:55): a complaint that Heimdall is slow loads one skill in both editions: quality first, today's
/// picks as dated judgement checked against current sources, and a Heimdall bench section. The picks themselves and
/// the bench's link may change; their shape stays.
#[test]
fn speed_and_models_ships_in_both_editions() {
    let all = skills();
    // One general copy since 14 Nov, compiled into both editions (the personal twin was identical, map-skills-twins.md).
    for set in ["public"] {
        let s = &all.iter().find(|s| s.set == set && s.name == "speed-and-models").unwrap_or_else(|| panic!("{set}/speed-and-models")).text;
        for part in ["slow", "fewest steps", "## Today's picks", "not a measurement", "check current sources", "never memory",
            "## Heimdall bench", "only when"] {
            assert!(s.contains(part), "{set}/speed-and-models misses {part:?}");
        }
        let dated = s.lines().any(|l| l.contains("not a measurement") && l.split_whitespace().any(|w| w.trim_end_matches([',', ':']).parse::<u16>().is_ok_and(|y| y >= 2026)));
        assert!(dated, "{set}/speed-and-models: the picks carry the date of the judgement");
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

/// Strategic friction is Heimdall's alone: "a skill only accessible to you, Heimdall, because your
/// role and the sub-agents' role are different". It lives in Heimdall's set, never where a T3 task loads skills, and
/// `writing-briefs` sends a substantive build's brief through it.
#[test]
fn purpose_check_is_heimdalls_alone() {
    // Since 14 Nov purpose-check is the owner's private Heimdall skill (private_layer.rs pins where it lives and what it
    // says); no general set carries it, and `skills/shared/` (where tasks load skills) least of all.
    assert!(!skills().iter().any(|s| s.name == "purpose-check"), "purpose-check is a private skill, never in skills/");
}

/// Phone skills Heimdall's own agent and its T3 tasks both use live in `skills/heimdall/` and `skills/workers/` as the
/// same file (Pi loads only its own set). Every verified phone capability gets a skill "you can
/// just do"; on 12 Nov `phone-voice` pointed Heimdall at `phone-senses`, which only the workers' set held, so away mode
/// and presence were missing from Heimdall's own skills. The two copies must never drift.
#[test]
fn phone_skills_are_the_same_file_for_heimdall_and_its_tasks() {
    // Since 14 Nov the phone is a personal-edition feature and its skills are the owner's private layer
    // (private_layer.rs keeps the two copies equal); no general skill carries one.
    for name in ["phone-control", "phone-senses", "phone-voice", "connect-phone"] {
        assert!(!skills().iter().any(|s| s.name == name), "{name} is a private skill, never in skills/");
    }
}
