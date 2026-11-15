//! The relay's system prompt (docs/claude-relay.md): the T3 thread acting as Heimdall's agent gets Heimdall's own prompt
//! inline in its first message, since T3 has no system-prompt parameter. `build.rs` composes it per edition from one
//! source of truth: `prompt/relay.md` (what differs in a T3 thread), Heimdall's prompt, and the bundled skills index
//! (`skill_index`). At runtime it gets the same fills as Pi's prompt (`harness::fill_prompt`), the profile's own skills
//! index, and the CLI line. `prompt/relay.generated.md` is the checked-in copy; a test fails when it is stale.
use crate::skill_index;
use std::path::Path;

/// The composed template (`build.rs`): the same for every edition.
pub const TEMPLATE: &str = include_str!(concat!(env!("OUT_DIR"), "/relay.md"));
/// The relay section's name for the CLI line (`Emergency::cli_for`).
pub const CLI_PLACEHOLDER: &str = "HEIMDALL_RELAY_CLI";
/// The line the skills index opens with in the composed template (`skill_index::compose`).
const INDEX_INTRO: &str = "The following skills provide specialized instructions for specific tasks.";
/// The command that rewrites the checked-in snapshot after a prompt, skill or relay.md change.
pub const REGENERATE: &str = "HEIMDALL_WRITE_RELAY_PROMPT=1 cargo test -p agent relay_prompt";

/// The skills index the relay should see, as Pi builds it: the user's own skills (`own`) first, then the profile's
/// general folder when it exists (what Pi loads), else the bundled sets read now with their real locations. A name
/// already listed is skipped, so a private skill replaces a general one. None when there is no skill at all.
pub fn skills_block(own: &Path, profile: &Path, bundled: &Path) -> Option<String> {
    let mut skills: Vec<(skill_index::Skill, String)> = Vec::new();
    let mut add = |found: Vec<skill_index::Skill>, root: String| {
        for s in found {
            if !skills.iter().any(|(k, _)| k.name == s.name) {
                skills.push((s, root.clone()));
            }
        }
    };
    add(skill_index::scan(&[own]), own.display().to_string());
    if profile.is_dir() {
        add(skill_index::scan(&[profile]), profile.display().to_string());
    } else {
        for set in crate::skills::sets(bundled) {
            add(skill_index::scan(&[&set]), set.display().to_string());
        }
    }
    (!skills.is_empty()).then(|| skill_index::render_located(&skills))
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
        Some((relay, prompt)) => format!("{}{}{}", crate::identity::template(relay, &crate::identity::current()), skill_index::SEPARATOR, fill(prompt)),
        None => fill(TEMPLATE),
    };
    let filled = match skills_block(&crate::environment::skills_dir(data), &skills, &crate::skills::bundled(exe)) {
        Some(block) => with_index(&filled, &block),
        None => filled,
    };
    let filled = filled.replace(skill_index::SKILLS_DIR, &skills.display().to_string()).replace(CLI_PLACEHOLDER, cli);
    // This install's private layer goes under Heimdall's general prompt, above the skills index, as Pi has it.
    let layer = crate::harness::private_layer(exe, Some(&crate::tool_log::path(data)), data);
    match filled.find(INDEX_INTRO) {
        Some(at) => format!("{}{layer}\n\n{}", &filled[..at], &filled[at..]),
        None => format!("{}\n\n{layer}\n", filled.trim_end()),
    }
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
        let now = skill_index::compose_from(dir).unwrap();
        assert_eq!(now, TEMPLATE, "build.rs and the sources disagree; rebuild");
        let snapshot = dir.join(skill_index::SNAPSHOT);
        if std::env::var_os("HEIMDALL_WRITE_RELAY_PROMPT").is_some() {
            std::fs::write(&snapshot, &now).unwrap();
        }
        let saved = std::fs::read_to_string(&snapshot).unwrap_or_default();
        assert!(saved == now, "{} is stale: Heimdall's prompt, a bundled skill or relay.md changed. Regenerate it with:\n  {REGENERATE}",
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
        skill(&bundled.join(crate::skills::GENERAL_SET), "models", "Pick a model.");
        skill(&bundled.join(crate::skills::SHARED_SET), "update-skills", "Change a skill.");
        skill(&bundled.join("workers"), "git-workflow", "Not Heimdall's.");
        let profile = d.path().join("agent/skills");
        let own = d.path().join("agent/private/skills");
        let block = skills_block(&own, &profile, &bundled).unwrap();
        assert!(block.contains(&format!("<location>{}/{}/models/SKILL.md</location>", bundled.display(), crate::skills::GENERAL_SET)), "{block}");
        assert!(block.contains(&format!("<location>{}/shared/update-skills/SKILL.md</location>", bundled.display())) && !block.contains("git-workflow"));
        // Pi's folder, with an edited and an added skill, a removed one, a hidden one and a half copy.
        skill(&profile, "models", "Edited: <pick> & go.");
        skill(&profile, "my-own", "Added by an agent.");
        std::fs::create_dir_all(profile.join("hidden")).unwrap();
        std::fs::write(profile.join("hidden/SKILL.md"), "---\nname: hidden\ndescription: x\ndisable-model-invocation: true\n---\n").unwrap();
        skill(&profile, ".half.part", "Interrupted copy.");
        let block = skills_block(&own, &profile, &bundled).unwrap();
        assert_eq!(block, format!("<available_skills>\n  <skill>\n    <name>models</name>\n    <description>Edited: &lt;pick&gt; &amp; go.</description>\n    \
<location>{p}/models/SKILL.md</location>\n  </skill>\n  <skill>\n    <name>my-own</name>\n    <description>Added by an agent.</description>\n    \
<location>{p}/my-own/SKILL.md</location>\n  </skill>\n</available_skills>", p = profile.display()));
        // The user's own skills come first: `my-models` adds to `models`, and a private `models` would replace it.
        skill(&own, "my-models", "Mine.");
        skill(&own, "my-own", "The private one wins.");
        let block = skills_block(&own, &profile, &bundled).unwrap();
        assert!(block.contains(&format!("<location>{}/my-models/SKILL.md</location>", own.display())), "{block}");
        assert!(block.contains(&format!("<location>{}/my-own/SKILL.md</location>", own.display())) && !block.contains(&format!("{}/my-own/SKILL.md", profile.display())));
        assert!(block.find("my-models").unwrap() < block.find("<name>models</name>").unwrap(), "private first");
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
        assert!(text.contains(&format!("`{}`", crate::harness::memory_dir(data).display())), "the relay gets the memory folder's path");
        let pi = crate::harness::fill_prompt(crate::harness::PROMPT, exe, Some(&crate::tool_log::path(data)), Some(data));
        assert!(text.contains(pi.trim_end()), "Heimdall's prompt with Pi's fills");
        // The private layer sits under Heimdall's prompt and above the skills index, with this profile's block.
        let (layer, index) = (text.find(crate::environment::HEADING).unwrap(), text.find(skill_index::START).unwrap());
        assert!(text.find(pi.trim_end()).unwrap() < layer && layer < index && text.contains(crate::environment::START));
        assert!(crate::environment::file(data).is_file(), "written on the way");
        for placeholder in [crate::tool_log::PROMPT_PLACEHOLDER, crate::harness::DOCS_PLACEHOLDER, crate::harness::EMERGENCY_PLACEHOLDER, crate::harness::CONNECTORS_PLACEHOLDER, crate::harness::MEMORY_PLACEHOLDER] {
            assert!(!text.contains(placeholder), "{placeholder}");
        }
    }
}
