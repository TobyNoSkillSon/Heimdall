//! The relay's system prompt (docs/claude-relay.md): the T3 thread acting as Heimdall's agent gets Heimdall's own prompt
//! inline in its first message, since T3 has no system-prompt parameter. `build.rs` composes it per edition from one
//! source of truth: `prompt/relay.md` (what differs in a T3 thread), Heimdall's prompt, and the bundled skills index
//! (`skill_index`). At runtime it gets the same fills as Pi's prompt (`harness::fill_prompt`), the profile's own skills
//! index, and the CLI line. `prompt/relay.generated.md` is the checked-in copy; a test fails when it is stale.
use crate::skill_index;
use std::path::Path;

/// The composed template for this edition (`build.rs`).
#[cfg(not(feature = "public"))]
pub const TEMPLATE: &str = include_str!(concat!(env!("OUT_DIR"), "/relay.md"));
#[cfg(feature = "public")]
pub const TEMPLATE: &str = include_str!(concat!(env!("OUT_DIR"), "/relay-public.md"));
/// The relay section's name for the CLI line (`Emergency::cli_for`).
pub const CLI_PLACEHOLDER: &str = "HEIMDALL_RELAY_CLI";
/// The command that rewrites the checked-in snapshot after a prompt, skill or relay.md change.
pub const REGENERATE: &str = "HEIMDALL_WRITE_RELAY_PROMPT=1 cargo test -p agent relay_prompt";

/// The skills index the relay should see: the profile's skill folder when it exists (what Pi loads), else the
/// bundled sets read now with their real locations; None when neither has a skill.
pub fn skills_block(profile: &Path, bundled: &Path) -> Option<String> {
    if profile.is_dir() {
        let skills = skill_index::scan(&[profile]);
        return (!skills.is_empty()).then(|| skill_index::render(&skills, &profile.display().to_string()));
    }
    let mut skills: Vec<skill_index::Skill> = Vec::new();
    for set in crate::skills::sets(bundled) {
        let name = set.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        for mut s in skill_index::scan(&[&set]) {
            if !skills.iter().any(|k| k.name == s.name) {
                s.folder = format!("{name}/{}", s.folder);
                skills.push(s);
            }
        }
    }
    (!skills.is_empty()).then(|| skill_index::render(&skills, &bundled.display().to_string()))
}

/// `prompt` with its skills index replaced by `block`.
fn with_index(prompt: &str, block: &str) -> String {
    match (prompt.rfind(skill_index::START), prompt.rfind(skill_index::END)) {
        (Some(start), Some(end)) if start < end => {
            format!("{}{block}{}", &prompt[..start], &prompt[end + skill_index::END.len()..])
        }
        _ => prompt.to_string(),
    }
}

