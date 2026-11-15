# ChatGPT subscription (the Codex provider)

Heimdall's agent can run on the user's ChatGPT plan through OpenAI's "Sign in with ChatGPT" token sharing. The menu
and Setup call it Codex.

Read this when: you connect a ChatGPT plan, the ChatGPT sign-in fails or names an error, you wonder why the Codex
CLI's login is not used, or you change the sign-in flow.
Code: `crates/agent/src/chatgpt.rs` (provider id `openai`, `agent::provider::CHATGPT`).

## Connect it

1. Open Agent ▸ Model ▸ Add Provider ▸ **Codex subscription…**, or pick the Codex tile in Setup's step 3 and click
   Sign In… ([setup.md](setup.md)). From a coding agent: `heimdall setup --provider codex`.
2. The browser opens OpenAI's consent page and a small panel says to continue there (Cancel stops it). There is no
   code to type and no key to paste.
3. Approve "ChatGPT plan usage" in the browser. The tab says "ChatGPT is signed in to Heimdall. You can close this
   tab." or "ChatGPT was not connected: <why>". The browser has `LOGIN_TIMEOUT` (10 minutes) to come back.
4. Check it worked: the provider row shows the account connected, and `heimdall.log` has a
   `providers: ChatGPT sign-in: …` line for the test request's status.

Signing in never switches the agent's model; the first provider connected becomes the agent only while none is
chosen ([models.md](models.md)). Models offered, their windows, budgets and thinking levels: [models.md](models.md).

## The route OpenAI documents

OpenAI documents one way for an open-source or locally hosted app to run on a user's ChatGPT plan: Sign in with
ChatGPT, token sharing (developers.openai.com/siwc/token-sharing-open-source). The app registers itself in the user's
browser, the user consents, and the app calls the public Responses API (`https://api.openai.com/v1/responses`) with
the access token. OpenAI says not to point such an app at ChatGPT's `backend-api` endpoints.

So Heimdall does not:

- read the Codex CLI's login (`$HOME/.codex/auth.json`): that token was issued to OpenAI's own client, lacks the
  `chatgpt.tokens.use.direct` scope, and refreshing it from Heimdall would rotate the CLI's refresh token from under it;
- use Pi's `openai-codex` provider, which sends the Codex CLI's client id to `chatgpt.com/backend-api`.

Pi's built-in `openai` provider already sends the documented request shape for a token that does not start with
`sk-` (`store: false`, `stream: true`, none of the rejected fields). Heimdall owns only the sign-in, the Keychain
session and the per-request token command, `heimdall --provider-token openai`.

## How the sign-in works

- The first sign-in registers Heimdall (`client_id=dynamic_agent_client`, agent name `Heimdall`, an opaque
  `ext_agent_host_id` kept in `<data>/agent/chatgpt-agent-host`).
- OpenAI redirects to `http://127.0.0.1:1455/auth/callback` (`chatgpt::PORT`). Heimdall checks the state, keeps the
  issued client id, exchanges the code with PKCE (no secret) and refuses a grant without `chatgpt.tokens.use.direct`.
  The browser tab waits for the exchange before it says anything.
- The session (a one-hour access token, a rotating 30-day refresh token, the client id) goes to the Keychain item
  `Heimdall Alpha API provider: ChatGPT` (`Heimdall API provider: ChatGPT` in the personal edition). The app refreshes
  the token 10 minutes before it expires (`console::Budget::APP`); Pi's token command refreshes under the same lock
  otherwise.
- If ChatGPT's settings removed the app, the issued client id stops working; Heimdall forgets it and the next sign-in
  registers again.
- Every sign-in makes one tiny test request (`chatgpt::probe`) on `chatgpt::test_model`: GPT-6 Luna when listed, else
  the smallest listed model (a Luna, mini or nano), else GPT-6.1 Sol (`DEFAULT_MODEL`), else the first listed.
