// API provider throttles are admission failures: retry this HTTP request, never replay
// the agent/tool turn. Other responses pass through; key, size and usage-limit failures
// are also reported in words for the menu and the agent window.
import { setTimeout as sleep } from 'node:timers/promises';
import { writeSync } from 'node:fs';
import { thinkingOff } from './thinking-policy.mjs';

export const PROVIDERS = [
  { name: 'OpenCode Go', keyEnv: 'HEIMDALL_OPENCODE_API_KEY', tokenEnv: 'HEIMDALL_OPENCODE_TOKEN', match: /^https:\/\/opencode\.ai\/(?:zen|inference)\/go\//,
    rejected: { 401: 'access rejected / no subscription', 403: 'access rejected / no subscription' }, throttle: 'usage limit reached' },
  // Anthropic: 529 is a transient overload (retried here with backoff); a spent prepaid balance is not retried.
  { name: 'Anthropic', keyEnv: 'HEIMDALL_ANTHROPIC_API_KEY', match: /^https:\/\/api\.anthropic\.com\//,
    rejected: { 401: 'key rejected', 403: 'key not allowed to use this model' }, throttle: 'rate limit reached', overload: true, credits: true },
];
// 529 Overloaded: resend after the stated delay, else 1 s, 2 s, 4 s; then Pi's own bounded retry takes over.
export const OVERLOAD_DELAYS_MS = [1000, 2000, 4000];
/** Error text that means the account's prepaid credits are spent (Anthropic: 400 "credit balance is too low", 402). */
export function creditsExhausted(text) {
  const t = String(text).toLowerCase();
  return t.includes('credit balance') || t.includes('billing_error') || (t.includes('credit') && /exhaust|used up|insufficient/.test(t));
}
// Longer resets (an exhausted 5-hour or weekly allowance) fail the turn with the reset time.
export const MAX_WAIT_MS = 15 * 60 * 1000;
// The response must start within this; a long stream after that is never cut off.
export const HEADERS_MS = 120000;
// A prompt near the context window takes the provider longer to read before the first byte (26 Oct: 183k tokens
// answered in 12-13 s). HEADERS_MS covers prompts up to HEADERS_TOKENS with about ten times that margin; each larger
// request gets one more second per PREFILL_TOKENS_PER_S estimated tokens (4 characters a token), about a quarter of
// the measured rate, so a 1M-token prompt waits 320 s for its first byte instead of failing at 120 s.
export const HEADERS_TOKENS = 200000;
export const PREFILL_TOKENS_PER_S = 4000;
/** How long this request may wait for its response to start: `base`, plus time for a prompt above HEADERS_TOKENS. */
export function headersDeadline(body, base = HEADERS_MS) {
  const tokens = typeof body === 'string' ? Math.ceil(body.length / 4) : 0;
  return base + Math.ceil(Math.max(0, tokens - HEADERS_TOKENS) / PREFILL_TOKENS_PER_S) * 1000;
}

/** Milliseconds until the provider's stated reset, or null when it states none. */
export function retryDelay(headers, now = Date.now()) {
  const after = headers.get('retry-after');
  if (after !== null) {
    const seconds = Number(after);
    if (Number.isFinite(seconds) && seconds >= 0) return Math.ceil(seconds * 1000);
    const date = Date.parse(after);
    if (Number.isFinite(date)) return Math.max(0, date - now);
  }
  const ms = Number(headers.get('retry-after-ms'));
  if (ms > 0) return Math.ceil(ms);
  const reset = Number(headers.get('x-ratelimit-reset'));
  if (reset > 0) return reset > 1e9 ? Math.max(0, reset * 1000 - now) : Math.ceil(reset * 1000);
  return null;
}

