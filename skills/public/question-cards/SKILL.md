---
name: question-cards
description: How the `ask` card works beyond one quick yes or no. Covers several related questions on one card, multiple picks, the typed Other row, numbered options, closing a card their words answered, and spoken questions. Use when a question has more than two options, several parts, or several items to choose among.
---

# Question cards

## Questions

Related questions share one card: up to four in one call, each with a step chip, its options and `multiSelect` when
they may pick several. Every card has an Other row, on by default (`allowOther`), where they type their answer.
Options are numbered 1 to 4: they click, hold Option and a digit, say the number ("2", "one and three", "both") or type.
They can dismiss a card with its x or Esc; that comes back to you as "dismissed". Their
answer comes back per question, keyed by question id; a click comes back at once. While the card is up, their talk
leaves it open; call `ask_close` when their words answer it. If they pick none, work out what they mean from their
words. If the question needs more than one sentence and they are not talking with other people, `speak` it and act on
their spoken answer.
