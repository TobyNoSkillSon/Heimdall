---
name: models
description: Choose a task or sub-agent model and reasoning effort, respect provider plans and subscription limits, customise routing, and check local models. Use before delegating any task, when access fails, or when local AI models are needed.
---

# Models

## Routing

Models, for every task and every sub-agent a brief asks for. When the user names a model, use it;
if it is unavailable, report that and ask rather than silently substituting. These are routing preferences,
not measured claims of equivalent intelligence, speed or quality.

| Work | Default target |
|---|---|
| Light work, research, search, extraction, transcripts, downloads, mechanical checks and scripts | Claude Haiku 5.5, high effort, on the Anthropic subscription |
| Visual work, design, taste decisions and quality-sensitive fresh review | Claude Opus 5.5, medium effort, on the Anthropic subscription |
| Engineering, app code, debugging, integration, builds and releases | Claude Opus 5.5, medium effort, on the Anthropic subscription |

Delegates run on the Anthropic subscription only. Never start a Codex task, sub-agent, benchmark or review, and never
fall back to Codex when Claude capacity is unavailable: report the block and wait. Every brief says so, so
sub-workers follow it too. This rule
covers delegates only: the live Heimdall agent keeps its own model and provider, Codex included; this table does not
move it.

For mixed work, split independent design review and implementation when useful. Keep merges, integration, installs
and local model/audio evaluation on Opus, with a fresh Opus review for quality-sensitive changes. Never Haiku for
merges, integration or installs. Escalate only the failing job type: after a light model fails, or
after two returns for the same fault, move it to Opus medium. Use higher reasoning only for the riskiest pieces, where
a mistake costs a rebuild, within supported options.

Resolve the provider instance, model ID and options from T3's live catalog and the user's actual authenticated
plan before delegating. A catalog entry does not prove quota or subscription access. Recheck availability when
choosing; never invent a generation or model ID.

Model and effort go inside `target`, with the selected provider instance. Examples, only when offered:
- Haiku: {"model":"claude-haiku-5-5","options":{"effort":"high"}}.
- Opus: {"model":"claude-opus-5-5","options":{"effort":"medium"}}.

Use subscription access, with no metered API fallback. Respect the plan's actual remaining allowance. If subscription
access or quota fails, report the block; continue independent work only on an already authorised Anthropic
subscription when no explicit model request prevents it. Never silently switch to metered APIs or another provider,
buy credits, or enable billing. Report any provider/model change.

A wrong model id or option comes back with T3's live choices. Check current facts about AI models with tools,
never from memory. Model selection changes neither consent nor the task's safety, install, GPU or memory limits.

## Local models

Include this precision rule explicitly in every local-model brief, even when a default task prompt also carries it:
big models keep their own
precision and check against BF16 tolerances; no FP32 beyond weights of a few MB, and no converting quantized weights
up to BF16. Keep the existing memory gates and one-heavy-GPU-job rule; routing does not relax them.

## Customisation

The active profile's models/SKILL.md is editable by the user's own agent without rebuilding the app. This file
holds routing explanations and defaults; runtime provider selection and billing enforcement are separate code.
Bundled defaults ship in app updates, but current seeding is once-only: existing profiles need a guarded manual
copy and reload. Automatic upgrades and durable release notices are a pending integrator requirement in
docs/skills.md, not implemented by this skill. Preserve local edits; propose adopting or merging new defaults.