// Claude binds each thinking block to the history before it. Compaction (and any other rewrite of earlier
// messages) invalidates every kept block, and the provider then rejects every request with a 400 until the
// blocks are gone (18 Oct: two failed turns after a compaction). Ask it to drop such blocks; if a request is
// still rejected for a thinking signature, resend it once without thinking blocks. Both are admission
// failures: nothing was generated and no tool ran, so resending is safe.
export const THINKING_BINDING_BETA = 'thinking-binding-controls-2026-08-01';
const SIGNATURE_REJECTED = /Invalid `signature` in `thinking` block/;

function withThinkingBinding(init) {
  if (typeof init?.body !== 'string') return init;
  let body;
  try { body = JSON.parse(init.body); } catch { return init; }
  const type = body?.thinking?.type;
  if (type !== 'adaptive' && type !== 'enabled') return init;
  body.thinking = { ...body.thinking, block_binding: { prefix_mismatch_behavior: 'drop_block' } };
  const headers = new Headers(init.headers);
  const betas = (headers.get('anthropic-beta') ?? '').split(',').map(b => b.trim()).filter(Boolean);
  if (!betas.includes(THINKING_BINDING_BETA)) betas.push(THINKING_BINDING_BETA);
  headers.set('anthropic-beta', betas.join(','));
  return { ...init, body: JSON.stringify(body), headers };
}

// Thinking Off in Anthropic's form for this model (thinking-policy.mjs), on every Messages request: turns and
// compaction summaries alike.
const MESSAGES = /^https:\/\/api\.anthropic\.com\/v1\/messages(?:\?|$)/;
function withThinkingOff(url, init, mode) {
  if (!mode || !MESSAGES.test(url) || typeof init?.body !== 'string') return init;
  let body;
  try { body = JSON.parse(init.body); } catch { return init; }
  const off = thinkingOff(body, mode);
  return off === body ? init : { ...init, body: JSON.stringify(off) };
}

function withoutThinking(init) {
  const body = JSON.parse(init.body);
  body.messages = body.messages.map(m => m.role === 'assistant' && Array.isArray(m.content)
    ? { ...m, content: m.content.filter(b => b.type !== 'thinking' && b.type !== 'redacted_thinking') }
    : m);
  return { ...init, body: JSON.stringify(body) };
}

// A rejected key or account (401/403) is final. Pi 1.4.0 retries any failure whose text says "upstream request failed",
// which is how OpenCode Go words every rejection ("Upstream request failed: Invalid credential"), so that phrase
// becomes Heimdall's words for the rejection and Pi fails the turn once.
const UPSTREAM_FAILED = /upstream.?request.?failed:?\s*/i;
export async function finalRejection(response, words) {
  const text = await response.clone().text().catch(() => '');
  if (!UPSTREAM_FAILED.test(text)) return response;
  const headers = new Headers(response.headers);
  headers.delete('content-length');
  headers.delete('content-encoding');
  await response.body?.cancel();
  return new Response(text.replace(new RegExp(UPSTREAM_FAILED.source, 'gi'), `${words}: `), { status: response.status, statusText: response.statusText, headers });
}

function clock(ms) {
  return new Date(ms).toLocaleTimeString('en-GB', { hour: '2-digit', minute: '2-digit' });
}

