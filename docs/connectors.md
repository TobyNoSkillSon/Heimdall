# Connecting accounts and MCP servers

For the agent. Read this when someone asks you to connect an account or an MCP server, or whether you can reach one.
Checked 24 Oct 2026 against Claude Code's MCP documentation, Pi's MCP documentation and the Codex CLI's help; providers
checked 4 Nov 2026 against this build. Connector listings change, so check the one you name before you promise it.

## Agent providers

The agent runs on one provider, chosen in Agent ▸ Model (Add Provider adds one). The user needs one of them:

- **Claude via T3 Code** ("Claude Code subscription"): the user's Claude Pro or Max plan, reached through a T3 Code
  thread that acts as the agent (docs/claude-relay.md). Needs T3 Code and a signed-in Claude Code.
- **Sign in with ChatGPT** ("Codex subscription"): the user's ChatGPT Plus or Pro plan, signed in through the browser
  (docs/chatgpt.md). Beta: some requests fail with HTTP 400 because the provider rejects a prompt-cache breakpoint.
- **OpenCode Go** ("OpenCode Go subscription"): signed in with a device code from opencode.ai.
- **Anthropic API key** ("Anthropic API"): a key from the Anthropic Console, billed per use.
- **OpenRouter** ("OpenRouter API"): an OpenRouter key, billed per use.
- **Own Engine**: any OpenAI-compatible URL, key optional, such as a model server the user runs.

Keys and sessions stay in the Keychain under the public app's own names. Never ask the user to paste a key into chat:
they type it into the provider panel themselves.

## Which route

- **Accounts with a claude.ai connector** (Gmail, Google Calendar, Google Drive, Slack, Notion, Stripe and many more):
  route 1. Reach them through a Claude Code task, not directly.
- **Codex**: route 2.
- **A server added to you** (your own Pi): route 3.
- **Discord** is not in the connector directory. Say so; there is no route for it here.

## Route 1: claude.ai connectors through a Claude Code task

Only Claude Code fetches these connectors, and only when it is signed in with a claude.ai subscription. Your own Pi
cannot use them directly. A Claude Code task can.

1. The user adds the connector and signs in at claude.ai/customize/connectors. Only they can take that step.
   Gmail, Google Calendar and Microsoft 365 cannot be signed into from Claude Code; they must be signed into there.
2. The task must use the claude.ai login. These stop connectors from loading: `ANTHROPIC_API_KEY`,
   `ANTHROPIC_AUTH_TOKEN`, `apiKeyHelper`, a third-party provider such as Amazon Bedrock, and a
   `CLAUDE_CODE_OAUTH_TOKEN`. If a connector is missing from the task's `/mcp` list, run `/status` to see which
   login is active.
3. Check it: a task lists the connector's tools and reads one thing. A connector shown as needing authentication is
   not authorised yet, and its tools are not there until the user signs in.
4. A headless run (`claude -p` or the Agent SDK) has no `/mcp` panel and cannot finish a sign-in. Authorise on
   claude.ai first.
5. Gmail can send. Reading and drafting need no click. Sending, replying, forwarding and posting do.

To switch connectors off: `"disableClaudeAiConnectors": true` in a settings file turns all of them off;
`ENABLE_CLAUDEAI_MCP_SERVERS=false` does the same for one shell; `deniedMcpServers` with a name such as
`"claude.ai Slack"` blocks one.

## Route 2: Codex

Codex keeps its own MCP server list in `~/.codex/config.toml`. The claude.ai connectors in route 1 belong to Claude Code,
not Codex. Manage the list with `codex mcp`:

- `codex mcp add <name> -- <command> [args]` adds a local server. `--env KEY=VALUE` sets its environment.
- `codex mcp add <name> --url <url>` adds a remote server. `--bearer-token-env-var NAME` reads its token from an
  environment variable.
- `codex mcp login <name>` signs in to a remote server that uses OAuth.
- `codex mcp list` shows what is configured; `codex mcp remove <name>` removes a server.

Never type a token into a command or paste it into chat. Use an environment variable name or OAuth.

## Route 3: a server added to you

Your Pi reads MCP servers from `mcp.json` in its agent folder: `~/Library/Application Support/Heimdall/agent/config/mcp.json`.
Heimdall never writes that file, so it is yours to edit. Heimdall's own tools are separate and keep working. Pi's own
MCP guide is in the installed app's Resources, at `agent-runtime/node_modules/@earendil-works/pi-coding-agent/docs/mcp.md`.

Every server runs a command, or sends data to a URL, each time Pi starts. So before you add one:

1. Get the user's yes on the exact command or URL, and on any install it needs (`npx`, `uvx`, `brew`, a download). That
   changes their Mac, so it needs their yes.
2. Edit `mcp.json`; create it if it is missing. Use an absolute path for `command`: Heimdall starts Pi with a small
   environment, so a bare `npx` may not be found. Each server is one entry under `mcpServers`:

   ```json
   {
     "mcpServers": {
       "notes": {
         "command": "/usr/local/bin/some-notes-server",
         "args": ["--read-only"],
         "exposure": "direct"
       }
     }
   }
   ```

   A remote server has a `"url"` and optional `"headers"` instead of `command` and `args`.
3. Keep `"exposure": "direct"` on every server. Heimdall runs Pi without codemode, so Pi's default exposure would leave
   the tools unreachable. Server names may contain only letters, digits, `_` and `-`. Do not name a server `heimdall`.
4. Call `reload_me` with the reason. Pi restarts after your turn ends and connects every enabled server.
5. Check it: the next turn's tools include `mcp__<name>__<tool>`. If they do not, the server did not connect. Try
   `pi mcp list` with `PI_CODING_AGENT_DIR` set to the agent folder (`pi` is the CLI in the installed app's agent-runtime;
   untested from bash) and say what it prints. `mcp.log` in the same folder holds only the server's own log lines.

To remove a server, delete its entry and call `reload_me`.

Secrets: `env` and `headers` values sit in plain text in that file. Have the user type a token in themselves, or use
Pi's `!command` form, where the whole value is a command that prints the secret. Never paste a token into chat. `${NAME}`
reads Pi's environment, which Heimdall filters, so do not rely on it. Pi's OAuth sign-in (`pi mcp login`) opens a browser;
it is not tested through Heimdall yet, so say so before you try it.

## Walking a user through it

- Ask which account or tool they mean. Check the current state first: a task's tool list, `codex mcp list`, or the
  entries in `mcp.json`.
- Do every step you can yourself. The user takes only the steps that need them: signing in to claude.ai, approving a
  connector, a browser sign-in. Name the exact page.
- After they say it is done, check again and report what you saw.
- Keep it brief. Explain the mechanism only if they ask.
- Sending, posting, replying, buying or deleting: draft it, show it, and wait for their click. Silence is not approval.

## What to tell them

- When the route works: say yes, then say the one click they need to make.
- When a step is untested, such as an OAuth sign-in through route 3: say that before you try it.
- Never say a connector or server works until a task or your own tool list has shown its tools.