- The models offered are those the account lists at `/v1/models` with `visibility: list`.
- The answer's scope and expiry are read in every shape OpenAI documents, with the access token's own claims as a
  fallback.

Port 1455 is also used by `codex login` while it runs. If both are open at once, Heimdall says "Port 1455 is busy:
finish or cancel the Codex or ChatGPT sign-in open elsewhere, then try again".

## Errors

Each failure is named on the provider row (`chatgpt::explain`, `not_eligible`):

| Message | Cause |
|---|---|
| ChatGPT: your <plan> plan can't share its usage with apps · OpenAI allows Plus and Pro | the plan cannot share usage (the plan is named when the ID token says it) |
| ChatGPT: usage limit reached · see ChatGPT ▸ Settings ▸ Usage | `subscription_sharing_usage_limit_exceeded` |
| ChatGPT: usage check unavailable · try again shortly | `subscription_sharing_usage_unavailable` or `…_user_unavailable` |
| ChatGPT: account check failed · try again shortly | `subscription_sharing_invalid_user` |
| ChatGPT: this model or feature isn't available on the plan | `subscription_sharing_unsupported_capability` |
| ChatGPT: request not allowed for this sign-in | `subscription_sharing_route_not_supported` |
| ChatGPT: sign in again | a revoked, expired or reused refresh token, or a 401 |
| OpenAI refused the sign-in code (<code>: <description>) | the token endpoint's own error |

Every step is one `providers: ChatGPT sign-in: …` line in `heimdall.log`: started, the browser's return, the token
endpoint's status and error code, the answer's field names, the plan, the models listed and the test request's status.
Never a code, token or verifier.

## Claude subscriptions

A Claude Pro or Max plan is not signed into the way ChatGPT is. Anthropic's terms
(code.claude.com/docs/en/legal-and-compliance) reserve subscription OAuth for Claude Code and Anthropic's own apps, so
Heimdall does not use Pi's Anthropic OAuth flow. Two other routes exist:

- **Claude Code subscription**: the agent runs as a Claude Code thread through T3 Code, on the user's own Claude Code
  login ([claude-relay.md](claude-relay.md)).
- **Anthropic API…**: an API key ([models.md](models.md)).

## Tests

- `cargo test -p agent chatgpt`: the unit tests in `chatgpt.rs` (the exchange, the issued client id, the browser
  tab's words, the test model, plan errors, token rotation, a busy port).
- Offline lab: `crates/agent/tests/chatgpt_lab.rs` runs the real pinned Pi against a fake api.openai.com with a
  ChatGPT-style token from the per-request command. It checks `Authorization: Bearer`, `store: false`,
  `stream: true`, no rejected fields and no system items, and that a 429
  `subscription_sharing_usage_limit_exceeded` reads "ChatGPT: usage limit reached".

  The lab needs a verified agent runtime (`HEIMDALL_AGENT_RUNTIME`, else the build cache's current one).

  ```sh
  HEIMDALL_CHATGPT_LAB=$PWD/target/chatgpt-lab cargo nextest run -p agent --test chatgpt_lab --run-ignored only -E 'not test(live)'
  ```

- Live (opt-in, one tiny request on the user's plan after a browser consent; `HEIMDALL_CHATGPT_OPEN=1` opens the
  browser, otherwise it prints the link):

  ```sh
  HEIMDALL_CHATGPT_LIVE=1 HEIMDALL_CHATGPT_OPEN=1 HEIMDALL_CHATGPT_LAB=$PWD/target/chatgpt-lab \
    cargo nextest run -p agent --test chatgpt_lab --run-ignored only -E 'test(live)'
  ```

## See also

- [models.md](models.md): providers, model windows and budgets
- [setup.md](setup.md): choosing the agent in Setup
- [claude-relay.md](claude-relay.md): the Claude Code subscription route
- [connectors.md](connectors.md): connecting accounts and MCP servers
