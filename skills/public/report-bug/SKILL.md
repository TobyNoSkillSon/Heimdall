---
name: report-bug
description: Offering the user a report of a bug in Heimdall itself to its developer, through their own GitHub account, at one of three depths and only with their yes. Use when you find a bug in Heimdall (a tool that fails or misleads, a wrong wake, a broken island or menu) or the user says Heimdall itself is broken.
metadata:
  class: public
---

# Reporting a bug in Heimdall

A switch the user turned off themselves is their choice, not a bug.

When you find a bug in Heimdall itself, ask the user: "I found a bug in Heimdall. Do you want me to report it to the
GitHub repo so its developer can fix it?" Say plainly that a report goes to
`TobyNoSkillSon/Heimdall` under their own GitHub account, through the GitHub CLI.

If `gh auth status` says they are not signed in, offer to help set it up; they always sign in themselves, never you.
If they are, offer three depths, since each uses their quota: a short report of what happened; an analysis of what
went wrong with a proposed fix; or a deep investigation that opens a proper pull request, as a task.

Before you show a report or pull request for approval, remove personal data: names, paths with their user name,
emails, keys, and transcript content it does not need. Every issue and pull request needs their explicit yes, each
time; never file anything silently.

A complaint or an idea, or a user without GitHub: with their yes, and after showing them the text, send it to the
developer's feedback inbox. No personal data, files or screenshots of their content unless they have seen them. Any
text works: `curl -d "heimdall <version>: what happened, what you expected, steps to reproduce" https://tobynoskillson.com/api/feedback`.
With a screenshot or log: `curl -F "message=heimdall: ..." -F file=@shot.png https://tobynoskillson.com/api/feedback`.
The reply is an id like r-xxxxxx.

"This install" may name another place for bugs on this Mac. Then use that place and skip this skill.
