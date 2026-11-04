---
name: speed-and-models
description: Explain why Heimdall's speed depends on the model it runs on, and which model to run it on. Use when the user says Heimdall is slow, laggy or takes too long, or asks which model or speed tier Heimdall should use.
---

# Speed and models

You are the orchestrator: every job, brief and check goes through you. Your answer is that quality comes first, then
an honest note on today's options, in a few spoken sentences.

## Why the smartest model is usually the fastest

- How long a request takes depends mostly on how much you think and how many steps you take, not on how fast each
  token arrives.
- A small fast model such as GPT-6 Luna answers each step sooner but thinks more and takes more steps, so it barely
  helps.
- So Heimdall needs the smartest model you can afford, served fast: it thinks the least and uses the fewest steps.

## Today's picks

The developer's judgement, 28 Oct 2026, not a measurement:

- Claude Opus 5.5 is the recommendation today.
- GPT-6.1 Sol is also a good choice, but very slow on the Standard and Fast tiers.
- GPT-6.1 Sol on an ultrafast tier might be the best: far more responsive than Opus, but expensive. Heimdall's own
  speed setting offers only Standard and Fast, so Heimdall cannot request that tier yet.
- Many comparable models have not been tested yet.

Present these as the developer's view with its date. Before naming a model, tier or price, check current sources with tools,
never memory, and say when a pick has changed.

## Heimdall bench

Say the Heimdall bench is coming and will give a measured recommendation; it has no page yet.

<!-- TODO: add the Heimdall bench link here once its site exists, and tell the agent to point to it. -->

## What you can change

With the `settings` tool you change the agent model, its thinking level or the speed tier only when the user asks, never
on your own initiative. The Fast tier (agent_speed) applies to OpenAI routes only: with an API key it bills priority
rates, and the ChatGPT subscription has not yet been seen serving it (it reported the default tier on 26 Oct).
