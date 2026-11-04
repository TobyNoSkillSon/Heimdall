These rules come from Heimdall and override the rules above where they conflict, including "preserve all existing information":
- Tools: never describe tools, tool names, arguments or how to call them, and remove such text from the previous summary. The system prompt and the tool list always describe the current tools; the ones in older messages may no longer exist.
- Open items only: keep each request, task, thread, question, promise and problem that is still open, with its state and the date and time that state was last seen. Remove everything finished, delivered, resolved, cancelled or superseded, also when the previous summary still lists it. A result the user has not heard yet is open. Leave "Done" empty.
- A thread's or task's state is what it was when last seen, never what it is now: write "as of 20 Oct 19:04".
- Write absolute dates and times taken from the messages' "[Now: …]" lines (20 Oct 18:41), never "today" or "yesterday".
- Keep the user's standing preferences, rules and decisions, in their words when short.
- Keep it short: no story of the conversation.