/** `offMode`: Heimdall's Off form for this session's Claude model ('between_tools' | 'disabled'), if any. */
export function providerFetch(fetch, emit, wait = sleep, now = Date.now, headersMs = HEADERS_MS, offMode = undefined) {
  return async function(input, init) {
    const url = typeof input === 'string' || input instanceof URL ? String(input) : input.url;
    const provider = PROVIDERS.find(p => p.match.test(url));
    const method = String(init?.method ?? input?.method ?? 'GET').toUpperCase();
    if (!provider || method !== 'POST') return fetch(input, init);
    const signal = init?.signal ?? input?.signal;
    init = withThinkingBinding(withThinkingOff(url, init, offMode));
    let waited = false;
    let thinkingDropped = false;
    let overloads = 0;
    for (;;) {
      signal?.throwIfAborted();
      const attempt = new AbortController();
      const forward = () => attempt.abort(signal.reason);
      signal?.addEventListener('abort', forward, { once: true });
      const limit = headersDeadline(init?.body, headersMs);
      const timer = setTimeout(() => attempt.abort(new Error(`${provider.name} did not answer within ${limit / 1000} seconds`)), limit);
      let response;
      try { response = await fetch(input, { ...init, signal: attempt.signal }); }
      catch (error) { signal?.removeEventListener('abort', forward); throw error; }
      finally { clearTimeout(timer); }
      const status = response.status;
      if (provider.credits && [400, 402, 429].includes(status) && creditsExhausted(await response.clone().text().catch(() => ''))) {
        emit({ type: 'heimdall_provider_problem', message: `${provider.name} credits used up` });
        return response;
      }
      if (provider.overload && status === 529 && overloads < OVERLOAD_DELAYS_MS.length) {
        const delay = Math.min(retryDelay(response.headers, now()) ?? OVERLOAD_DELAYS_MS[overloads], MAX_WAIT_MS);
        overloads++;
        signal?.removeEventListener('abort', forward);
        await response.body?.cancel();
        emit({ type: 'heimdall_provider_wait', provider: provider.name, delay_ms: delay, until_ms: now() + delay });
        waited = true;
        await wait(delay, undefined, { signal });
        continue;
      }
      if (provider.overload && status === 529) emit({ type: 'heimdall_provider_problem', message: `${provider.name} overloaded · try again shortly` });
      if (provider.rejected[status]) {
        emit({ type: 'heimdall_provider_problem', message: `${provider.name}: ${provider.rejected[status]}` });
        if (waited) emit({ type: 'heimdall_provider_wait', provider: provider.name, delay_ms: 0 });
        return finalRejection(response, provider.rejected[status]);
      }
      if (status === 404) emit({ type: 'heimdall_provider_problem', message: `${provider.name}: endpoint not found for this model` });
      if (status === 413) emit({ type: 'heimdall_provider_problem', message: `${provider.name}: request too large for this model` });
      if (status === 400 && !thinkingDropped && typeof init?.body === 'string' && init.body.includes('"thinking"')) {
        const text = await response.clone().text().catch(() => '');
        if (SIGNATURE_REJECTED.test(text)) {
          thinkingDropped = true;
          signal?.removeEventListener('abort', forward);
          await response.body?.cancel();
          init = withoutThinking(init);
          emit({ type: 'heimdall_thinking_dropped', provider: provider.name });
          continue;
        }
      }
      if (status !== 429) {
        if (waited) emit({ type: 'heimdall_provider_wait', provider: provider.name, delay_ms: 0 });
        return response;
      }
      const delay = retryDelay(response.headers, now());
      if (delay === null || delay > MAX_WAIT_MS) {
        const message = delay === null
          ? `${provider.name}: ${provider.throttle}`
          : `${provider.name}: ${provider.throttle} · resets ${clock(now() + delay)}`;
        emit({ type: 'heimdall_provider_problem', message });
        // Pi's own bounded retry (two attempts) sees the 429; nothing here loops.
        return response;
      }
      signal?.removeEventListener('abort', forward);
      await response.body?.cancel();
      emit({ type: 'heimdall_provider_wait', provider: provider.name, delay_ms: delay, until_ms: now() + delay });
      waited = true;
      await wait(delay, undefined, { signal });
    }
  };
}

export function installProviderRetry(offMode) {
  // Installed in every API session: a Console sign-in passes no secret through the environment.
  if (!process.env.HEIMDALL_API_PROVIDER) return;
  // Pi redirects process.stdout during extension calls; status frames go to the RPC descriptor.
  globalThis.fetch = providerFetch(globalThis.fetch, event => writeSync(1, JSON.stringify(event) + '\n'), sleep, Date.now, HEADERS_MS, offMode);
}
