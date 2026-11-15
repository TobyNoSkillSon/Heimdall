#!/usr/bin/env python3
"""check_public_split.py refuses a tree where private text could ship, and passes this repository's public layer.
No real export needed: tiny trees in a temp folder."""
import importlib.util, shutil, tempfile
from pathlib import Path

spec = importlib.util.spec_from_file_location('cps', Path(__file__).with_name('check_public_split.py'))
cps = importlib.util.module_from_spec(spec); spec.loader.exec_module(cps)


def skill(root, rel, name, cls, body='Body.\n'):
    d = root / rel / name
    d.mkdir(parents=True, exist_ok=True)
    meta = f'metadata:\n  class: {cls}\n' if cls else ''
    (d / 'SKILL.md').write_text(f'---\nname: {name}\ndescription: What it does. Use when asked.\n{meta}---\n\n{body}')


def has(problems, part):
    return any(part in p for p in problems)


assert cps.skill_class('---\nname: a\ndescription: b\nmetadata:\n  class: private\n---\n') == 'private'
assert cps.skill_class('---\nname: a\ndescription: b\nmetadata:\n  author: x\n  class: "public"\n---\n') == 'public'
assert cps.skill_class('---\nname: a\ndescription: b\nclass: public\n---\n') is None, 'only under metadata'
assert cps.skill_class('---\nname: a\ndescription: class: public\n---\n') is None
assert cps.skill_class('no frontmatter') is None

