# ChatGPT subscription (Sign in with ChatGPT)

Agent ▸ Model ▸ Add Provider ▸ Subscription ▸ **ChatGPT…** connects the user's ChatGPT plan to the agent. Code:
`crates/agent/src/chatgpt.rs` (provider id `openai`). Decided 23 Oct 2026 from OpenAI's and Anthropic's own pages
(links below), read that day.

Beta: some requests fail with HTTP 400 because the provider rejects the prompt-cache breakpoint Heimdall sends. Model windows, budgets and thinking levels for these models are in `docs/models.md`.

## The route OpenAI documents

OpenAI documents one way for an open-source or locally hosted app to run on a user's ChatGPT plan: **Sign in with
ChatGPT, token sharing** (developers.openai.com/siwc/token-sharing-open-source). The app registers itself in the
user's browser, the user consents to "ChatGPT plan usage", and the app calls the public Responses API
(`https://api.openai.com/v1/responses`) with the access token. OpenAI's models page says "do not point it at ChatGPT's
`backend-api` endpoints".

So Heimdall does **not**:

- read the Codex CLI's login (`~/.codex/auth.json`). That token was issued to OpenAI's own Codex client, has no
  `chatgpt.tokens.use.direct` scope, and refreshing it from Heimdall would rotate the CLI's refresh token from under it;
- use Pi's `openai-codex` provider (Pi labels it "OpenAI Codex (legacy)"), which sends the Codex CLI's client id to
  `chatgpt.com/backend-api`.

Pi's built-in `openai` provider already sends the documented request shape for a non-`sk-` token (`store: false`,
`stream: true`, none of the rejected fields). Heimdall owns only the sign-in, the Keychain session and the per-request
token command (`heimdall --provider-token openai`).

## How it behaves

1. Click **ChatGPT…**. The browser opens OpenAI's consent page and a small panel says to continue there (Cancel stops
   it). There is no code to type and no key to paste. The first sign-in registers Heimdall
   (`client_id=dynamic_agent_client`, an opaque `ext_agent_host_id` kept in `<data>/agent/chatgpt-agent-host`).
2. OpenAI redirects to `http://127.0.0.1:1455/auth/callback`. Heimdall checks the state, keeps the issued client id,
   exchanges the code (PKCE) and refuses a grant without `chatgpt.tokens.use.direct`. The browser tab waits for the
   exchange, then says "ChatGPT is signed in to Heimdall" or "ChatGPT was not connected: <why>". The session (one-hour
   access token, rotating refresh token, client id) goes to the Keychain (`Heimdall API provider: ChatGPT`); the app
   keeps the token fresh 10 minutes ahead, and Pi's token command refreshes under the shared lock otherwise.
3. Every sign-in makes one tiny test request (`chatgpt::probe`) on `chatgpt::test_model`: GPT-6 Luna when listed, else
   the smallest listed (Luna, mini, nano), else GPT-6.1 Sol. Signing in never switches the agent. A plan that cannot share its usage is named on
   the provider row: "ChatGPT: your Free plan can't share its usage with apps · OpenAI allows Plus and Pro" (the plan is
   named when the ID token says it).
4. The models offered are the ones the account lists at `/v1/models` (`visibility: list`). Their figures and refresh
   rules are in `docs/models.md`.
5. Errors are named (`chatgpt::explain`): the token endpoint's own code and description ("OpenAI refused the sign-in
   code (invalid_grant: …)"), usage limit reached (with ChatGPT ▸ Settings ▸ Usage), plan not eligible, usage check
   unavailable, sign in again (revoked, expired or reused refresh token, or a 401).
6. Every step is one `providers: ChatGPT sign-in: …` line in `heimdall.log`: started, the browser's return, the token
   endpoint's status and error code, the answer's field names, the plan, the models listed, and the test request's
   status. Never a code, token or verifier.

Port 1455 is also used by `codex login` while it runs. If both are open at once, Heimdall says the port is busy.

The answer's scope and expiry are read in every shape OpenAI documents, with the access token's own claims as a
fallback. Reason: on 24 Oct OpenAI granted the scope, but Heimdall showed "connected" before the exchange, the exchange
or its parsing failed, and nothing was logged; the tab now waits and every step is logged.

## Tests

- Offline: `tests/chatgpt_lab.rs` runs the real pinned Pi against a fake api.openai.com with a ChatGPT-style token from
  the per-request command: `Authorization: Bearer`, `store: false`, `stream: true`, no rejected fields, no system items;
  a 429 `subscription_sharing_usage_limit_exceeded` reads "ChatGPT: usage limit reached".
- Live (opt-in, one tiny request on your own plan):

```
HEIMDALL_CHATGPT_LIVE=1 HEIMDALL_CHATGPT_OPEN=1 HEIMDALL_CHATGPT_LAB=$PWD/target/chatgpt-lab \
  HEIMDALL_AGENT_RUNTIME=<a verified runtime> cargo nextest run -p agent --test chatgpt_lab --run-ignored only -E 'test(live)'
```


## Claude plans

Anthropic is connected directly only with an API key. A Claude Pro or Max plan is used through T3 Code instead
("Claude Code subscription", docs/claude-relay.md), where Claude Code itself holds the login. Heimdall can't sign in to
the plan itself: Anthropic's legal and compliance page
(code.claude.com/docs/en/legal-and-compliance) says subscription OAuth is "intended exclusively" for Claude Code and
Anthropic's own apps, that developers "should use API key authentication", and that third parties may not "offer
Claude.ai login" or "route requests through Free, Pro, or Max plan credentials". Pi has an Anthropic OAuth flow, and
Heimdall deliberately does not use it.
