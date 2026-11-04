---
name: islands-and-plan
description: Floating islands, the user's Goals, Scheduled and Settled lists, and scheduled wakes. Use when showing work at a glance, when they ask to change their plan or be reminded at a time, and when an island notice arrives.
---

# Islands and plan

## Floating islands

Work the user should see at a glance gets a floating island: `islands_put` with a 1-3 word title, state, 1-2 word
stage and one-line summary. Each `delegate_task` gets a job island bound to its thread by itself (the result names
it): it follows the thread and shows its questions as buttons; add details with `islands_put` on that id, or pass
`thread` for other T3 work. `islands_view` opens any file for them; a report or page a task made opens there, never in
a browser tab, which stays for real websites and pages that need a local server (a picks page a task serves);
`islands_remove` clears islands they have seen.
Goals are where the user is going, Scheduled what is promised and when, Settled what is done, each with its proof.
Nothing enters Goals or Scheduled without their yes, by voice or one click: `plan_edit` propose a goal, a next step
under a goal (goal) or a promise with a real time (list scheduled, due), with why: the words behind it, and say it in
one short line. Their yes is accept (quote them; a new time is accept with due), their no is decline; a click on Yes
arrives as a notice and is already done. One proposal shows per list; unanswered, it is archived after two days and
never comes back. Add directly only what they asked for in their own words: a promise with due and quote, or an ask
in Goals (kind, goal) for something only they can do now; asks and blocked or failed jobs show on Scheduled's line of
what waits on them, never proposals. Settled fills itself from proof (a commit, a passing test, an install, an output
file, their own words for a decision). `plan_edit` show reads a list with its proposals. A report's own buttons reach
you from the viewer (`window.heimdall.send(action, value, label)`). Call `refresh_plan` after you agree on or finish
something; a list they closed is only hidden: show brings it back.

## Scheduled wakes

When the user says "at seven, do this", call `schedule_wake` with the time and a note of what to do: the note wakes
you then, also after a restart. On long work, set one as a safety check in case its notice never comes; `list_wakes`
and `cancel_wake` show and remove pending ones.
