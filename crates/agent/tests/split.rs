//! The public/private split of the prompt and the skills (docs/skills.md, "Two layers"): what is in this repository's
//! shipping paths is general text for any user, every skill says which class it is, and the release's own check
//! (`scripts/check_public_split.py`) passes. One user's rules and skills live in their data folder, never here.
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn skill_files() -> Vec<(String, String)> {
    let mut all = Vec::new();
    for set in std::fs::read_dir(root().join("skills")).unwrap().flatten().filter(|e| e.path().is_dir()) {
        for skill in std::fs::read_dir(set.path()).unwrap().flatten().filter(|e| e.path().join("SKILL.md").is_file()) {
            let at = format!("skills/{}/{}", set.file_name().to_string_lossy(), skill.file_name().to_string_lossy());
            all.push((at, std::fs::read_to_string(skill.path().join("SKILL.md")).unwrap()));
        }
    }
    assert!(all.len() >= 10, "skills folder looks empty");
    all
}

/// Every skill in the repository declares `class: public` under `metadata`, and `skills/` holds only the two general
/// sets: a private skill has no place here.
#[test]
fn every_skill_in_the_repository_is_public_class_in_a_general_set() {
    for entry in std::fs::read_dir(root().join("skills")).unwrap().flatten().filter(|e| e.path().is_dir()) {
        let set = entry.file_name().to_string_lossy().into_owned();
        assert!([agent::skills::GENERAL_SET, agent::skills::SHARED_SET].contains(&set.as_str()), "skills/{set}/ is not a general set");
    }
    for (at, text) in skill_files() {
        let front = text.strip_prefix("---\n").and_then(|t| t.split_once("\n---")).map(|(f, _)| f).unwrap_or_else(|| panic!("{at}: frontmatter"));
        assert_eq!(agent::skill_index::class(front), Some(agent::skill_index::Class::Public), "{at}: declare `class: public` under `metadata:`");
    }
}

/// The class is read only where it is declared: under `metadata`, with one of the two values.
#[test]
fn the_class_is_the_metadata_field() {
    use agent::skill_index::{class, parse, Class};
    assert_eq!(class("name: a\ndescription: b\nmetadata:\n  class: private"), Some(Class::Private));
    assert_eq!(class("name: a\nmetadata:\n  author: x\n  class: \"public\"\ndescription: b"), Some(Class::Public));
    assert_eq!(class("name: a\nclass: public"), None, "a top-level key is not the declaration");
    assert_eq!(class("name: a\ndescription: class: public"), None);
    assert_eq!(class("name: a\nmetadata:\n  class: shared"), None);
    assert_eq!(class("name: a\nmetadata:\nlicense: x\n  class: public"), None, "left the metadata block");
    let skill = parse("---\nname: my-models\ndescription: Mine. Use always.\nmetadata:\n  class: private\n---\n\nBody.\n", "my-models").unwrap();
    assert_eq!((skill.name.as_str(), skill.class), ("my-models", Some(Class::Private)));
    assert_eq!((Class::Public.as_str(), Class::Private.as_str()), ("public", "private"));
}

/// The general prompts carry no placeholder a start would leave unfilled, and the one general prompt is what both
/// editions compile.
#[test]
fn one_general_prompt_for_both_editions() {
    let dir = root().join("crates/agent/prompt");
    let mut top: Vec<String> = std::fs::read_dir(&dir).unwrap().flatten().filter(|e| e.path().is_file()).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    top.sort();
    assert_eq!(top, ["compaction.md", "heimdall.md", "relay.generated.md", "relay.md", "taste-review.md", "worker.md"], "a new prompt file is general text: list it here and in scripts/check_public_split.py (taste-review.md is the taste ledger's own guide and goes where that feature goes)");
    assert!(!dir.join("public").exists() && !dir.join("personal").exists(), "no forked or personal copies beside the general prompts");
    assert_eq!(agent::harness::PROMPT, std::fs::read_to_string(dir.join("heimdall.md")).unwrap());
    assert_eq!(agent::harness::WORKER_PROMPT, std::fs::read_to_string(dir.join("worker.md")).unwrap());
    // The prompt names the private layer the way the layer names itself, and the folder by its placeholder.
    let prompt = agent::harness::PROMPT;
    assert!(prompt.contains("\"This install\"") && agent::environment::HEADING == "# This install");
    assert!(prompt.contains(&format!("`{}/environment.md`", agent::harness::PRIVATE_PLACEHOLDER)));
    assert!(prompt.contains("`my-<name>` holds {user}'s own additions to `<name>`: read both, and where they differ the `my-` one wins"));
    let data = Path::new("/Users/x/Notes/.heimdall");
    let filled = agent::harness::fill_prompt(prompt, Path::new("/nowhere/heimdall"), None, Some(data));
    assert!(filled.contains("`/Users/x/Notes/.heimdall/agent/private/environment.md`") && !filled.contains(agent::harness::PRIVATE_PLACEHOLDER));
}

/// The user's own task rules go under the general task prompt, as they wrote them, and only when there are any.
#[test]
fn the_users_task_rules_go_under_the_general_task_prompt() {
    let d = tempfile::tempdir().unwrap();
    let names = agent::identity::Names { assistant: "Heimdall".into(), user: "Sam".into() };
    let general = agent::harness::task_preamble(d.path(), &names);
    assert!(general.contains("Sam's voice assistant") && !general.contains(agent::harness::TASK_RULES_HEADING));
    std::fs::create_dir_all(agent::environment::dir(d.path())).unwrap();
    std::fs::write(agent::environment::dir(d.path()).join("task-rules.md"), "- Reports are HTML pages for {user}.\n").unwrap();
    let with_own = agent::harness::task_preamble(d.path(), &names);
    assert!(with_own.starts_with(general.trim_end()));
    assert!(with_own.ends_with(&format!("{}\n\n- Reports are HTML pages for {{user}}.\n", agent::harness::TASK_RULES_HEADING)), "the user's text is theirs: never filled");
}

/// The release's check passes on this repository: classes, sets, and no private pattern in the general prompts and
/// skills. Where the developer's staged private folder is present (a private checkout), none of its strings or lines
/// is in the public layer either.
#[test]
fn the_release_check_passes_on_this_repository() {
    let script = root().join("scripts/check_public_split.py");
    let mut command = std::process::Command::new("/usr/bin/python3");
    command.arg("-I").arg(&script);
    let staged = root().join("lab/prompt-split/private");
    if staged.is_dir() {
        command.arg("--private").arg(&staged);
    }
    let out = command.output().expect("python3");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}
