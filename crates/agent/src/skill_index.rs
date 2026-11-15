//! The skills index as Pi shows it to the model (its `formatSkillsForPrompt`): each skill's name, description and
//! `SKILL.md` location, from the frontmatter. Std only: `build.rs` includes this file to compose the relay prompt
//! (`relay_prompt`), and the relay renders the profile's own folder with it at runtime.
use std::path::Path;

/// The index's delimiters, as Pi writes them.
pub const START: &str = "<available_skills>";
pub const END: &str = "</available_skills>";

/// Whose a skill is (docs/skills.md, "Two layers"). A `public` skill is true for any user and ships in the app and
/// the public repository; a `private` one is one user's own and lives only in their data folder. Each `SKILL.md`
/// declares it as `class:` under `metadata:` in its frontmatter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Public,
    Private,
}
impl Class {
    pub fn as_str(self) -> &'static str {
        match self {
            Class::Public => "public",
            Class::Private => "private",
        }
    }
}

/// One skill: its name and description from the frontmatter, its folder's name, and the class it declares (None when
/// it declares none or an unknown one).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub folder: String,
    pub class: Option<Class>,
}

/// The skills in `dirs`, each subfolder with a `SKILL.md` that has a description and no
/// `disable-model-invocation: true` (Pi hides those), sorted by folder; a name already listed and hidden folders (a
/// half-copied `.name.part`) are skipped.
pub fn scan(dirs: &[&Path]) -> Vec<Skill> {
    let mut out: Vec<Skill> = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        let mut found: Vec<_> = entries.flatten().map(|e| e.path()).filter(|p| p.join("SKILL.md").is_file())
            .filter(|p| !p.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')))
            .collect();
        found.sort();
        for path in found {
            let folder = path.file_name().unwrap().to_string_lossy().into_owned();
            let Ok(text) = std::fs::read_to_string(path.join("SKILL.md")) else { continue };
            let Some(skill) = parse(&text, &folder) else { continue };
            if !out.iter().any(|s| s.name == skill.name) {
                out.push(skill);
            }
        }
    }
    out.sort_by(|a, b| a.folder.cmp(&b.folder));
    out
}

/// A `SKILL.md`'s frontmatter as a skill, or None without a description or with model invocation disabled.
pub fn parse(text: &str, folder: &str) -> Option<Skill> {
    let body = text.strip_prefix("---\n")?;
    let front = &body[..body.find("\n---")?];
    let field = |key: &str| {
        front.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix(':').map(|v| unquote(v.trim()).to_string()))
    };
    if field("disable-model-invocation").as_deref() == Some("true") {
        return None;
    }
    let description = field("description").filter(|d| !d.is_empty())?;
    let name = field("name").filter(|n| !n.is_empty()).unwrap_or_else(|| folder.to_string());
    Some(Skill { name, description, folder: folder.to_string(), class: class(front) })
}

/// The class a frontmatter declares: the `class:` line indented under `metadata:` (the Agent Skills field every
/// harness ignores), `public` or `private`.
pub fn class(front: &str) -> Option<Class> {
    let mut in_metadata = false;
    for line in front.lines() {
        if !line.starts_with([' ', '\t']) {
            in_metadata = line.trim_end() == "metadata:";
            continue;
        }
        if in_metadata {
            match line.trim().strip_prefix("class:").map(|v| unquote(v.trim())) {
                Some("public") => return Some(Class::Public),
                Some("private") => return Some(Class::Private),
                _ => {}
            }
        }
    }
    None
}

fn unquote(v: &str) -> &str {
    for q in ['"', '\''] {
        if let Some(inner) = v.strip_prefix(q).and_then(|v| v.strip_suffix(q)) {
            return inner;
        }
    }
    v
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

/// The index block, `START` to `END`, each location `<root>/<folder>/SKILL.md`.
pub fn render(skills: &[Skill], root: &str) -> String {
    render_located(&skills.iter().map(|s| (s.clone(), root.to_string())).collect::<Vec<_>>())
}

/// `render` for skills from several folders, each with its own root: the user's own skills and the general ones in
/// one index.
pub fn render_located(skills: &[(Skill, String)]) -> String {
    let mut lines = vec![START.to_string()];
    for (s, root) in skills {
        lines.push("  <skill>".into());
        lines.push(format!("    <name>{}</name>", escape(&s.name)));
        lines.push(format!("    <description>{}</description>", escape(&s.description)));
        lines.push(format!("    <location>{}</location>", escape(&format!("{root}/{}/SKILL.md", s.folder))));
        lines.push("  </skill>".into());
    }
    lines.push(END.into());
    lines.join("\n")
}

/// Where the relay's skills live, filled at runtime (`relay_prompt::fill`).
pub const SKILLS_DIR: &str = "HEIMDALL_SKILLS_DIR";

/// Between the relay section and Heimdall's prompt in the composed template.
pub const SEPARATOR: &str = "\n\n---\n\n";

/// The relay prompt's template from its sources: the relay section, Heimdall's own prompt, then the skills index with
/// Pi's opening lines (the read tool named generically: the relay reads with its own file tools).
pub fn compose(relay: &str, prompt: &str, index: &str) -> String {
    format!(
        "{}{SEPARATOR}{}\n\nThe following skills provide specialized instructions for specific tasks.\nRead a skill's file \
when the task matches its description.\nWhen a skill file references a relative path, resolve it against the skill \
directory (parent of SKILL.md) and use that absolute path in tool commands.\n\n{index}\n",
        relay.trim_end(),
        prompt.trim_end()
    )
}

/// The source files of the relay prompt, relative to the agent crate: the relay section, Heimdall's general prompt,
/// and the general skill sets under the repository's `skills/`. One set of sources for every edition: what differs
/// per user is the private layer, added at runtime from their data folder (`crate::environment`).
pub fn sources() -> (&'static str, &'static str, [&'static str; 2]) {
    ("prompt/relay.md", "prompt/heimdall.md", ["../../skills/public", "../../skills/shared"])
}

/// The relay template composed from the files under the agent crate `dir`.
pub fn compose_from(dir: &Path) -> std::io::Result<String> {
    let (relay, prompt, sets) = sources();
    let skills = scan(&[&dir.join(sets[0]), &dir.join(sets[1])]);
    Ok(compose(&std::fs::read_to_string(dir.join(relay))?, &std::fs::read_to_string(dir.join(prompt))?, &render(&skills, SKILLS_DIR)))
}

/// The checked-in copy of the composed template, relative to the agent crate.
pub const SNAPSHOT: &str = "prompt/relay.generated.md";