/// The relay prompt for the profile in `data`, as the T3 thread gets it: `TEMPLATE` with Pi's runtime fills, the
/// profile's skills index (`skills_block`), and `cli` for the CLI line.
pub fn text(exe: &Path, data: &Path, cli: &str) -> String {
    let skills = crate::skills::folder(&data.join("agent/config"));
    // Heimdall's prompt is filled exactly as Pi's (the names note after its opening paragraph, `identity::fill`); the
    // relay section above it gets the names only (`skill_index::compose`'s separator splits them).
    let fill = |t: &str| crate::harness::fill_prompt(t, exe, Some(&crate::tool_log::path(data)), Some(data));
    let filled = match TEMPLATE.split_once(skill_index::SEPARATOR) {
        Some((relay, prompt)) => format!("{}{}{}", crate::identity::names_only(relay, &crate::identity::current()), skill_index::SEPARATOR, fill(prompt)),
        None => fill(TEMPLATE),
    };
    let filled = match skills_block(&skills, &crate::skills::bundled(exe)) {
        Some(block) => with_index(&filled, &block),
        None => filled,
    };
    filled.replace(skill_index::SKILLS_DIR, &skills.display().to_string()).replace(CLI_PLACEHOLDER, cli)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(dir: &Path, name: &str, description: &str) {
        std::fs::create_dir_all(dir.join(name)).unwrap();
        std::fs::write(dir.join(name).join("SKILL.md"), format!("---\nname: {name}\ndescription: {description}\n---\n\nBody.\n")).unwrap();
    }

    /// The staleness guard: the checked-in snapshot is what the sources compose to now.
    #[test]
    fn relay_prompt_snapshot_matches_heimdall_s_prompt_the_skills_and_relay_md() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let public = cfg!(feature = "public");
        let now = skill_index::compose_from(dir, public).unwrap();
        assert_eq!(now, TEMPLATE, "build.rs and the sources disagree; rebuild");
        let snapshot = dir.join(skill_index::snapshot(public));
        if std::env::var_os("HEIMDALL_WRITE_RELAY_PROMPT").is_some() {
            std::fs::write(&snapshot, &now).unwrap();
        }
        let saved = std::fs::read_to_string(&snapshot).unwrap_or_default();
        let edition = if public { " --features public" } else { "" };
        assert!(saved == now, "{} is stale: Heimdall's prompt, a bundled skill or relay.md changed. Regenerate it with:\n  {REGENERATE}{edition}",
            snapshot.display());
    }

    #[test]
    fn the_template_holds_the_relay_section_heimdall_s_prompt_and_every_bundled_skill() {
        assert!(TEMPLATE.contains(CLI_PLACEHOLDER) && TEMPLATE.contains(crate::harness::PROMPT.trim_end()));
        let bundled = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../skills");
        for set in crate::skills::sets(&bundled) {
            for s in skill_index::scan(&[&set]) {
                assert!(TEMPLATE.contains(&format!("<location>{}/{}/SKILL.md</location>", skill_index::SKILLS_DIR, s.folder)), "{}", s.name);
            }
        }
        assert!(!TEMPLATE.contains("<name>git-workflow</name>"), "worker skills are not Heimdall's");
    }

    #[test]
    fn the_profile_s_skill_folder_is_the_index_when_it_exists_else_the_bundled_sets() {
        let d = tempfile::tempdir().unwrap();
        let bundled = d.path().join("bundle");
        skill(&bundled.join(crate::skills::EDITION_SET), "models", "Pick a model.");
        skill(&bundled.join(crate::skills::SHARED_SET), "update-skills", "Change a skill.");
        skill(&bundled.join("workers"), "git-workflow", "Not Heimdall's.");
        let profile = d.path().join("agent/skills");
        let block = skills_block(&profile, &bundled).unwrap();
        assert!(block.contains(&format!("<location>{}/{}/models/SKILL.md</location>", bundled.display(), crate::skills::EDITION_SET)), "{block}");
        assert!(block.contains(&format!("<location>{}/shared/update-skills/SKILL.md</location>", bundled.display())) && !block.contains("git-workflow"));
        // Pi's folder, with an edited and an added skill, a removed one, a hidden one and a half copy.
        skill(&profile, "models", "Edited: <pick> & go.");
        skill(&profile, "my-own", "Added by an agent.");
        std::fs::create_dir_all(profile.join("hidden")).unwrap();
        std::fs::write(profile.join("hidden/SKILL.md"), "---\nname: hidden\ndescription: x\ndisable-model-invocation: true\n---\n").unwrap();
        skill(&profile, ".half.part", "Interrupted copy.");
        let block = skills_block(&profile, &bundled).unwrap();
        assert_eq!(block, format!("<available_skills>\n  <skill>\n    <name>models</name>\n    <description>Edited: &lt;pick&gt; &amp; go.</description>\n    \
<location>{p}/models/SKILL.md</location>\n  </skill>\n  <skill>\n    <name>my-own</name>\n    <description>Added by an agent.</description>\n    \
<location>{p}/my-own/SKILL.md</location>\n  </skill>\n</available_skills>", p = profile.display()));
    }

    #[test]
    fn the_relay_text_has_pi_s_fills_the_profile_s_skills_and_the_cli() {
        let d = tempfile::tempdir().unwrap();
        let data = d.path();
        skill(&data.join("agent/skills"), "only-here", "The profile's own.");
        let exe = Path::new("/nowhere/target/debug/heimdall");
        let text = text(exe, data, "/A/heimdall-emergency --data-dir /D");
        assert!(text.contains("`/A/heimdall-emergency --data-dir /D tools`") && !text.contains(CLI_PLACEHOLDER));
        assert!(text.contains(&format!("{}/agent/skills/only-here/SKILL.md", data.display())) && !text.contains(skill_index::SKILLS_DIR));
        assert_eq!(text.matches(skill_index::START).count(), 1);
        let pi = crate::harness::fill_prompt(crate::harness::PROMPT, exe, Some(&crate::tool_log::path(data)), Some(data));
        assert!(text.contains(pi.trim_end()), "Heimdall's prompt with Pi's fills");
        for placeholder in [crate::tool_log::PROMPT_PLACEHOLDER, crate::harness::DOCS_PLACEHOLDER, crate::harness::EMERGENCY_PLACEHOLDER, crate::harness::CONNECTORS_PLACEHOLDER] {
            assert!(!text.contains(placeholder), "{placeholder}");
        }
    }
}