with tempfile.TemporaryDirectory() as t:
    tree, private = Path(t) / 'tree', Path(t) / 'private'
    for p in cps.PUBLIC_PROMPTS:
        (tree / p).parent.mkdir(parents=True, exist_ok=True)
        (tree / p).write_text('# Heimdall\n\nYou are the assistant. [Now: Tuesday 20 Oct 2026, 19:22]\n')
    skill(tree, 'skills/public', 'models', 'public')
    skill(tree, 'skills/shared', 'update-skills', 'public')
    assert cps.check(tree) == [], cps.check(tree)
    assert cps.check(tree, whole_tree=True) == []
    # A third-party package's example skill (the agent runtime's node_modules) is not one of Heimdall's skills.
    skill(tree, 'Contents/Resources/agent-runtime/node_modules/pkg/examples/x', 'demo', None)
    assert cps.check(tree, whole_tree=True) == [], cps.check(tree, whole_tree=True)
    shutil.rmtree(tree / 'Contents')

    # 1: a skill without a class, or with one written outside `metadata`.
    skill(tree, 'skills/public', 'unclassed', None)
    assert has(cps.check(tree), 'skills/public/unclassed/SKILL.md: no class')
    skill(tree, 'skills/public', 'unclassed', 'public')
    # 2: a private-class skill in a public set.
    skill(tree, 'skills/public', 'my-models', 'private')
    assert has(cps.check(tree), 'my-models/SKILL.md: a private-class skill in a public place')
    skill(tree, 'skills/public', 'my-models', 'public')
    # A set that is not public.
    skill(tree, 'skills/heimdall', 'night', 'public')
    assert has(cps.check(tree), 'skills/heimdall/: not a public skill set')
    # 3: private paths, in a whole tree only (a source checkout keeps its lab).
    assert has(cps.check(tree, whole_tree=True), 'private path in the tree: skills/heimdall')
    import shutil
    shutil.rmtree(tree / 'skills/heimdall')
    for rel in ['crates/agent/prompt/personal/x.md', 'lab/prompt-split/private/environment.md', 'docs/personal/a.md', 'data/agent/private/environment.md']:
        (tree / rel).parent.mkdir(parents=True, exist_ok=True)
        (tree / rel).write_text('x\n')
    assert cps.check(tree) == [], 'the source checkout may hold them'
    found = cps.check(tree, whole_tree=True)
    for part in ['prompt/personal/', 'lab/prompt-split/', 'docs/personal/', 'agent/private/']:
        assert has(found, part), (part, found)
    for rel in ['crates/agent/prompt/personal', 'lab', 'docs', 'data']:
        shutil.rmtree(tree / rel)

    # 4: the built-in patterns in the public layer: a home path, a vault, a dated quote.
    prompt = tree / cps.PUBLIC_PROMPTS[0]
    clean = prompt.read_text()
    for bad, name in [('Read /Users/sam/notes.md.', 'home path'), ('In the Vault/Projects folder.', 'personal vault'),
                      ('Always ask first (Sam, 14 Nov).', 'dated quote'), ('He said it on 14 Nov 09:26.', 'dated quote')]:
        prompt.write_text(clean + bad + '\n')
        assert has(cps.check(tree), f'heimdall.md:4: {name}'), (bad, cps.check(tree))
    # One user's model routing stated as a rule: a provider prescribed or forbidden, a model named as the one to use.
    for bad, name in [('Never start a Codex task or sub-agent.', 'provider rule'), ('Delegates run on the Anthropic subscription only.', 'provider rule'),
                      ('Use Claude only for tasks.', 'provider rule'), ('Research goes to Claude Haiku 5.5, high effort.', 'model named'),
                      ('Pick `claude-opus-5-5` for code.', 'model named'), ('Judges run on GPT-6.1 Sol.', 'model named')]:
        prompt.write_text(clean + bad + '\n')
        assert has(cps.check(tree), f'heimdall.md:4: {name}'), (bad, cps.check(tree))
    # The mechanism, and the names of the harnesses, are general.
    prompt.write_text(clean + 'Tasks run in Claude Code, Codex, OpenCode or Grok through T3 Code, on the accounts the user set up.\n'
                      'Its strongest model for code, its lighter one for lookups. It guards Claude Code and T3 Code only. Never switch provider unasked.\n')
    assert cps.check(tree) == [], cps.check(tree)
    # The speed skill alone may name models, as the developer's dated opinion.
    skill(tree, 'skills/public', 'speed-and-models', 'public', 'The developer\'s view: Claude Opus 5.5 today.\n')
    assert cps.check(tree) == [], cps.check(tree)
    skill(tree, 'skills/public', 'speed-and-models', 'public', 'Never use Codex for this.\n')
    assert has(cps.check(tree), 'speed-and-models/SKILL.md:8: provider rule'), cps.check(tree)
    import shutil as _sh
    _sh.rmtree(tree / 'skills/public/speed-and-models')
    prompt.write_text(clean + 'Example: /Users/x/Library, 20 Oct 2026, by 14 Nov.\n')
    assert cps.check(tree) == [], cps.check(tree)

    # One user's private folder: its strings and its lines.
    private.mkdir()
    (private / 'private-strings.txt').write_text('# mine\n[everywhere]\nSecretProject\n[prompt and skills]\nSam\nCzech\n')
    (private / 'environment.md').write_text('# This install\n\n<!-- heimdall:environment:start -->\n- A generated line that the app itself writes for everyone.\n'
                                            '<!-- heimdall:environment:end -->\n\n## Rules\n\n- Never open the blue browser while a call is running, ever.\n')
    skill(private, 'skills', 'my-models', 'private', 'Always route the heavy work to the big model on the second account.\n')
    assert cps.check(tree, [private]) == []
    prompt.write_text(clean + 'Answer in czech when asked. Samson and the samples are fine.\n')
    assert [p for p in cps.check(tree, [private])] == ['crates/agent/prompt/heimdall.md:4: private string #3 of private-strings.txt'], cps.check(tree, [private])
    prompt.write_text(clean + 'Ask Sam.\n')
    assert has(cps.check(tree, [private]), 'heimdall.md:4: private string #2')
    prompt.write_text(clean)
    # The app's own model prompts are in the layer too (release 1114: a personal copy came across unscrubbed): a private
    # string there is refused; their placeholder paths and dated design notes are not.
    app = tree / 'crates/app/prompt/goals.md'
    app.parent.mkdir(parents=True, exist_ok=True)
    app.write_text('Examples: "Website: push branch".\nAsk Sam.\n')
    assert has(cps.check(tree, [private]), 'crates/app/prompt/goals.md:2: private string #2'), cps.check(tree, [private])
    app.write_text('Open "/Users/me/Reports/x.html". Bad (6 Nov): a blur.\n')
    assert cps.check(tree, [private]) == [], cps.check(tree, [private])
    app.unlink()
    # A private line copied into a public skill, rewrapped.
    skill(tree, 'skills/public', 'models', 'public', 'Always route the heavy work to the big\nmodel on the second account.\n')
    assert has(cps.check(tree, [private]), 'skills/public/models/SKILL.md: carries a line of a private file')
    skill(tree, 'skills/public', 'models', 'public')
    # The app's own template above the marker is not a private line.
    (tree / 'crates/agent/src').mkdir(parents=True)
    (tree / 'crates/agent/src/environment.rs').write_text('// A generated line that the app itself writes for everyone.\n')
    assert cps.check(tree, [private], whole_tree=True) == [], cps.check(tree, [private], whole_tree=True)
    # In a whole tree, a private line or an everywhere string in any text file.
    (tree / 'crates/agent/src/notes.rs').write_text('// Never open the blue browser while a call is running, ever.\n// see SecretProject\n')
    found = cps.check(tree, [private], whole_tree=True)
    assert has(found, 'notes.rs: carries a line of a private file') and has(found, 'notes.rs: private string #'), found
    assert cps.check(tree, [private]) == [], 'the layer check reads only prompts and skills'

# This repository's own public layer is clean, and every listed file exists.
root = Path(__file__).resolve().parent.parent
problems = cps.check(root)
assert problems == [], problems
assert all((root / f).is_file() for f in cps.listing(root)) and any(f.endswith('heimdall.md') for f in cps.listing(root))
print('check_public_split: ok')
