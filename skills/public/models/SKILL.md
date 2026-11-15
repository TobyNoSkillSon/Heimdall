---
name: models
description: Choose a task or sub-agent model and reasoning effort, respect provider plans and subscription limits, customise routing, and check local models. Use before delegating any task, when access fails, or when local AI models are needed.
metadata:
  class: public
---

# Models

## Routing

Tasks and their sub-agents run on the coding-agent accounts the user set up in T3 Code: Claude, ChatGPT or Codex, or
another one the app supports. Nothing here prefers a provider. Three things decide, in this order:

1. A model or provider the user names for this task. Use it; if it is unavailable, report that and ask rather than
   silently substituting.
2. The user's own routing, in a `my-models` skill when there is one. It wins over everything below.
3. Otherwise, the providers this Mac has ("This install" in your prompt, and T3's live catalog), picked by the kind
   of work:

| Work | Pick, from a provider the user has |
|---|---|
| Light work, research, search, extraction, transcripts, downloads, mechanical checks and scripts | its lighter, cheaper model, at high effort |
| Visual work, design, taste decisions and quality-sensitive fresh review | its strongest model, at medium effort |
| Engineering, app code, debugging, integration, builds and releases | its strongest model, at medium effort |

These are routing preferences, not measured claims of equivalent intelligence, speed or quality. When the user has
more than one provider and has not said which one tasks use, ask once, with one card, and write the answer into
`my-models` (`heimdall-setup` says how), so the question never comes back.

This covers delegates only: your own model and provider are the user's separate setting (`heimdall-controls`), and
this table does not move it.

For mixed work, split independent design review and implementation when useful. Keep merges, integration, installs
and local model/audio evaluation on the strongest model, with a fresh review by it for quality-sensitive changes;
never the light model for those. Escalate only the failing job type: after a light model fails, or after two returns
for the same fault, move it to the strongest model at medium effort. Use higher reasoning only for the riskiest
pieces, where a mistake costs a rebuild, within supported options.

Resolve the provider instance, model ID and options from T3's live catalog and the user's actual authenticated
plan before delegating. A catalog entry does not prove quota or subscription access. Recheck availability when
choosing; never invent a generation or model ID. A wrong model id or option comes back with T3's live choices, and
`t3_tools` with the query "model" finds the tool that lists them.

Model and effort go inside `target`, with the selected provider instance, in the shape
{"model":"<an id from the catalog>","providerInstanceId":"<its instance>","options":{"effort":"high"}}.
Always pass `target` yourself: left out, `delegate_task` falls back to one built-in model, which this user may not
have.

Use the access the user set up; never move to metered API billing they did not choose. Respect the plan's actual remaining allowance. If subscription
access or quota fails, report the block; continue independent work only on a subscription the user already
authorised, when no explicit model request prevents it. Never silently switch to metered APIs or another provider,
buy credits, or enable billing. Report any provider/model change.

Check current facts about AI models with tools, never from memory. Model selection changes neither consent nor the
task's safety, install, GPU or memory limits.

## Local models

One heavy GPU job at a time; check free memory before a model load.

## Customisation

The active profile's models/SKILL.md is editable by the user's own agent without rebuilding the app. This file
holds routing explanations and defaults; runtime provider selection and billing enforcement are separate code.
Bundled defaults ship in app updates: the updater replaces this file only while it is unedited, and keeps an edited
copy, saving the new default beside it as a conflict in the release notice. The user's own routing goes in a private
`my-models` skill (the `update-skills` skill says where), which no update touches; then `reload_me`. Preserve local
edits; propose adopting or merging new defaults.
